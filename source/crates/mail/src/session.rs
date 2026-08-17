/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Duration;

use async_imap::imap_proto::types::{MailboxDatum, MessageSection, NameAttribute, Response, SectionPath, Status, StatusAttribute};
use async_imap::types::Fetch;
use async_imap::Session;
use futures::TryStreamExt;
use lookout_core::mailbox::role_from_special_use;
use lookout_core::{AccountId, BodyPart, EmailBody, EmailSummary, Mailbox, MailboxId, MailboxRole, SystemFlagBit, Uid, UidValidity};

use crate::auth::XOAuth2Authenticator;
use crate::body::{parse_body, preview_from_raw};
use crate::config::{AccountConfig, Credential};
use crate::connection::{connect_tls, ImapStream};
use crate::envelope::{flags_from_fetch, summary_from_fetch};
use crate::error::{Error, Result};
use crate::send::{build_raw_message, send_smtp, ComposedMessage};

/// How many of the most recent messages the background body prefetch queues
/// for download on a folder it hasn't warmed yet. Bounds the *body* warm-up
/// only - the display/envelope sync fetches the whole folder (see
/// `sync_mailbox`) - so a folder's newest N get bodies downloaded in batches
/// while anything older fetches on demand. Since the prefetch learns each
/// message's `BODYSTRUCTURE` in its envelope pass, "download" here means the
/// text parts only - attachments are never fetched for the cache. On
/// CONDSTORE connections the steady-state sync is itself a `CHANGEDSINCE`
/// delta (see `sync_mailbox`); with QRESYNC enabled the wake's `SELECT
/// (QRESYNC ...)` additionally reports expunged UIDs via `VANISHED`,
/// replacing the delta path's `UID SEARCH ALL` membership pass.
const INITIAL_FETCH_LIMIT: u32 = 200;

/// How long a single IDLE wait runs before we re-enter it purely as a
/// keepalive, well under RFC 2177's ~29-minute server timeout. On-demand
/// commands don't wait for this timeout: the IDLE wait future is raced
/// against `commands.recv()` directly (see `connect_and_run`'s main loop),
/// and dropping the `Handle`'s `StopSource` cancels the wait immediately,
/// so a command arriving mid-IDLE is picked up right away.
const IDLE_SLICE: Duration = Duration::from_secs(25 * 60);

/// How many folder-count STATUS calls the drain runs between `FoldersUpdated`
/// emits. The drain itself yields to the command queue before *every* round
/// trip - this only bounds how often the sidebar repaints while the counts
/// fill in, so a hundred-folder account doesn't emit a hundred folder lists.
const COUNT_STATUS_BATCH: usize = 5;

/// How many previewless messages one sync will fetch snippets for. The list
/// only ever shows a screenful at a time, and `sync_mailbox` re-runs on
/// every IDLE wake - so this bounds the *first* sync of a mailbox (one extra
/// round trip, ~50 x 4 KB) while steady-state resyncs, where almost every
/// uid already has a cached preview, fetch only the handful that are new.
/// Anything past this keeps `preview: None` and renders a blank snippet line
/// until a later sync reaches it.
const PREVIEW_FETCH_LIMIT: usize = 50;

/// How much of each message body to pull for its preview. `BODY.PEEK[]<0.N>`
/// asks for a byte prefix, which needs no BODYSTRUCTURE round trip and no
/// part-number guessing - `preview_from_raw` is built to tolerate the
/// truncated MIME that comes back.
///
/// Generous because the *raw* prefix is what's bounded, not the readable
/// text in it: a marketing HTML mail can spend its first several KB on
/// headers and a `<style>` block, and quoted-printable/base64 inflates
/// whatever text follows. Too small a window reliably yields an empty
/// snippet for exactly the bulk mail whose subject lines say least. At the
/// `PREVIEW_FETCH_LIMIT` above this is ~800 KB on a mailbox's first sync
/// only - steady-state resyncs fetch just the newly-arrived uids.
const PREVIEW_FETCH_BYTES: u32 = 16384;

/// How many full message bodies to fetch per IDLE cycle during background
/// prefetch. Small enough to keep each batch fast (a few seconds), large
/// enough to make progress across hundreds of messages.
const PREFETCH_BATCH_SIZE: usize = 10;

/// How many bytes of sequence-set syntax one `UID`-prefixed command line may
/// carry before it is split into further commands. Servers cap command-line
/// length - Dovecot's default `imap_max_line_length` is 64 KiB, and a line
/// beyond it tears the connection down, which is exactly the failure this
/// chunking exists to prevent. 40 KiB leaves room for the command prefix
/// (`UID FETCH ... (query)`) and for servers with tighter limits, while a
/// dense 7-digit UID space still packs ~5k uids per chunk.
const UID_SET_BYTE_BUDGET: usize = 40 * 1024;

/// Tracks progress of the background body prefetch across all mailboxes.
struct PrefetchState {
    /// Mailbox IDs to prefetch, in processing order. INBOX is typically first.
    mailboxes: Vec<MailboxId>,
    /// Index into `mailboxes` — which mailbox we're currently prefetching.
    current: usize,
    /// UIDs remaining to fetch in the current mailbox, newest first.
    pending_uids: Vec<Uid>,
    /// The uidvalidity of the current mailbox.
    uidvalidity: UidValidity,
    /// Whether we've fetched envelope UIDs for the current mailbox.
    envelopes_fetched: bool,
    /// The IMAP folder path of the current prefetch mailbox (for SELECT).
    current_folder_name: String,
    /// Per-uid `BODYSTRUCTURE`-derived part lists, learned in the envelope
    /// pass so the body fetches can be text-parts-only (see
    /// `fetch_body_partial`) instead of whole-message downloads.
    structures: HashMap<Uid, Vec<BodyPart>>,
}

impl PrefetchState {
    fn new(mailboxes: Vec<MailboxId>) -> Self {
        Self {
            mailboxes,
            current: 0,
            pending_uids: Vec::new(),
            uidvalidity: UidValidity(0),
            envelopes_fetched: false,
            current_folder_name: String::new(),
            structures: HashMap::new(),
        }
    }

    fn is_done(&self) -> bool {
        self.current >= self.mailboxes.len()
    }

    fn advance(&mut self) {
        self.current += 1;
        self.pending_uids.clear();
        self.uidvalidity = UidValidity(0);
        self.envelopes_fetched = false;
        self.current_folder_name.clear();
        self.structures.clear();
    }
}

#[derive(Debug, Clone)]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Idle,
    Busy,
    Error { message: String, retryable: bool },
}

#[derive(Debug)]
pub enum AccountCommand {
    /// Select a mailbox and (re)fetch its envelopes.
    SyncMailbox(MailboxId),
    /// Fetch a message's body - the viewer's renderable text, not
    /// attachments. With a `BODYSTRUCTURE`-derived part structure in the
    /// message's summary this SELECTs `mailbox` on demand and downloads only
    /// the headers and text parts (`BODY.PEEK[HEADER]` + `BODY.PEEK[<part>]`);
    /// without one (server omitted it, or the summary predates this feature)
    /// it falls back to a whole-message `BODY.PEEK[]` fetch.
    FetchBody {
        mailbox: MailboxId,
        uid: Uid,
    },
    /// Fetch one attachment part's *decoded* bytes on demand - the other half
    /// of `FetchBody`, which deliberately never downloads attachments. `part`
    /// is the `BodyPart` (part number + transfer encoding) the viewer's
    /// reading pane already knows from the message's `BODYSTRUCTURE`; the
    /// session SELECTs `mailbox` on demand (same contract as `StoreFlags`),
    /// fetches `BODY.PEEK[<part.part_number>]`, decodes the wire bytes, stores
    /// them in the flat-file attachment cache, and answers with
    /// `AccountEvent::PartFetched`. A previously-fetched part is served from
    /// the cache with no network round trip.
    FetchAttachment {
        mailbox: MailboxId,
        uid: Uid,
        part: BodyPart,
    },
    /// Fetch a message's whole raw RFC 5322 bytes (`BODY.PEEK[]`, so `\Seen`
    /// is never set) for the .eml export/cache. Same contract as
    /// `FetchAttachment`: the session SELECTs `mailbox` on demand, serves a
    /// previously-fetched copy from the flat-file raw-message cache with no
    /// network round trip, stores freshly-fetched bytes back into it, and
    /// answers with `AccountEvent::RawMessageFetched` (or
    /// `RawMessageFetchFailed`).
    FetchRawMessage {
        mailbox: MailboxId,
        uid: Uid,
    },
    /// Force a folder-list + current-mailbox resync outside of IDLE's own cadence.
    Refresh,
    /// Send a composed message over SMTP, then `APPEND` it to the account's
    /// Sent mailbox (two explicit steps - IMAP has no JMAP-style implicit
    /// filing on submit). If no Sent mailbox can be identified, the message
    /// is still sent; only the archival copy is skipped (logged as a warning).
    /// When the APPEND does land, the handler also resyncs the Sent mailbox
    /// so the new message shows up immediately instead of waiting behind the
    /// cache-hit shortcut in `SyncMailbox` below.
    /// Boxed: the message can be large (an iMIP reply carries a whole
    /// iCalendar document), and commands are passed one at a time through an
    /// mpsc channel - the heap indirection keeps the command enum compact.
    SendMessage(Box<ComposedMessage>),
    /// A hint that it's worth retrying the connection now rather than
    /// waiting out the current backoff delay - e.g. the app crate's
    /// `Gio.NetworkMonitor` reporting connectivity just came back. A no-op
    /// if the session is already connected (nothing to reconnect).
    Reconnect,
    /// Moves a message from its current mailbox into the account's mailbox
    /// with the given special-use role (Trash for Delete, Archive for
    /// Archive, Junk for Report-as-junk) - via IMAP MOVE (RFC 6851) if the
    /// server advertises it, else COPY + STORE `\Deleted` + EXPUNGE. If no
    /// mailbox with that role exists, this fails with an `Error` event
    /// rather than silently permanent-deleting - there's no
    /// confirmation-dialog UI in this app yet, so no destructive fallback
    /// without one.
    MoveMessage {
        mailbox: MailboxId,
        uid: Uid,
        role: MailboxRole,
    },
    /// `MoveMessage` for a whole batch of messages in one mailbox at once -
    /// one `MOVE`/`COPY`+`STORE`+`EXPUNGE` over a joined UID set instead of
    /// one round trip per message, and one resync at the end instead of one
    /// per message. Cross-mailbox batches are the caller's job: send one of
    /// these per `(account, mailbox)` group.
    MoveMessages {
        mailbox: MailboxId,
        uids: Vec<Uid>,
        role: MailboxRole,
    },
    /// `MoveMessages` to an explicit target mailbox rather than by
    /// special-use role - the drag-to-folder path, where the drop target can
    /// be any folder (not just Trash/Archive/Junk). `target` is the
    /// destination `MailboxId`; same MOVE/COPY+EXPUNGE semantics and the
    /// same one-resync-on-success contract as `MoveMessages`.
    MoveMessagesTo {
        mailbox: MailboxId,
        uids: Vec<Uid>,
        target: MailboxId,
    },
    /// Permanently deletes every message in `mailbox` - the "empty Trash /
    /// Junk" quick action, so the UI only offers it for those roles. SELECTs
    /// the mailbox on demand, then `UID SEARCH ALL` + STORE `\Deleted` +
    /// EXPUNGE (same mechanics as `purge_by_message_id`, and the whole-mailbox
    /// `\Deleted` caveat of `move_uids_to_path` is exactly the intent here).
    /// Irreversible: no move-to-Trash fallback for this one.
    EmptyMailbox {
        mailbox: MailboxId,
    },
    /// Client-side only - IMAP has no native snooze. Records `until` in the
    /// local cache and hides the message from `MessagesUpdated` until that
    /// time passes.
    SnoozeMessage {
        mailbox: MailboxId,
        uid: Uid,
        until: chrono::DateTime<chrono::Utc>,
    },
    /// `SnoozeMessage` for a whole batch of messages in one mailbox at once.
    SnoozeMessages {
        mailbox: MailboxId,
        uids: Vec<Uid>,
        until: chrono::DateTime<chrono::Utc>,
    },
    /// Adds and/or removes IMAP system flags on one message (`STORE`), for
    /// the client-driven flag changes this app makes: marking a message read
    /// when it's opened (bodies are fetched with `BODY.PEEK`, so the server
    /// never sets `\Seen` on its own) and the toolbar's Flag/Unflag toggle.
    /// Unlike `FetchBody`, this doesn't require `mailbox` to be the
    /// currently-open folder - it SELECTs the message's own folder if
    /// needed, and the main loop re-selects the user's folder before the
    /// next IDLE.
    StoreFlags {
        mailbox: MailboxId,
        uid: Uid,
        add: Vec<SystemFlagBit>,
        remove: Vec<SystemFlagBit>,
    },
    /// `StoreFlags` for a whole batch of messages in one mailbox at once -
    /// one `STORE` per add/remove side over a joined UID set, and the cache
    /// patched (or a single resync issued) for the whole batch rather than
    /// once per message. Backs the toolbar's batch Flag/Unflag and Mark
    /// read/unread actions.
    StoreFlagsMany {
        mailbox: MailboxId,
        uids: Vec<Uid>,
        add: Vec<SystemFlagBit>,
        remove: Vec<SystemFlagBit>,
    },
    /// Adds and/or removes custom IMAP keywords (raw flag atoms, e.g.
    /// `$Lookout-tag-<key>` - see `lookout_core::tag_keyword`) on one
    /// message: the server side of color tags. Same `STORE .SILENT`
    /// mechanics and the same folder-handling contract as `StoreFlags` (it
    /// SELECTs the message's own folder when it isn't the open one, and the
    /// main loop re-selects the user's folder before the next IDLE).
    StoreKeywords {
        mailbox: MailboxId,
        uid: Uid,
        add: Vec<String>,
        remove: Vec<String>,
    },
    /// `StoreKeywords` for a whole batch of messages in one mailbox at once -
    /// one `STORE` per add/remove side over a joined UID set, and the cache
    /// patched (or a single resync issued) for the whole batch. Backs the
    /// drag-messages-onto-a-tag action.
    StoreKeywordsMany {
        mailbox: MailboxId,
        uids: Vec<Uid>,
        add: Vec<String>,
        remove: Vec<String>,
    },
    /// Kick off background body prefetch for all mailboxes. The prefetch
    /// runs cooperatively in batches between IDLE cycles, fetching full
    /// message bodies and caching them on disk so subsequent message views
    /// are instant. Triggered automatically after the initial sync.
    PrefetchBodies,
    /// Best-effort draft autosave: build `msg` into a raw RFC 5322 message
    /// and `APPEND` it to the account's Drafts mailbox with `\Draft \Seen`
    /// flags. `msg.message_id` carries a stable per-compose-session id; with
    /// `replace` set, any draft already stored under that `Message-ID` is
    /// deleted first, so repeated autosaves never accumulate duplicates. If
    /// the account has no Drafts mailbox yet, one is `CREATE`d. Failures are
    /// warning-level and silent in the UI (a draft is housekeeping, not a
    /// user action - same convention as the Sent `APPEND` after sending).
    SaveDraft {
        msg: Box<ComposedMessage>,
        replace: bool,
    },
    /// The IMAP `SEARCH` fallback for full-text search: run `UID SEARCH TEXT
    /// "<query>"` against `mailbox` (SELECTing it on demand, like
    /// `StoreFlags`), fetch the matching messages' envelopes, and answer with
    /// an `AccountEvent::SearchResults` - always emitted, even for an empty
    /// match set, so the UI knows the live pass is complete. One round trip
    /// per folder, which is why the app only sends it for the mailbox the
    /// user is actually viewing; everything already synced is covered
    /// instantly by the local FTS index (`Cache::search`).
    SearchMailbox {
        mailbox: MailboxId,
        query: String,
    },
    /// Best-effort counterpart of `SaveDraft`: permanently remove the draft
    /// stored under `message_id` from the Drafts mailbox - sent right before
    /// `SendMessage` when the message being sent was draft-autosaved, so the
    /// sent mail doesn't linger in Drafts too.
    DeleteDraft {
        message_id: String,
    },
    Shutdown,
}

#[derive(Debug)]
pub enum AccountEvent {
    ConnectionStateChanged(ConnectionState),
    FoldersUpdated(Vec<Mailbox>),
    MessagesUpdated {
        mailbox: MailboxId,
        messages: Vec<EmailSummary>,
    },
    /// A subset of a `MessagesUpdated` sync: just the messages that are both
    /// genuinely new (their UID wasn't cached before this sync) and still
    /// unread, for a desktop "new mail" notification. Never emitted on a
    /// mailbox's first-ever sync (nothing cached yet to compare against) -
    /// see `new_unread_for_notification` - so connecting an account doesn't
    /// notify once per message already sitting in the inbox.
    NewMessages {
        mailbox: MailboxId,
        messages: Vec<EmailSummary>,
    },
    BodyFetched {
        mailbox: MailboxId,
        uid: Uid,
        body: EmailBody,
    },
    /// The answer to an `AccountCommand::FetchAttachment`: `bytes` are the
    /// part's transfer-decoded content bytes, `part` echoes the request (so
    /// the UI can match it to the strip row it came from and name the saved
    /// file from `BodyPart::filename`). Emitted on both the cache-hit and the
    /// live-fetch path, mirroring `BodyFetched`.
    PartFetched {
        mailbox: MailboxId,
        uid: Uid,
        part: BodyPart,
        bytes: Vec<u8>,
    },
    /// The answer to an `AccountCommand::FetchAttachment` when the part's
    /// bytes couldn't be produced: the server didn't return the section, or
    /// the fetch failed. Unlike a connection failure (which is the session's
    /// problem to recover from), this is a per-request outcome - the UI uses
    /// it to restore the Save button and tell the user, instead of leaving it
    /// stuck on "Fetching…" forever.
    PartFetchFailed {
        mailbox: MailboxId,
        uid: Uid,
        part_number: String,
        message: String,
    },
    /// The answer to an `AccountCommand::FetchRawMessage`: `bytes` are the
    /// whole raw RFC 5322 message exactly as `BODY.PEEK[]` returned it - a
    /// valid `.eml` file, unmodified. Emitted on both the cache-hit and the
    /// live-fetch path, mirroring `PartFetched`.
    RawMessageFetched {
        mailbox: MailboxId,
        uid: Uid,
        bytes: Vec<u8>,
    },
    /// The answer to an `AccountCommand::FetchRawMessage` when the raw message
    /// couldn't be produced: the server didn't return it (it may have been
    /// expunged) or the fetch failed. Mirrors `PartFetchFailed`, so the UI
    /// restores the export action and tells the user rather than leaving it
    /// stuck on "Exporting…".
    RawMessageFetchFailed {
        mailbox: MailboxId,
        uid: Uid,
        message: String,
    },
    SendCompleted,
    /// A `SendMessage` request failed. Kept distinct from the generic
    /// `Error` below so the UI can retract the "Sending…" toast it showed
    /// for this specific send without also swallowing unrelated errors
    /// (sync, connection, etc.) that might land while a send is outstanding.
    SendFailed(String),
    /// A `MoveMessage`/`MoveMessages`/`MoveMessagesTo` request failed. Kept
    /// distinct from `Error` below, mirroring `SendFailed`, so the UI can
    /// restore exactly the rows it optimistically removed from the message
    /// list on send, without also swallowing unrelated errors.
    MoveFailed {
        mailbox: MailboxId,
        uids: Vec<Uid>,
        role: MailboxRole,
        message: String,
    },
    /// A `StoreFlags`/`StoreFlagsMany` request failed. Kept distinct from
    /// `Error` below, mirroring `MoveFailed`, so the UI can restore exactly
    /// the summaries it optimistically flag-patched (if any - harmless
    /// no-op otherwise), without also swallowing unrelated errors.
    StoreFlagsFailed {
        mailbox: MailboxId,
        uids: Vec<Uid>,
        message: String,
    },
    /// A `SaveDraft` request landed server-side; `message_id` is the draft's
    /// stable `Message-ID`, so only the compose session that owns that id
    /// acts on it.
    DraftSaved {
        message_id: String,
    },
    MessageMoved {
        role: MailboxRole,
    },
    MessageSnoozed,
    /// The answer to an `AccountCommand::EmptyMailbox`: every message is gone
    /// from the server. `role` is the emptied mailbox's role (Trash/Junk),
    /// so the UI can name the confirmation toast.
    MailboxExpunged {
        role: MailboxRole,
    },
    /// The answer to an `AccountCommand::SearchMailbox`: the envelopes of
    /// every message in `mailbox` whose headers/body matched `query` per the
    /// server's `UID SEARCH`. Emitted even when `messages` is empty, so the
    /// UI can tell "searched, nothing found" apart from "still searching".
    SearchResults {
        mailbox: MailboxId,
        query: String,
        messages: Vec<EmailSummary>,
    },
    Error(String),
}

/// Fetches a fresh credential immediately before each (re)connect attempt or
/// SMTP send. `lookout-mail` never caches credentials itself; the app crate
/// implements this trait against `lookout-goa`, keeping this crate free of
/// D-Bus concerns and independently testable (see the `imap_integration` test).
#[async_trait::async_trait]
pub trait CredentialProvider: Send + Sync {
    async fn imap_credential(&self) -> std::result::Result<Credential, String>;
    async fn smtp_credential(&self) -> std::result::Result<Credential, String>;
}

/// The session's optional on-disk cache, shared with the blocking thread pool.
/// `Cache` is already `Sync` (its connection is `Mutex`-guarded - see
/// `cache.rs`), so a plain `Arc` makes it shareable: every cache touch runs on
/// the blocking pool via `cache_op`, keeping SQLite reads/writes, JSON
/// parse/serialize, and FTS index work off the session's async worker - the
/// same worker every account's session runs on, where a multi-second
/// whole-folder write would stall this account's actor and hold the worker
/// against every other account's session too.
type CacheHandle = std::sync::Arc<Option<crate::cache::Cache>>;

/// Runs `op` against the on-disk cache on the blocking thread pool, returning
/// `None` when no cache is open (or the task was cancelled) and `Some(result)`
/// otherwise. Every cache call in the session goes through here so no SQLite
/// or JSON work ever executes on the async worker; `op` must own all its
/// inputs (it is `'static`, so callers clone arguments the closure needs).
async fn cache_op<T, F>(cache: &CacheHandle, op: F) -> Option<T>
where
    T: Send + 'static,
    F: FnOnce(&crate::cache::Cache) -> T + Send + 'static,
{
    let cache = cache.clone();
    tokio::task::spawn_blocking(move || (*cache).as_ref().map(op)).await.ok().flatten()
}

/// Runs one account's IMAP connection lifecycle on the calling task (spawn
/// this onto the shared tokio worker thread - see the crate docs). Reconnects
/// with backoff on any connection error; re-fetches credentials from
/// `credentials` on every attempt rather than reusing a possibly-expired one.
pub async fn run_account_session(
    config: AccountConfig,
    credentials: std::sync::Arc<dyn CredentialProvider>,
    commands: async_channel::Receiver<AccountCommand>,
    events: async_channel::Sender<AccountEvent>,
) {
    let cache: CacheHandle = std::sync::Arc::new(match crate::cache::Cache::open(&config.account_id) {
        Ok(cache) => Some(cache),
        Err(e) => {
            tracing::warn!("couldn't open local cache, continuing without it: {e}");
            None
        }
    });

    // Fast first paint: emit whatever's cached from the previous session
    // before the network connection even starts. This is immediately
    // superseded by live data once the connection succeeds - the cache is
    // never treated as authoritative (see `Cache`'s doc comment).
    if let Some(Ok(folders)) = cache_op(&cache, {
        let account_id = config.account_id.clone();
        move |c| c.load_mailboxes(&account_id)
    })
    .await
    {
        if !folders.is_empty() {
            let _ = events.send(AccountEvent::FoldersUpdated(folders)).await;
        }
    }
    let inbox_id = MailboxId::new(&config.account_id, "INBOX");
    if let Some(Ok(messages)) = cache_op(&cache, {
        let inbox_id = inbox_id.clone();
        move |c| c.load_messages(&inbox_id)
    })
    .await
    {
        if !messages.is_empty() {
            let _ = events.send(AccountEvent::MessagesUpdated { mailbox: inbox_id, messages }).await;
        }
    }

    // One-time FTS backfill, fired off to the blocking pool *without*
    // awaiting. It used to run inline before the connection was even
    // attempted - re-parsing every cached row and body on the async worker -
    // which delayed login on every launch. It now runs concurrently with the
    // connect attempt; the search index is incremental either way (rows are
    // indexed as they sync), and the pass is a cheap no-op once the index
    // exists. `Cache::open` deliberately doesn't do this itself (see that
    // method's note), because the app also opens a read-side handle from the
    // main thread at connect time.
    let backfill_cache = cache.clone();
    tokio::task::spawn_blocking(move || {
        if let Some(c) = backfill_cache.as_ref() {
            if let Err(e) = c.backfill_search_index() {
                tracing::warn!("failed to backfill the search index: {e}");
            }
        }
    });

    let mut backoff = Duration::from_secs(1);
    const MAX_BACKOFF: Duration = Duration::from_secs(60);

    // A command that arrived while disconnected (see below) and must not be
    // lost - handed to the next `connect_and_run` so it's the first thing
    // processed once a session exists again, exactly as if it had arrived
    // right after connecting.
    let mut carried_command: Option<AccountCommand> = None;

    loop {
        let _ = events.send(AccountEvent::ConnectionStateChanged(ConnectionState::Connecting)).await;
        match connect_and_run(&config, credentials.as_ref(), &commands, &events, &cache, carried_command.take()).await {
            Ok(ShutdownReason::Requested) => {
                let _ = events.send(AccountEvent::ConnectionStateChanged(ConnectionState::Disconnected)).await;
                return;
            }
            Err(e) => {
                tracing::warn!("account session error, will reconnect: {e}");
                // Connection failures are warning-level: the loop below
                // retries with backoff, so only the connection-lifecycle
                // event is sent (no duplicate `AccountEvent::Error`). The UI
                // treats retryable states as non-actionable and stays quiet.
                let _ = events
                    .send(AccountEvent::ConnectionStateChanged(ConnectionState::Error {
                        message: e.to_string(),
                        retryable: true,
                    }))
                    .await;
            }
        }

        // Wait out the backoff delay, but cut it short if a command arrives
        // in the meantime (in particular `Reconnect`, sent by the app crate
        // when `Gio.NetworkMonitor` reports connectivity is back - no point
        // waiting out a multi-second delay once the network is actually
        // usable again). `Shutdown` received while disconnected exits
        // immediately rather than looping back into another connect attempt;
        // any other command (the user opened a message, switched folders,
        // ...) can't be answered without a session, but must not just vanish
        // either - it's carried into the next `connect_and_run` above rather
        // than dropped, so reconnecting doesn't silently eat the interaction
        // that was waiting on it.
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            cmd = commands.recv() => {
                match cmd {
                    Ok(AccountCommand::Shutdown) => {
                        let _ = events.send(AccountEvent::ConnectionStateChanged(ConnectionState::Disconnected)).await;
                        return;
                    }
                    Ok(other) => carried_command = Some(other),
                    Err(_) => {}
                }
            }
        }
        backoff = std::cmp::min(backoff * 2, MAX_BACKOFF);
    }
}

enum ShutdownReason {
    Requested,
}

async fn connect_and_run(
    config: &AccountConfig,
    credentials: &dyn CredentialProvider,
    commands: &async_channel::Receiver<AccountCommand>,
    events: &async_channel::Sender<AccountEvent>,
    cache: &CacheHandle,
    mut carried_command: Option<AccountCommand>,
) -> Result<ShutdownReason> {
    let credential = credentials.imap_credential().await.map_err(Error::LoginFailed)?;
    let (mut session, has_move, condstore, has_list_status, qresync) = login(config, credential).await?;

    let account_id = config.account_id.clone();
    let (mut folders, list_counts_supplied) = list_mailboxes(&mut session, &account_id, condstore, has_list_status).await?;
    // Fast first paint: the folder tree shows immediately and the INBOX sync
    // that fills the message list starts right away. On a LIST-STATUS (RFC
    // 5819) connection the list just answered every folder's counts in that
    // one round trip, so the sidebar paints them instantly and no per-folder
    // STATUS pass is queued; otherwise the counts are queued for the main
    // loop's cooperative drain (see below) rather than issued here, so a
    // large account's count pass never stalls the session for commands.
    //
    // A LIST that carried no counts arrives all-zero, so the ones learned
    // last run are carried over from the cache first - otherwise this emit
    // would visibly blank every count the pre-connect cache replay just
    // painted, and the sidebar would flash counts -> zeros -> counts on every
    // launch. A LIST-STATUS list keeps its fresh values and fills only gaps.
    let mut known_folders: Option<Vec<Mailbox>> = None;
    if let Some(Ok(known)) = cache_op(cache, {
        let account_id = account_id.clone();
        move |c| c.load_mailboxes(&account_id)
    })
    .await
    {
        carry_counts_forward(&mut folders, &known, list_counts_supplied);
        known_folders = Some(known);
    }
    publish_folders(&folders, &account_id, cache, events).await;

    let inbox_id = MailboxId::new(&account_id, "INBOX");
    // Fresh session: no mailbox is SELECTed yet, so the sync must select INBOX.
    sync_mailbox(&mut session, &account_id, "INBOX", &inbox_id, events, cache, &mut folders, None, condstore, qresync).await?;

    // Folders still awaiting their STATUS count, drained cooperatively below.
    // Held as ids rather than indices so a re-list that reorders (or shortens)
    // the folder list can't leave the queue pointing at the wrong mailbox.
    // Re-filled by `relist_folders` whenever the list changes. Empty when the
    // LIST-STATUS path already supplied the counts in one round trip.
    let mut counts_pending: VecDeque<MailboxId> = if list_counts_supplied {
        VecDeque::new()
    } else {
        queue_folder_counts(&folders, &inbox_id)
    };

    // Mailboxes *other than the one currently on screen* whose server-side
    // counts have moved since their message cache was last synced - new mail
    // landing in a folder that isn't selected/IDLE'd. Plain IDLE only reports
    // the selected mailbox, and even a freshly re-entered IDLE never reports
    // a change that happened before it started; the STATUS drain (or, on a
    // LIST-STATUS connection, the fresh counts of the last LIST) is what
    // notices. `SyncMailbox`'s cache-hit shortcut consults this so a folder
    // switch doesn't repaint a stale envelope list just because the cache
    // happens to be non-empty; a real sync clears the entry once it runs. The
    // currently-open mailbox never lives in this set - the drain (or the
    // caller's post-re-list resync) refreshes it immediately since the user
    // is looking at it right now.
    let mut dirty_mailboxes: HashSet<MailboxId> = HashSet::new();
    // With LIST-STATUS there is no drain pass to notice count moves later -
    // the comparison runs now, against the pre-list state just carried over
    // from the cache. The open folder (INBOX) is synced above regardless, so
    // only off-screen mailboxes are marked.
    if list_counts_supplied {
        if let Some(known) = &known_folders {
            mark_count_changes(known, &folders, &inbox_id, &mut dirty_mailboxes);
        }
    }

    let mut current_mailbox_name = "INBOX".to_string();
    let mut current_mailbox_id = inbox_id.clone();

    // The mailbox the IMAP session is actually SELECTed on. This normally
    // tracks `current_mailbox_id`, but diverges whenever the cache-skip path
    // serves a folder switch without a SELECT (see `SyncMailbox`). IDLE waits
    // monitor whichever folder is selected, so before re-entering IDLE we
    // re-SELECT the user's folder if they've drifted apart.
    let mut session_selected = inbox_id.clone();

    // Build the initial prefetch list from all selectable mailboxes except
    // INBOX (already synced above). The prefetch will run cooperatively in
    // batches between IDLE cycles.
    let prefetch_mailboxes: Vec<MailboxId> = folders.iter().filter(|m| m.id != inbox_id).map(|m| m.id.clone()).collect();
    let mut prefetch = if prefetch_mailboxes.is_empty() {
        None
    } else {
        tracing::info!(count = prefetch_mailboxes.len(), "starting background body prefetch");
        Some(PrefetchState::new(prefetch_mailboxes))
    };

    // What ended a main-loop iteration's wait. Declared outside the loop
    // because the wait itself is now conditional - a command already sitting
    // in the queue produces a `Wake` without any IDLE at all.
    enum Wake {
        Idle(std::result::Result<async_imap::extensions::idle::IdleResponse, async_imap::error::Error>),
        Command(AccountCommand),
        ChannelClosed,
    }

    loop {
        // Take a command that's *already* queued without entering IDLE first.
        // Establishing an IDLE and tearing it down again costs two round
        // trips, and the previous shape paid them both before the wake select
        // could even look at the queue - so every command that arrived while
        // the session was busy with background work (a prefetch batch, a
        // folder-count STATUS) waited on a SELECT plus those two round trips
        // purely to be handed something already in hand. That is most of what
        // made a folder click land a beat late while counts were filling in.
        // A command carried over from a previous, failed connection attempt
        // (see `run_account_session`) takes priority over the queue - it's
        // already the oldest thing waiting to be answered.
        let wake = match carried_command.take() {
            Some(cmd) => Wake::Command(cmd),
            None => match commands.try_recv() {
                Ok(cmd) => Wake::Command(cmd),
                Err(async_channel::TryRecvError::Closed) => return Ok(ShutdownReason::Requested),
                Err(async_channel::TryRecvError::Empty) => {
                    let _ = events.send(AccountEvent::ConnectionStateChanged(ConnectionState::Idle)).await;

                    // A cache-served folder switch (or an interrupted prefetch) can
                    // leave the session SELECTed on a different folder than the one
                    // the user is viewing. IDLE only reports changes to the
                    // currently-selected folder, so bring the session back in line
                    // before the wait. This is a cheap round trip (no FETCH) and is
                    // skipped whenever the session already matches. Only reached on
                    // the way *into* IDLE, so a queued command never pays for it -
                    // its own handler selects whatever folder it needs.
                    if session_selected != current_mailbox_id {
                        let meta = session.select(&current_mailbox_name).await?;
                        session_selected = current_mailbox_id.clone();
                        // Keep the folder list's `uidvalidity`/`uidnext` fresh so a
                        // sync that skips its SELECT (the session is already on the
                        // folder, see `sync_mailbox`) still keys the cache correctly
                        // instead of falling back to a stale pre-connect value.
                        if let Some(f) = folders.iter_mut().find(|f| f.id == current_mailbox_id) {
                            if let Some(v) = meta.uid_validity {
                                f.uidvalidity = UidValidity(v);
                            }
                            if let Some(next) = meta.uid_next {
                                f.uidnext = next;
                            }
                            if let Some(modseq) = meta.highest_modseq {
                                f.highest_modseq = Some(modseq);
                            }
                        }
                    }

                    let mut handle = session.idle();
                    handle.init().await?;
                    let (wait_fut, stop_source) = handle.wait_with_timeout(IDLE_SLICE);

                    // Race the IDLE wait against the next command so an on-demand
                    // request (open a message, switch folders, ...) doesn't wait for
                    // IDLE_SLICE to elapse. If the command branch wins, `wait_fut` is
                    // dropped along with `stop_source`; dropping a `StopSource` cancels
                    // its associated wait immediately (see `stop_token::StopSource`'s
                    // docs) - but since we're also dropping `wait_fut` itself here, we
                    // don't even need to observe that cancellation, we just move
                    // straight on to `handle.done()` below to send IMAP's `DONE` and
                    // reclaim the session.
                    let wake = tokio::select! {
                        r = wait_fut => Wake::Idle(r),
                        c = commands.recv() => match c {
                            Ok(cmd) => Wake::Command(cmd),
                            Err(_) => Wake::ChannelClosed,
                        },
                    };

                    // Emit cached messages for instant display the instant a folder
                    // switch arrives, *before* the IDLE teardown (handle.done().await)
                    // so the UI paints from disk while we wait for the network round-trip.
                    if let Wake::Command(AccountCommand::SyncMailbox(mailbox_id)) = &wake {
                        emit_cached_messages(cache, mailbox_id, events).await;
                    }

                    drop(stop_source);
                    session = handle.done().await?;
                    wake
                }
            },
        };

        let _ = events.send(AccountEvent::ConnectionStateChanged(ConnectionState::Busy)).await;

        let mut woke_on_command = None;
        match wake {
            // A server notification during IDLE (EXISTS/EXPUNGE/etc) means
            // the currently-selected mailbox changed; re-sync it. When
            // CONDSTORE is enabled this is a `CHANGEDSINCE` delta, not a
            // full flag re-fetch - see `sync_mailbox`.
            Wake::Idle(Ok(async_imap::extensions::idle::IdleResponse::Timeout)) => {}
            Wake::Idle(Ok(_)) => {
                // `sync_mailbox` derives the open folder's count fields from what
                // this sync already fetched (the flag list for `unread`, SELECT
                // meta for `total`/`uidnext`) and re-publishes the sidebar only
                // when they changed - no STATUS round trip on top of the wake.
                sync_mailbox(
                    &mut session,
                    &account_id,
                    &current_mailbox_name,
                    &current_mailbox_id,
                    events,
                    cache,
                    &mut folders,
                    Some(&session_selected),
                    condstore,
                    qresync,
                )
                .await?;
                dirty_mailboxes.remove(&current_mailbox_id);
                session_selected = current_mailbox_id.clone();
            }
            Wake::Idle(Err(e)) => return Err(Error::Imap(e)),
            Wake::Command(cmd) => woke_on_command = Some(cmd),
            Wake::ChannelClosed => return Ok(ShutdownReason::Requested),
        }

        // Process the command that woke us (if any), then drain any further
        // commands queued up while we were mid-teardown.
        for command in woke_on_command.into_iter().chain(std::iter::from_fn(|| commands.try_recv().ok())) {
            match command {
                AccountCommand::Shutdown => {
                    let _ = session.logout().await;
                    return Ok(ShutdownReason::Requested);
                }
                AccountCommand::Refresh => {
                    relist_folders(
                        &mut session,
                        &mut folders,
                        &mut counts_pending,
                        &account_id,
                        &current_mailbox_id,
                        cache,
                        events,
                        condstore,
                        has_list_status,
                        &mut dirty_mailboxes,
                    )
                    .await?;
                    sync_mailbox(
                        &mut session,
                        &account_id,
                        &current_mailbox_name,
                        &current_mailbox_id,
                        events,
                        cache,
                        &mut folders,
                        Some(&session_selected),
                        condstore,
                        qresync,
                    )
                    .await?;
                    session_selected = current_mailbox_id.clone();
                    // Rebuild the prefetch list to include any new folders.
                    let new_mailboxes: Vec<MailboxId> = folders.iter().filter(|m| m.id != current_mailbox_id).map(|m| m.id.clone()).collect();
                    if !new_mailboxes.is_empty() {
                        prefetch = Some(PrefetchState::new(new_mailboxes));
                    }
                }
                AccountCommand::SyncMailbox(mailbox_id) => {
                    // MailboxId is "<account_id>:<folder path>"; recover the folder path.
                    if let Some(path) = mailbox_id.0.strip_prefix(&format!("{}:", account_id.0)) {
                        current_mailbox_name = path.to_string();
                        current_mailbox_id = mailbox_id;
                        // If the cache already has envelope summaries for this
                        // mailbox, emit them and skip the full IMAP sync to
                        // avoid blocking the session for seconds on a network
                        // round trip that would produce identical data. This
                        // mirrors the pre-IDLE cached emit, but *must* happen
                        // here too: a SyncMailbox that arrived queued behind
                        // another command (e.g. a FetchBody that woke the
                        // session) never goes through the wake-command path,
                        // and without this emit the app's sync request would
                        // be answered by nothing - its pending entry would
                        // stick and suppress every later sync for this folder.
                        //
                        // The cache is safe to serve because `Cache::open`
                        // wipes the envelope table once when the on-disk
                        // format version changes, and every `sync_mailbox`
                        // since then writes the whole folder - so a non-empty
                        // cache is a complete snapshot, never a pre-fix
                        // windowed subset. (A `STATUS (MESSAGES)` count is
                        // *not* a safe completeness reference: on Gmail's All
                        // Mail it over-reports vs. what a fetch returns, which
                        // would force a pointless full re-sync every open.)
                        // A STATUS pass may have observed this mailbox's count
                        // move while it wasn't selected (new mail arriving
                        // outside IDLE's view, see `dirty_mailboxes` above) -
                        // that makes the cached envelope list stale even
                        // though it's non-empty, so the shortcut below must
                        // not apply to it.
                        let cached = !dirty_mailboxes.contains(&current_mailbox_id)
                            && cache_op(cache, {
                                let mailbox = current_mailbox_id.clone();
                                move |c| c.has_messages(&mailbox)
                            })
                            .await
                            .and_then(|r| r.ok())
                            .unwrap_or(false);
                        if cached {
                            tracing::debug!(mailbox = %current_mailbox_id, "SyncMailbox: cache hit, emitting cached messages without IMAP sync");
                            emit_cached_messages(cache, &current_mailbox_id, events).await;
                            continue;
                        }
                        sync_mailbox(
                            &mut session,
                            &account_id,
                            &current_mailbox_name,
                            &current_mailbox_id,
                            events,
                            cache,
                            &mut folders,
                            Some(&session_selected),
                            condstore,
                            qresync,
                        )
                        .await?;
                        session_selected = current_mailbox_id.clone();
                        dirty_mailboxes.remove(&current_mailbox_id);
                    }
                }
                AccountCommand::FetchBody { mailbox, uid } => {
                    // Fetch the body from whichever mailbox it actually lives
                    // in. Previously this required `mailbox` to be the open
                    // folder, which full-text search broke: a search result
                    // can come from any folder (or account) the local index
                    // covers, and opening it must not silently no-op. SELECTing
                    // on demand has the same contract as `StoreFlags` - the
                    // body may live anywhere - and the top of the loop puts
                    // the session back on the user's folder before the next
                    // IDLE wait.
                    let Some(path) = mailbox.0.strip_prefix(&format!("{}:", account_id.0)).map(str::to_string) else {
                        continue;
                    };
                    if session_selected != mailbox {
                        session.select(&path).await?;
                        session_selected = mailbox.clone();
                    }
                    // The mailbox's uidvalidity guards the body cache against
                    // serving a stale body for a recycled uid after the
                    // mailbox was re-created (see `Cache::load_body`). The
                    // `UidValidity(0)` fallback (mailbox not in the list) is
                    // a deliberate cache-miss sentinel: no row can match 0.
                    let uidvalidity = folders.iter().find(|m| m.id == mailbox).map(|m| m.uidvalidity).unwrap_or(UidValidity(0));
                    let started = std::time::Instant::now();
                    // `None` for the structure: the partial-fetch path reads
                    // the message's summary (with its BODYSTRUCTURE-derived
                    // part list) from the cache, and falls back to a
                    // whole-message fetch when there isn't one.
                    let body = fetch_body_cached(cache, &mut session, &mailbox, uid, uidvalidity, None).await?;
                    tracing::debug!(?mailbox, uid = uid.0, elapsed_ms = started.elapsed().as_millis(), "FetchBody: body ready");
                    if let Some(body) = body {
                        let _ = events.send(AccountEvent::BodyFetched { mailbox, uid, body }).await;
                    }
                }
                AccountCommand::FetchAttachment { mailbox, uid, part } => {
                    // Same on-demand-SELECT contract as `FetchBody` and
                    // `StoreFlags`: the attachment lives wherever the message
                    // does, and the top of the loop puts the session back on
                    // the user's folder before the next IDLE wait.
                    let Some(path) = mailbox.0.strip_prefix(&format!("{}:", account_id.0)).map(str::to_string) else {
                        continue;
                    };
                    if session_selected != mailbox {
                        session.select(&path).await?;
                        session_selected = mailbox.clone();
                    }
                    // The mailbox's uidvalidity guards the flat-file cache the
                    // same way it guards the body cache - a recycled uid after
                    // a mailbox re-create must not resolve to a stale part
                    // (see `Cache::load_attachment`). `UidValidity(0)` is the
                    // deliberate cache-miss sentinel used by `FetchBody`.
                    let uidvalidity = folders.iter().find(|m| m.id == mailbox).map(|m| m.uidvalidity).unwrap_or(UidValidity(0));
                    let started = std::time::Instant::now();
                    // A failed part fetch must answer the UI with
                    // `PartFetchFailed` rather than kill the whole session via
                    // `?` - one bad section shouldn't cost the connection.
                    let fetched = match cache_op(cache, {
                        let mailbox = mailbox.clone();
                        let part_number = part.part_number.clone();
                        move |c| c.load_attachment(&mailbox, uid, uidvalidity, &part_number)
                    })
                    .await
                    {
                        Some(Ok(Some(bytes))) => {
                            tracing::debug!(?mailbox, uid = uid.0, part = %part.part_number, "FetchAttachment: served from disk cache");
                            Ok(Some(bytes))
                        }
                        Some(Ok(None)) => fetch_attachment_part(&mut session, uid, &part).await,
                        Some(Err(e)) => {
                            tracing::warn!(?mailbox, uid = uid.0, part = %part.part_number, "failed to read cached attachment: {e}");
                            fetch_attachment_part(&mut session, uid, &part).await
                        }
                        None => fetch_attachment_part(&mut session, uid, &part).await,
                    };
                    let bytes = match fetched {
                        Ok(Some(bytes)) => Some(bytes),
                        // `None` means the server didn't return the requested
                        // section - a part number that doesn't match the
                        // server's view of the message. That's usually a
                        // structure that went stale after the message was
                        // rewritten server-side (mailing-list/gateway/proxy
                        // rewrites after the body or summary was cached): the
                        // part is still there, just not at the old section
                        // path. Re-derive it from the whole message before
                        // giving up.
                        Ok(None) => match fetch_stale_part_fallback(cache, &mut session, &mailbox, uid, uidvalidity, &part).await {
                            Ok(Some(bytes)) => Some(bytes),
                            // The re-derive found no such part either - the
                            // part genuinely no longer exists.
                            Ok(None) => {
                                tracing::warn!(?mailbox, uid = uid.0, part = %part.part_number, "FetchAttachment: server returned no such section");
                                let _ = events
                                    .send(AccountEvent::PartFetchFailed {
                                        mailbox: mailbox.clone(),
                                        uid,
                                        part_number: part.part_number.clone(),
                                        message: "the server didn't return this attachment's part - it may no longer exist".to_string(),
                                    })
                                    .await;
                                None
                            }
                            Err(e) => {
                                tracing::warn!(?mailbox, uid = uid.0, part = %part.part_number, "FetchAttachment: stale-structure fallback failed: {e}");
                                let _ = events
                                    .send(AccountEvent::PartFetchFailed {
                                        mailbox: mailbox.clone(),
                                        uid,
                                        part_number: part.part_number.clone(),
                                        message: format!("couldn't fetch this attachment: {e}"),
                                    })
                                    .await;
                                None
                            }
                        },
                        Err(e) => {
                            tracing::warn!(?mailbox, uid = uid.0, part = %part.part_number, "FetchAttachment: fetch failed: {e}");
                            let _ = events
                                .send(AccountEvent::PartFetchFailed {
                                    mailbox: mailbox.clone(),
                                    uid,
                                    part_number: part.part_number.clone(),
                                    message: format!("couldn't fetch this attachment: {e}"),
                                })
                                .await;
                            None
                        }
                    };
                    if let Some(bytes) = bytes {
                        if let Some(Err(e)) = cache_op(cache, {
                            let mailbox = mailbox.clone();
                            let part_number = part.part_number.clone();
                            let bytes = bytes.clone();
                            move |c| c.store_attachment(&mailbox, uid, uidvalidity, &part_number, &bytes)
                        })
                        .await
                        {
                            tracing::warn!(?mailbox, uid = uid.0, part = %part.part_number, "failed to cache attachment bytes: {e}");
                        }
                        tracing::debug!(?mailbox, uid = uid.0, part = %part.part_number, bytes = bytes.len(), elapsed_ms = started.elapsed().as_millis(), "FetchAttachment: part ready");
                        let _ = events.send(AccountEvent::PartFetched { mailbox, uid, part, bytes }).await;
                    }
                }
                AccountCommand::FetchRawMessage { mailbox, uid } => {
                    // Same on-demand-SELECT contract as `FetchBody`/`StoreFlags`
                    // - the message may live in any folder - and the top of the
                    // loop puts the session back on the user's folder before
                    // the next IDLE wait.
                    let Some(path) = mailbox.0.strip_prefix(&format!("{}:", account_id.0)).map(str::to_string) else {
                        continue;
                    };
                    if session_selected != mailbox {
                        session.select(&path).await?;
                        session_selected = mailbox.clone();
                    }
                    // The mailbox's uidvalidity guards the raw-message flat-file
                    // cache the same way it guards attachments and bodies; the
                    // `UidValidity(0)` fallback is the same cache-miss sentinel.
                    let uidvalidity = folders.iter().find(|m| m.id == mailbox).map(|m| m.uidvalidity).unwrap_or(UidValidity(0));
                    let started = std::time::Instant::now();
                    // A failure must answer the UI with `RawMessageFetchFailed`
                    // rather than kill the whole session via `?` - one bad
                    // message shouldn't cost the connection.
                    let fetched = fetch_raw_message_cached(cache, &mut session, &mailbox, uid, uidvalidity).await;
                    match fetched {
                        Ok(Some(bytes)) => {
                            tracing::debug!(
                                ?mailbox,
                                uid = uid.0,
                                bytes = bytes.len(),
                                elapsed_ms = started.elapsed().as_millis(),
                                "FetchRawMessage: raw message ready"
                            );
                            let _ = events.send(AccountEvent::RawMessageFetched { mailbox, uid, bytes }).await;
                        }
                        Ok(None) => {
                            tracing::warn!(?mailbox, uid = uid.0, "FetchRawMessage: server returned no such message");
                            let _ = events
                                .send(AccountEvent::RawMessageFetchFailed {
                                    mailbox: mailbox.clone(),
                                    uid,
                                    message: "the server didn't return this message - it may no longer exist".to_string(),
                                })
                                .await;
                        }
                        Err(e) => {
                            tracing::warn!(?mailbox, uid = uid.0, "FetchRawMessage: fetch failed: {e}");
                            let _ = events
                                .send(AccountEvent::RawMessageFetchFailed {
                                    mailbox: mailbox.clone(),
                                    uid,
                                    message: format!("couldn't fetch this message: {e}"),
                                })
                                .await;
                        }
                    }
                }
                AccountCommand::SearchMailbox { mailbox, query } => {
                    // The server-side search pass of full-text search. One
                    // round trip per folder, so the app only sends this for
                    // the mailbox the user is viewing; SELECTing on demand
                    // (same contract as `StoreFlags`/`FetchBody` above) covers
                    // a search result from any folder, and the top of the loop
                    // puts the session back on the user's folder afterwards.
                    let Some(path) = mailbox.0.strip_prefix(&format!("{}:", account_id.0)).map(str::to_string) else {
                        continue;
                    };
                    if session_selected != mailbox {
                        session.select(&path).await?;
                        session_selected = mailbox.clone();
                    }
                    let messages = match search_mailbox(&mut session, &account_id, &mailbox, &query).await {
                        Ok(messages) => messages,
                        // A failed search must still complete the app's live
                        // pass (an empty answer), or its "searching" state
                        // would stick forever - and this is a background
                        // nicety, not a user action, so a warning is enough.
                        Err(e) => {
                            tracing::warn!(%mailbox, "mailbox search failed: {e}");
                            Vec::new()
                        }
                    };
                    let _ = events.send(AccountEvent::SearchResults { mailbox, query, messages }).await;
                }
                AccountCommand::SendMessage(msg) => match send_message(config, credentials, &mut session, &folders, *msg).await {
                    Ok(sent_mailbox) => {
                        let _ = events.send(AccountEvent::SendCompleted).await;
                        // The APPEND landed in Sent - resync it now rather than
                        // leaving the newly archived copy stuck behind the
                        // cache-hit shortcut in `SyncMailbox` above, which would
                        // otherwise hide it indefinitely (even across restarts)
                        // until an explicit Refresh. Mirrors the resync-after-
                        // mutation pattern `MoveMessage` uses below.
                        if let Some(sent_id) = sent_mailbox {
                            if let Some(path) = sent_id.0.strip_prefix(&format!("{}:", account_id.0)).map(str::to_string) {
                                let open_changed = relist_folders(
                                    &mut session,
                                    &mut folders,
                                    &mut counts_pending,
                                    &account_id,
                                    &current_mailbox_id,
                                    cache,
                                    events,
                                    condstore,
                                    has_list_status,
                                    &mut dirty_mailboxes,
                                )
                                .await?;
                                sync_mailbox(&mut session, &account_id, &path, &sent_id, events, cache, &mut folders, Some(&session_selected), condstore, qresync).await?;
                                dirty_mailboxes.remove(&sent_id);
                                session_selected = sent_id.clone();
                                if session_selected != current_mailbox_id {
                                    session.select(&current_mailbox_name).await?;
                                    session_selected = current_mailbox_id.clone();
                                }
                                // The re-list's LIST-STATUS counts said the
                                // open folder changed and the Sent sync above
                                // didn't cover it - resync so the sidebar and
                                // the list on screen agree (the STATUS drain's
                                // immediate-resync counterpart).
                                if open_changed && sent_id != current_mailbox_id {
                                    sync_mailbox(
                                        &mut session,
                                        &account_id,
                                        &current_mailbox_name,
                                        &current_mailbox_id,
                                        events,
                                        cache,
                                        &mut folders,
                                        Some(&session_selected),
                                        condstore,
                                        qresync,
                                    )
                                    .await?;
                                    dirty_mailboxes.remove(&current_mailbox_id);
                                    session_selected = current_mailbox_id.clone();
                                }
                            }
                        }
                    }
                    Err(e) => {
                        let _ = events.send(AccountEvent::SendFailed(format!("Couldn't send message: {e}"))).await;
                    }
                },
                AccountCommand::SaveDraft { msg, replace } => {
                    let had_drafts_folder = drafts_path(&folders, &account_id).is_some();
                    match save_draft(&mut session, &folders, &account_id, *msg, replace).await {
                        Ok(message_id) => {
                            let _ = events.send(AccountEvent::DraftSaved { message_id }).await;
                        }
                        Err(e) => {
                            tracing::warn!("draft save failed: {e}");
                        }
                    }
                    // The draft path SELECTs the Drafts mailbox; bring the
                    // session back to the user's folder so IDLE and the next
                    // command operate on what's actually on screen.
                    session.select(&current_mailbox_name).await?;
                    session_selected = current_mailbox_id.clone();
                    // `save_draft` may have CREATEd a Drafts mailbox the
                    // folder list doesn't know about; refresh the list so
                    // the next save finds it (instead of CREATE-ing again)
                    // and the folder tree shows it. Same pattern as
                    // `MoveMessage`.
                    if !had_drafts_folder {
                        let open_changed = relist_folders(
                            &mut session,
                            &mut folders,
                            &mut counts_pending,
                            &account_id,
                            &current_mailbox_id,
                            cache,
                            events,
                            condstore,
                            has_list_status,
                            &mut dirty_mailboxes,
                        )
                        .await?;
                        // The re-list's LIST-STATUS counts said the open
                        // folder changed while the draft save had the session
                        // elsewhere - resync it (the STATUS drain's
                        // immediate-resync counterpart).
                        if open_changed {
                            sync_mailbox(
                                &mut session,
                                &account_id,
                                &current_mailbox_name,
                                &current_mailbox_id,
                                events,
                                cache,
                                &mut folders,
                                Some(&session_selected),
                                condstore,
                                qresync,
                            )
                            .await?;
                            dirty_mailboxes.remove(&current_mailbox_id);
                            session_selected = current_mailbox_id.clone();
                        }
                    }
                }
                AccountCommand::DeleteDraft { message_id } => {
                    if let Err(e) = delete_draft(&mut session, &folders, &account_id, &message_id).await {
                        tracing::warn!("draft delete failed: {e}");
                    }
                    session.select(&current_mailbox_name).await?;
                    session_selected = current_mailbox_id.clone();
                }
                // Already connected - nothing to reconnect. This variant
                // only does something useful while backed off between
                // connection attempts, see `run_account_session`.
                AccountCommand::Reconnect => {}
                AccountCommand::MoveMessage { mailbox, uid, role } => {
                    let Some(path) = mailbox.0.strip_prefix(&format!("{}:", account_id.0)).map(str::to_string) else {
                        continue;
                    };
                    // SELECT the message's own folder on demand, same
                    // contract as `StoreFlags` below: a move can race a
                    // folder switch, and the unified "All Inboxes" view
                    // shows messages from mailboxes this session doesn't
                    // currently have open at all. The main loop's own
                    // re-select puts the session back on the user's folder
                    // before the next IDLE wait.
                    if session_selected != mailbox {
                        session.select(&path).await?;
                        session_selected = mailbox.clone();
                    }
                    match move_message_to_role(&mut session, &folders, &account_id, &[uid], role, has_move).await {
                        Ok(()) => {
                            let _ = events.send(AccountEvent::MessageMoved { role }).await;
                            // The MOVE already succeeded server-side, so drop the
                            // message from the cache and republish the remaining
                            // cached set right away. Without this, the row only
                            // disappears once the authoritative resync below has
                            // re-fetched the whole envelope window - seconds of
                            // network round trips the user experiences as lag.
                            // The subsequent `sync_mailbox` emit is byte-identical
                            // (the message is gone from the server too), so the UI
                            // rebuilds once and the list never flickers.
                            if let Some(Err(e)) = cache_op(cache, {
                                let mailbox = mailbox.clone();
                                move |c| c.delete_messages(&mailbox, &[uid])
                            })
                            .await
                            {
                                tracing::warn!("failed to drop moved message from cache: {e}");
                            }
                            emit_cached_messages_after_removal(cache, &mailbox, events).await;
                            relist_folders(
                                &mut session,
                                &mut folders,
                                &mut counts_pending,
                                &account_id,
                                &current_mailbox_id,
                                cache,
                                events,
                                condstore,
                                has_list_status,
                                &mut dirty_mailboxes,
                            )
                            .await?;
                            sync_mailbox(
                                &mut session,
                                &account_id,
                                &current_mailbox_name,
                                &current_mailbox_id,
                                events,
                                cache,
                                &mut folders,
                                Some(&session_selected),
                                condstore,
                                qresync,
                            )
                            .await?;
                            session_selected = current_mailbox_id.clone();
                        }
                        Err(e) => {
                            let _ = events
                                .send(AccountEvent::MoveFailed {
                                    mailbox,
                                    uids: vec![uid],
                                    role,
                                    message: format!("Couldn't move message: {e}"),
                                })
                                .await;
                        }
                    }
                }
                AccountCommand::MoveMessages { mailbox, uids, role } => {
                    if uids.is_empty() {
                        continue;
                    }
                    let Some(path) = mailbox.0.strip_prefix(&format!("{}:", account_id.0)).map(str::to_string) else {
                        continue;
                    };
                    if session_selected != mailbox {
                        session.select(&path).await?;
                        session_selected = mailbox.clone();
                    }
                    match move_message_to_role(&mut session, &folders, &account_id, &uids, role, has_move).await {
                        Ok(()) => {
                            let _ = events.send(AccountEvent::MessageMoved { role }).await;
                            if let Some(Err(e)) = cache_op(cache, {
                                let mailbox = mailbox.clone();
                                let uids = uids.clone();
                                move |c| c.delete_messages(&mailbox, &uids)
                            })
                            .await
                            {
                                tracing::warn!("failed to drop moved messages from cache: {e}");
                            }
                            emit_cached_messages_after_removal(cache, &mailbox, events).await;
                            relist_folders(
                                &mut session,
                                &mut folders,
                                &mut counts_pending,
                                &account_id,
                                &current_mailbox_id,
                                cache,
                                events,
                                condstore,
                                has_list_status,
                                &mut dirty_mailboxes,
                            )
                            .await?;
                            sync_mailbox(
                                &mut session,
                                &account_id,
                                &current_mailbox_name,
                                &current_mailbox_id,
                                events,
                                cache,
                                &mut folders,
                                Some(&session_selected),
                                condstore,
                                qresync,
                            )
                            .await?;
                            session_selected = current_mailbox_id.clone();
                        }
                        Err(e) => {
                            let _ = events
                                .send(AccountEvent::MoveFailed {
                                    mailbox,
                                    uids,
                                    role,
                                    message: format!("Couldn't move messages: {e}"),
                                })
                                .await;
                        }
                    }
                }
                AccountCommand::MoveMessagesTo { mailbox, uids, target } => {
                    if uids.is_empty() {
                        continue;
                    }
                    let Some(path) = mailbox.0.strip_prefix(&format!("{}:", account_id.0)).map(str::to_string) else {
                        continue;
                    };
                    let Some(target_path) = target.0.strip_prefix(&format!("{}:", account_id.0)).map(str::to_string) else {
                        continue;
                    };
                    if path == target_path {
                        continue;
                    }
                    if session_selected != mailbox {
                        session.select(&path).await?;
                        session_selected = mailbox.clone();
                    }
                    match move_uids_to_path(&mut session, &uids, &target_path, has_move).await {
                        Ok(()) => {
                            // `Custom` here is just a marker for the generic
                            // "Moved" toast - the target was an arbitrary
                            // folder, not a special-use role.
                            let _ = events.send(AccountEvent::MessageMoved { role: MailboxRole::Custom }).await;
                            if let Some(Err(e)) = cache_op(cache, {
                                let mailbox = mailbox.clone();
                                let uids = uids.clone();
                                move |c| c.delete_messages(&mailbox, &uids)
                            })
                            .await
                            {
                                tracing::warn!("failed to drop moved messages from cache: {e}");
                            }
                            emit_cached_messages_after_removal(cache, &mailbox, events).await;
                            relist_folders(
                                &mut session,
                                &mut folders,
                                &mut counts_pending,
                                &account_id,
                                &current_mailbox_id,
                                cache,
                                events,
                                condstore,
                                has_list_status,
                                &mut dirty_mailboxes,
                            )
                            .await?;
                            sync_mailbox(
                                &mut session,
                                &account_id,
                                &current_mailbox_name,
                                &current_mailbox_id,
                                events,
                                cache,
                                &mut folders,
                                Some(&session_selected),
                                condstore,
                                qresync,
                            )
                            .await?;
                            session_selected = current_mailbox_id.clone();
                        }
                        Err(e) => {
                            let _ = events
                                .send(AccountEvent::MoveFailed {
                                    mailbox,
                                    uids,
                                    role: MailboxRole::Custom,
                                    message: format!("Couldn't move messages: {e}"),
                                })
                                .await;
                        }
                    }
                }
                AccountCommand::EmptyMailbox { mailbox } => {
                    let Some(path) = mailbox.0.strip_prefix(&format!("{}:", account_id.0)).map(str::to_string) else {
                        continue;
                    };
                    if session_selected != mailbox {
                        session.select(&path).await?;
                        session_selected = mailbox.clone();
                    }
                    match empty_mailbox(&mut session).await {
                        Ok(count) => {
                            let role = folders.iter().find(|m| m.id == mailbox).map(|m| m.role).unwrap_or(MailboxRole::Custom);
                            let _ = events.send(AccountEvent::MailboxExpunged { role }).await;
                            if count > 0 {
                                if let Some(folder) = folders.iter().find(|m| m.id == mailbox) {
                                    let uidvalidity = folder.uidvalidity;
                                    if let Some(Err(e)) = cache_op(cache, {
                                        let mailbox = mailbox.clone();
                                        move |c| c.replace_messages(&mailbox, uidvalidity, &[])
                                    })
                                    .await
                                    {
                                        tracing::warn!("failed to clear emptied mailbox from cache: {e}");
                                    }
                                }
                            }
                            relist_folders(
                                &mut session,
                                &mut folders,
                                &mut counts_pending,
                                &account_id,
                                &current_mailbox_id,
                                cache,
                                events,
                                condstore,
                                has_list_status,
                                &mut dirty_mailboxes,
                            )
                            .await?;
                            sync_mailbox(
                                &mut session,
                                &account_id,
                                &current_mailbox_name,
                                &current_mailbox_id,
                                events,
                                cache,
                                &mut folders,
                                Some(&session_selected),
                                condstore,
                                qresync,
                            )
                            .await?;
                            session_selected = current_mailbox_id.clone();
                        }
                        Err(e) => {
                            let _ = events.send(AccountEvent::Error(format!("Couldn't empty folder: {e}"))).await;
                        }
                    }
                }
                AccountCommand::SnoozeMessage { mailbox, uid, until } => {
                    if let Some(Err(e)) = cache_op(cache, {
                        let mailbox = mailbox.clone();
                        move |c| c.snooze_message(&mailbox, uid, until)
                    })
                    .await
                    {
                        tracing::warn!("failed to record snooze: {e}");
                    }
                    let _ = events.send(AccountEvent::MessageSnoozed).await;
                    sync_mailbox(
                        &mut session,
                        &account_id,
                        &current_mailbox_name,
                        &current_mailbox_id,
                        events,
                        cache,
                        &mut folders,
                        Some(&session_selected),
                        condstore,
                        qresync,
                    )
                    .await?;
                    session_selected = current_mailbox_id.clone();
                }
                AccountCommand::SnoozeMessages { mailbox, uids, until } => {
                    if let Some(Err(e)) = cache_op(cache, {
                        let mailbox = mailbox.clone();
                        let uids = uids.clone();
                        move |c| c.snooze_messages(&mailbox, &uids, until)
                    })
                    .await
                    {
                        tracing::warn!("failed to record snooze: {e}");
                    }
                    let _ = events.send(AccountEvent::MessageSnoozed).await;
                    sync_mailbox(
                        &mut session,
                        &account_id,
                        &current_mailbox_name,
                        &current_mailbox_id,
                        events,
                        cache,
                        &mut folders,
                        Some(&session_selected),
                        condstore,
                        qresync,
                    )
                    .await?;
                    session_selected = current_mailbox_id.clone();
                }
                AccountCommand::StoreFlags { mailbox, uid, add, remove } => {
                    let Some(path) = mailbox.0.strip_prefix(&format!("{}:", account_id.0)).map(str::to_string) else {
                        continue;
                    };
                    // A flag change is triggered by the user's own selection,
                    // which can race a folder switch (and in the unified
                    // "All Inboxes" view the message needn't be in the folder
                    // this session has open at all). SELECTing here rather
                    // than dropping the command keeps mark-as-read reliable;
                    // the top of the loop puts the session back on the user's
                    // folder before the next IDLE wait.
                    if session_selected != mailbox {
                        session.select(&path).await?;
                        session_selected = mailbox.clone();
                    }
                    match store_flags(&mut session, &[uid], &add, &remove).await {
                        Ok(()) => {
                            // The server is now authoritative-and-changed;
                            // patch the cached summary to match so the list
                            // repaints from cache without a re-fetch (and so
                            // a restart before the next sync doesn't show the
                            // message unread again).
                            let patched = match cache_op(cache, {
                                let mailbox = mailbox.clone();
                                let add = add.clone();
                                let remove = remove.clone();
                                move |c| c.update_flags(&mailbox, uid, &add, &remove)
                            })
                            .await
                            {
                                Some(Ok(patched)) => patched,
                                Some(Err(e)) => {
                                    tracing::warn!("failed to update cached flags: {e}");
                                    false
                                }
                                None => false,
                            };
                            if patched {
                                emit_cached_messages(cache, &mailbox, events).await;
                            } else if mailbox == current_mailbox_id {
                                // No cache (or the uid fell outside the
                                // cached window): re-sync so the UI still
                                // sees the new flags.
                                sync_mailbox(
                                    &mut session,
                                    &account_id,
                                    &current_mailbox_name,
                                    &current_mailbox_id,
                                    events,
                                    cache,
                                    &mut folders,
                                    Some(&session_selected),
                                    condstore,
                                    qresync,
                                )
                                .await?;
                                session_selected = current_mailbox_id.clone();
                            }
                            // A \Seen store changes the folder's unread count;
                            // patch it locally so the sidebar reflects the
                            // change immediately instead of waiting for the
                            // next re-list. The full STATUS pass corrects it
                            // on the next re-list; saturating arithmetic keeps
                            // a best-effort delta from going negative.
                            let mut delta = 0i64;
                            if add.contains(&SystemFlagBit::Seen) {
                                delta -= 1;
                            }
                            if remove.contains(&SystemFlagBit::Seen) {
                                delta += 1;
                            }
                            let count_changed = delta != 0
                                && folders.iter_mut().any(|f| {
                                    if f.id == mailbox {
                                        f.unread = (f.unread as i64 + delta).max(0) as u32;
                                        true
                                    } else {
                                        false
                                    }
                                });
                            if count_changed {
                                publish_folders(&folders, &account_id, cache, events).await;
                            }
                        }
                        Err(e) => {
                            let _ = events
                                .send(AccountEvent::StoreFlagsFailed {
                                    mailbox,
                                    uids: vec![uid],
                                    message: format!("Couldn't update message flags: {e}"),
                                })
                                .await;
                        }
                    }
                }
                AccountCommand::StoreFlagsMany { mailbox, uids, add, remove } => {
                    if uids.is_empty() {
                        continue;
                    }
                    let Some(path) = mailbox.0.strip_prefix(&format!("{}:", account_id.0)).map(str::to_string) else {
                        continue;
                    };
                    if session_selected != mailbox {
                        session.select(&path).await?;
                        session_selected = mailbox.clone();
                    }
                    match store_flags(&mut session, &uids, &add, &remove).await {
                        Ok(()) => {
                            // Resync if even one uid couldn't be cache-patched
                            // (fell outside the cached window, say), rather
                            // than partially patching and partially resyncing -
                            // a full resync is already the correct, cheap
                            // fallback path for a single message, and stays
                            // so for a batch.
                            let all_patched = matches!(
                                cache_op(cache, {
                                    let mailbox = mailbox.clone();
                                    let uids = uids.clone();
                                    let add = add.clone();
                                    let remove = remove.clone();
                                    move |c| c.update_flags_many(&mailbox, &uids, &add, &remove)
                                })
                                .await,
                                Some(Ok(true))
                            );
                            if all_patched {
                                emit_cached_messages(cache, &mailbox, events).await;
                            } else if mailbox == current_mailbox_id {
                                sync_mailbox(
                                    &mut session,
                                    &account_id,
                                    &current_mailbox_name,
                                    &current_mailbox_id,
                                    events,
                                    cache,
                                    &mut folders,
                                    Some(&session_selected),
                                    condstore,
                                    qresync,
                                )
                                .await?;
                                session_selected = current_mailbox_id.clone();
                            }
                            // Best-effort delta scaled by batch size, same
                            // caveat as the single-message case above (and
                            // same self-correction on the next STATUS
                            // re-list): a uid whose \Seen state didn't
                            // actually need to change (e.g. "mark all read"
                            // applied to a selection that included some
                            // already-read messages) still counts toward the
                            // delta here.
                            let mut delta = 0i64;
                            if add.contains(&SystemFlagBit::Seen) {
                                delta -= uids.len() as i64;
                            }
                            if remove.contains(&SystemFlagBit::Seen) {
                                delta += uids.len() as i64;
                            }
                            let count_changed = delta != 0
                                && folders.iter_mut().any(|f| {
                                    if f.id == mailbox {
                                        f.unread = (f.unread as i64 + delta).max(0) as u32;
                                        true
                                    } else {
                                        false
                                    }
                                });
                            if count_changed {
                                publish_folders(&folders, &account_id, cache, events).await;
                            }
                        }
                        Err(e) => {
                            let _ = events
                                .send(AccountEvent::StoreFlagsFailed {
                                    mailbox,
                                    uids,
                                    message: format!("Couldn't update message flags: {e}"),
                                })
                                .await;
                        }
                    }
                }
                AccountCommand::StoreKeywords { mailbox, uid, add, remove } => {
                    let Some(path) = mailbox.0.strip_prefix(&format!("{}:", account_id.0)).map(str::to_string) else {
                        continue;
                    };
                    // Same folder-handling contract as `StoreFlags`: the
                    // command races a folder switch (or the unified view),
                    // so SELECT the message's own folder when needed; the
                    // top of the loop re-selects the user's folder before
                    // the next IDLE wait.
                    if session_selected != mailbox {
                        session.select(&path).await?;
                        session_selected = mailbox.clone();
                    }
                    // Keywords are client-supplied atoms; drop anything that
                    // isn't one so a malformed atom can't be sent (servers
                    // are entitled to reject the whole STORE).
                    let add: Vec<String> = add.into_iter().filter(|k| valid_keyword_atom(k)).collect();
                    let remove: Vec<String> = remove.into_iter().filter(|k| valid_keyword_atom(k)).collect();
                    match store_raw_flags(&mut session, &[uid], &add, &remove).await {
                        Ok(()) => {
                            let patched = match cache_op(cache, {
                                let mailbox = mailbox.clone();
                                let add = add.clone();
                                let remove = remove.clone();
                                move |c| c.update_keywords(&mailbox, uid, &add, &remove)
                            })
                            .await
                            {
                                Some(Ok(patched)) => patched,
                                Some(Err(e)) => {
                                    tracing::warn!("failed to update cached keywords: {e}");
                                    false
                                }
                                None => false,
                            };
                            if patched {
                                emit_cached_messages(cache, &mailbox, events).await;
                            } else if mailbox == current_mailbox_id {
                                // No cache (or the uid fell outside the
                                // cached window): re-sync so the UI still
                                // sees the new keywords.
                                sync_mailbox(
                                    &mut session,
                                    &account_id,
                                    &current_mailbox_name,
                                    &current_mailbox_id,
                                    events,
                                    cache,
                                    &mut folders,
                                    Some(&session_selected),
                                    condstore,
                                    qresync,
                                )
                                .await?;
                                session_selected = current_mailbox_id.clone();
                            }
                        }
                        Err(e) => {
                            let _ = events.send(AccountEvent::Error(format!("Couldn't update message tags: {e}"))).await;
                        }
                    }
                }
                AccountCommand::StoreKeywordsMany { mailbox, uids, add, remove } => {
                    if uids.is_empty() {
                        continue;
                    }
                    let Some(path) = mailbox.0.strip_prefix(&format!("{}:", account_id.0)).map(str::to_string) else {
                        continue;
                    };
                    if session_selected != mailbox {
                        session.select(&path).await?;
                        session_selected = mailbox.clone();
                    }
                    let add: Vec<String> = add.into_iter().filter(|k| valid_keyword_atom(k)).collect();
                    let remove: Vec<String> = remove.into_iter().filter(|k| valid_keyword_atom(k)).collect();
                    match store_raw_flags(&mut session, &uids, &add, &remove).await {
                        Ok(()) => {
                            // Same all-or-resync contract as `StoreFlagsMany`:
                            // patch the cache for every uid, or resync once.
                            let all_patched = matches!(
                                cache_op(cache, {
                                    let mailbox = mailbox.clone();
                                    let uids = uids.clone();
                                    let add = add.clone();
                                    let remove = remove.clone();
                                    move |c| c.update_keywords_many(&mailbox, &uids, &add, &remove)
                                })
                                .await,
                                Some(Ok(true))
                            );
                            if all_patched {
                                emit_cached_messages(cache, &mailbox, events).await;
                            } else if mailbox == current_mailbox_id {
                                sync_mailbox(
                                    &mut session,
                                    &account_id,
                                    &current_mailbox_name,
                                    &current_mailbox_id,
                                    events,
                                    cache,
                                    &mut folders,
                                    Some(&session_selected),
                                    condstore,
                                    qresync,
                                )
                                .await?;
                                session_selected = current_mailbox_id.clone();
                            }
                        }
                        Err(e) => {
                            let _ = events.send(AccountEvent::Error(format!("Couldn't update message tags: {e}"))).await;
                        }
                    }
                }
                AccountCommand::PrefetchBodies => {
                    tracing::debug!("PrefetchBodies command received");
                    // Already handled by the automatic trigger after initial
                    // sync; this command is a no-op if prefetch is already
                    // running or has completed.
                }
            }
        }

        // --- folder-count STATUS drain (cooperative) ---
        // Runs to completion in this one iteration rather than a few calls
        // per IDLE cycle: a STATUS is a single round trip, so a whole pass is
        // cheap, whereas re-entering and tearing down IDLE around every few of
        // them cost two extra round trips per batch and kept the session
        // audibly busy for the entire pass - which is what a folder click had
        // to queue behind. `commands.is_empty()` is checked before every round
        // trip, so the longest a user action ever waits here is one in-flight
        // STATUS; the queue is resumed on the next iteration exactly where it
        // stopped. Deliberately ahead of the prefetch batch below: counts are
        // one round trip each and immediately visible in the sidebar, while
        // prefetch is minutes of bulk body downloading.
        if !counts_pending.is_empty() {
            let mut since_emit = 0usize;
            let mut dirty = false;
            while commands.is_empty() {
                let Some(mailbox_id) = counts_pending.pop_front() else { break };
                // The folder may have vanished from a re-list between being
                // queued and being drained; skip it rather than miscounting.
                let Some(index) = folders.iter().position(|m| m.id == mailbox_id) else { continue };
                let changed = refresh_folder_counts(&mut session, &mut folders[index], &account_id, condstore).await;
                dirty |= changed;
                if changed {
                    if mailbox_id == current_mailbox_id {
                        // This STATUS drain - not IDLE - is what caught the
                        // change: new mail can land while the session is
                        // SELECTed elsewhere (a prefetch batch, another
                        // command's folder), and a plain IDLE re-entered
                        // afterward never reports a change that happened
                        // before it started (see the module notes on
                        // `dirty_mailboxes`). Since the user is looking at
                        // this exact mailbox right now, don't wait for a
                        // folder switch to notice the dirty flag - resync it
                        // immediately so the message list and the sidebar
                        // count land together.
                        sync_mailbox(
                            &mut session,
                            &account_id,
                            &current_mailbox_name,
                            &current_mailbox_id,
                            events,
                            cache,
                            &mut folders,
                            Some(&session_selected),
                            condstore,
                            qresync,
                        )
                        .await?;
                        session_selected = current_mailbox_id.clone();
                    } else {
                        // Not on screen right now - the message cache (if
                        // any) no longer matches the server's count for this
                        // mailbox, so a later folder switch must not serve it
                        // from cache until a real sync runs.
                        dirty_mailboxes.insert(mailbox_id);
                    }
                }
                since_emit += 1;
                if since_emit >= COUNT_STATUS_BATCH {
                    since_emit = 0;
                    if dirty {
                        publish_folders(&folders, &account_id, cache, events).await;
                        dirty = false;
                    }
                }
            }
            if dirty {
                publish_folders(&folders, &account_id, cache, events).await;
            }
        }

        // --- background body prefetch batch ---
        // Run a small batch of body fetches between IDLE cycles when no user
        // commands are in flight. This is cooperative: every batch yields
        // back to IDLE, so user actions (message clicks, folder switches)
        // are never starved.
        let mut did_prefetch_work = false;
        if let Some(pf) = prefetch.as_mut() {
            if !pf.is_done() {
                // Check before starting any batch work (SELECT, envelope
                // fetch, body fetch) so a rapid stream of user clicks
                // never waits for even one IMAP round trip.
                if !commands.is_empty() {
                    continue;
                }

                // Resolve the current prefetch mailbox's folder path from the
                // folder list if we haven't yet.
                if pf.current_folder_name.is_empty() {
                    if let Some(mailbox) = folders.iter().find(|m| m.id == pf.mailboxes[pf.current]) {
                        pf.current_folder_name = pf.mailboxes[pf.current].0.strip_prefix(&format!("{}:", account_id.0)).unwrap_or(&mailbox.name).to_string();
                        pf.uidvalidity = mailbox.uidvalidity;
                    } else {
                        // Mailbox not found (deleted since list); skip it.
                        pf.advance();
                        continue;
                    }
                }

                // Fetch envelope UIDs for this mailbox if not done yet. The
                // query also asks for BODYSTRUCTURE so the body fetches below
                // can be text-parts-only rather than whole-message downloads
                // (see `fetch_body_partial`).
                if !pf.envelopes_fetched {
                    // Check before SELECT to avoid blocking the session if a
                    // user command arrived during the previous body fetch.
                    if !commands.is_empty() {
                        continue;
                    }
                    let folder_name = pf.current_folder_name.clone();
                    let mailbox_meta = session.select(&folder_name).await?;
                    session_selected = pf.mailboxes[pf.current].clone();
                    // The prefetch is the first SELECT most folders ever get
                    // (only the open folder and STATUS-dirty ones go through
                    // `sync_mailbox`), so it's the primary place their
                    // `highest_modseq` gets learned - CONDSTORE-enabled
                    // SELECTs report it in the OK response.
                    if let Some(f) = folders.iter_mut().find(|f| f.id == pf.mailboxes[pf.current]) {
                        if let Some(modseq) = mailbox_meta.highest_modseq {
                            f.highest_modseq = Some(modseq);
                        }
                    }
                    let fetch_from = mailbox_meta.exists.saturating_sub(INITIAL_FETCH_LIMIT - 1).max(1);
                    let seq_range = format!("{fetch_from}:*");
                    let fetches: Vec<_> = session.fetch(&seq_range, "(UID BODYSTRUCTURE)").await?.try_collect().await?;

                    // Collect UIDs, newest first.
                    let mut uids: Vec<Uid> = fetches.iter().filter_map(|f| f.uid.map(Uid)).collect();
                    uids.sort_by_key(|u| std::cmp::Reverse(u.0));

                    // Filter out already-cached bodies. One query for the whole
                    // envelope batch rather than a SELECT per UID - on a first
                    // sync of a big folder that's thousands of statements.
                    let have = cache_op(cache, {
                        let mailbox = pf.mailboxes[pf.current].clone();
                        let uidvalidity = pf.uidvalidity;
                        let uids = uids.clone();
                        move |c| c.has_bodies(&mailbox, &uids, uidvalidity)
                    })
                    .await
                    .and_then(|r| r.ok())
                    .unwrap_or_default();
                    uids.retain(|uid| !have.contains(uid));

                    // Remember each still-wanted uid's part structure (only
                    // the newest `INITIAL_FETCH_LIMIT`'s worth) so its body
                    // fetch skips attachments entirely.
                    pf.structures.clear();
                    for fetch in &fetches {
                        let (Some(uid), Some(structure)) = (fetch.uid.map(Uid), fetch.bodystructure()) else {
                            continue;
                        };
                        if uids.contains(&uid) {
                            pf.structures.insert(uid, crate::structure::parts_from_bodystructure(structure));
                        }
                    }

                    tracing::debug!(
                        mailbox = %pf.mailboxes[pf.current],
                        total = uids.len(),
                        "prefetch: queued UIDs for body download"
                    );
                    pf.pending_uids = uids;
                    pf.envelopes_fetched = true;
                    did_prefetch_work = true;
                }

                // Fetch up to PREFETCH_BATCH_SIZE bodies in **one** `UID
                // FETCH` round trip (see `fetch_bodies_batch`), yielding to
                // user commands between batches so they are never blocked for
                // more than one batch's transfer.
                if !pf.pending_uids.is_empty() {
                    // Check before starting the batch fetch.
                    if !commands.is_empty() {
                        continue;
                    }
                    let batch: Vec<Uid> = pf.pending_uids.drain(..pf.pending_uids.len().min(PREFETCH_BATCH_SIZE)).collect();
                    let fetched = match fetch_bodies_batch(
                        cache,
                        &mut session,
                        &pf.mailboxes[pf.current],
                        pf.uidvalidity,
                        &batch,
                        &pf.structures,
                    )
                    .await
                    {
                        Ok(fetched) => fetched,
                        Err(e) => {
                            tracing::warn!(?e, "prefetch: batch fetch failed");
                            0
                        }
                    };
                    if fetched > 0 {
                        tracing::debug!(
                            mailbox = %pf.mailboxes[pf.current],
                            fetched,
                            remaining = pf.pending_uids.len(),
                            "prefetch: batch complete"
                        );
                        did_prefetch_work = true;
                    }
                }

                // If all UIDs for this mailbox are done, advance to the next.
                if pf.pending_uids.is_empty() {
                    pf.advance();
                    if pf.is_done() {
                        tracing::info!("background body prefetch complete");
                    }
                }
            }
        }

        // Re-select the user's current mailbox before re-entering IDLE so the
        // next IDLE wait operates on the right folder. Only needed when
        // prefetch actually changed the selected mailbox (SELECT for envelope
        // fetch or body fetches). Skip if a user command is pending — the
        // command handler will SELECT the folder it needs, and the top-of-loop
        // check re-selects the user's folder anyway.
        if did_prefetch_work && commands.is_empty() {
            session.select(&current_mailbox_name).await?;
            session_selected = current_mailbox_id.clone();
        }
    }
}

/// Splits `uids` into one or more IMAP sequence-set strings (`1,3,5` or
/// `1:5,9:12`) that are safe to put on a single command line. Sorts and
/// dedups first (callers hand over `HashSet`-derived data), then range-
/// compresses contiguous runs into `a:b` pairs (RFC 3501 §6.4.8) and flushes
/// a chunk before its rendered length would exceed `UID_SET_BYTE_BUDGET` - so
/// a dense big folder collapses to one short `1:<n>` command while a sparse
/// UID space still chunks into lines servers will accept. Returns nothing
/// for an empty input; callers loop over zero chunks and send no command.
fn uid_set_chunks(uids: &[Uid]) -> Vec<String> {
    let mut sorted: Vec<u32> = uids.iter().map(|u| u.0).collect();
    sorted.sort_unstable();
    sorted.dedup();

    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut iter = sorted.into_iter().peekable();
    while let Some(first) = iter.next() {
        let mut run_end = first;
        while iter.peek().is_some_and(|&next| next == run_end.saturating_add(1)) {
            run_end = iter.next().expect("peeked value is present");
        }
        let token = if run_end > first { format!("{first}:{run_end}") } else { first.to_string() };
        if !current.is_empty() && current.len() + token.len() + 1 > UID_SET_BYTE_BUDGET {
            chunks.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push(',');
        }
        current.push_str(&token);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// Issues `STORE +FLAGS.SILENT` / `STORE -FLAGS.SILENT` for raw flag atoms
/// on `uids` (one or many) in the currently selected mailbox. `.SILENT` so the
/// server doesn't echo an untagged FETCH per affected message: the caller
/// already knows the resulting flag set (it applies the same add/remove to
/// its cached summary), and the next `sync_mailbox` re-reads the real flags
/// from the server regardless.
///
/// Add and remove are two separate STOREs because IMAP has no combined form;
/// an empty side is skipped rather than sent as an empty flag list, which
/// servers are entitled to reject.
async fn store_raw_flags(session: &mut Session<ImapStream>, uids: &[Uid], add: &[String], remove: &[String]) -> Result<()> {
    let uid_chunks = uid_set_chunks(uids);
    for (op, flags) in [('+', add), ('-', remove)] {
        if flags.is_empty() {
            continue;
        }
        let list = flags.join(" ");
        let query = format!("{op}FLAGS.SILENT ({list})");
        for uid_set in &uid_chunks {
            let _: Vec<_> = session.uid_store(uid_set, &query).await?.try_collect().await?;
        }
    }
    Ok(())
}

/// `store_raw_flags` for system flags: maps each `SystemFlagBit` to its IMAP
/// atom and delegates.
async fn store_flags(session: &mut Session<ImapStream>, uids: &[Uid], add: &[SystemFlagBit], remove: &[SystemFlagBit]) -> Result<()> {
    let add = add.iter().map(|f| f.as_imap_flag().to_string()).collect::<Vec<_>>();
    let remove = remove.iter().map(|f| f.as_imap_flag().to_string()).collect::<Vec<_>>();
    store_raw_flags(session, uids, &add, &remove).await
}

/// A keyword atom must be non-empty, not start with `\` (that's a flag), and
/// contain none of the characters RFC 3501 reserves for flag-list
/// punctuation: spaces, control characters, and `( ) { } % * " \`. Lookout's
/// own tag keys are sanitized into this shape at creation (see
/// `lookout_core::sanitize_tag_key`); this guard just keeps an arbitrary
/// atom from ever reaching the wire.
fn valid_keyword_atom(keyword: &str) -> bool {
    !keyword.is_empty()
        && !keyword.starts_with('\\')
        && keyword
            .chars()
            .all(|c| c.is_ascii_graphic() && !matches!(c, '(' | ')' | '{' | '}' | '%' | '*' | '"' | '\\'))
}

/// Moves `uids` (one or many) from the currently selected mailbox into the
/// account's mailbox with special-use role `role`, via IMAP MOVE (RFC 6851)
/// if the server advertises it, else COPY + STORE `\Deleted` + EXPUNGE.
async fn move_message_to_role(session: &mut Session<ImapStream>, folders: &[Mailbox], account_id: &AccountId, uids: &[Uid], role: MailboxRole, has_move: bool) -> Result<()> {
    let Some(target) = folders.iter().find(|m| m.role == role) else {
        return Err(Error::NoSuchFolder(role));
    };
    let Some(path) = target.id.0.strip_prefix(&format!("{}:", account_id.0)) else {
        return Ok(());
    };
    move_uids_to_path(session, uids, path, has_move).await
}

/// The IMAP move itself, shared by the role-based (`MoveMessages`) and
/// explicit-target (`MoveMessagesTo`) paths: one MOVE over the joined UID
/// set (RFC 6851) when the server advertises it, else COPY + STORE
/// `\Deleted` + EXPUNGE. `has_move` comes from the single post-login
/// `CAPABILITY` fetch, never a per-batch round trip.
async fn move_uids_to_path(session: &mut Session<ImapStream>, uids: &[Uid], path: &str, has_move: bool) -> Result<()> {
    let uid_chunks = uid_set_chunks(uids);
    if has_move {
        for uid_set in &uid_chunks {
            session.uid_mv(uid_set, path).await?;
        }
    } else {
        for uid_set in &uid_chunks {
            session.uid_copy(uid_set, path).await?;
        }
        for uid_set in &uid_chunks {
            let _: Vec<_> = session.uid_store(uid_set, "+FLAGS (\\Deleted)").await?.try_collect().await?;
        }
        // NB: expunges every \Deleted-flagged message in the currently
        // selected mailbox, not just this batch - a documented, accepted
        // simplification since nothing else in this crate ever sets \Deleted.
        let _: Vec<_> = session.expunge().await?.try_collect().await?;
    }
    Ok(())
}

/// Sends `msg` over SMTP, then best-effort `APPEND`s the raw message to the
/// account's Sent mailbox (if one was identified in `folders`) so it shows
/// up in the Sent view - IMAP has no server-side "file to Sent on submit"
/// the way JMAP's EmailSubmission does. Returns the Sent mailbox's id when
/// the APPEND actually landed, so the caller can resync it - archiving is
/// best-effort, but the resync must not run against a mailbox nothing was
/// appended to.
async fn send_message(
    config: &AccountConfig,
    credentials: &dyn CredentialProvider,
    session: &mut Session<ImapStream>,
    folders: &[Mailbox],
    msg: ComposedMessage,
) -> Result<Option<MailboxId>> {
    let (raw, _message_id, recipients) = build_raw_message(&msg);

    let smtp_credential = credentials.smtp_credential().await.map_err(Error::LoginFailed)?;
    send_smtp(&config.smtp, smtp_credential, &msg.from, &recipients, &raw).await?;

    let Some(sent) = folders.iter().find(|m| matches!(m.role, lookout_core::MailboxRole::Sent)) else {
        tracing::warn!("no Sent mailbox found; message was sent but not archived");
        return Ok(None);
    };
    let Some(path) = sent.id.0.strip_prefix(&format!("{}:", config.account_id.0)) else {
        return Ok(None);
    };
    if let Err(e) = session.append(path, Some("(\\Seen)"), None, raw.as_slice()).await {
        tracing::warn!("message was sent but APPEND to Sent failed: {e}");
        return Ok(None);
    }
    Ok(Some(sent.id.clone()))
}

/// The account's Drafts folder path from the latest folder list, or `None`
/// if it has no Drafts mailbox (yet).
fn drafts_path<'a>(folders: &'a [Mailbox], account_id: &AccountId) -> Option<&'a str> {
    let drafts = folders.iter().find(|m| matches!(m.role, MailboxRole::Drafts))?;
    drafts.id.0.strip_prefix(&format!("{}:", account_id.0))
}

/// Permanently removes every message in the *currently selected* mailbox -
/// the server side of `AccountCommand::EmptyMailbox`. `UID SEARCH ALL` +
/// STORE `\Deleted` + EXPUNGE, reusing the same wire pattern as
/// `purge_by_message_id`; the whole-mailbox `\Deleted` caveat from
/// `move_uids_to_path` is exactly the intent here, since nothing else in this
/// crate sets `\Deleted` inside the folder being emptied. Returns how many
/// messages were removed (0 when the folder is already empty, with no round
/// trips beyond the search).
async fn empty_mailbox(session: &mut Session<ImapStream>) -> Result<u32> {
    let uids = session.uid_search("ALL").await?;
    let count = uids.len() as u32;
    if uids.is_empty() {
        return Ok(count);
    }
    // `UID STORE 1:*` covers the whole selected mailbox in one short command
    // (RFC 3501 §6.4.8: `*` is the largest UID in use), where spelling out
    // every uid could exceed the server's command-line limit on a big folder.
    let _: Vec<_> = session.uid_store("1:*", "+FLAGS.SILENT (\\Deleted)").await?.try_collect().await?;
    let _: Vec<_> = session.expunge().await?.try_collect().await?;
    Ok(count)
}

/// Permanently removes every message in the *currently selected* mailbox
/// whose `Message-ID` header equals `message_id` (bare id, no brackets).
/// `UID SEARCH HEADER` + `\Deleted` + EXPUNGE rather than MOVE so it works
/// on servers without RFC 6851; the EXPUNGE-everything-`\Deleted` caveat
/// from `move_message_to_role` applies, and is harmless here because this
/// crate only ever sets `\Deleted` on the very uids it just flagged.
async fn purge_by_message_id(session: &mut Session<ImapStream>, message_id: &str) -> Result<()> {
    let uids: Vec<Uid> = session.uid_search(format!("HEADER Message-Id <{message_id}>")).await?.into_iter().map(Uid).collect();
    if uids.is_empty() {
        return Ok(());
    }
    for uid_set in &uid_set_chunks(&uids) {
        let _: Vec<_> = session.uid_store(uid_set, "+FLAGS.SILENT (\\Deleted)").await?.try_collect().await?;
    }
    let _: Vec<_> = session.expunge().await?.try_collect().await?;
    Ok(())
}

/// Saves `msg` as a draft: `APPEND`s the raw message to the account's
/// Drafts mailbox with `\Draft \Seen` flags (drafts must not show as
/// unread). With `replace`, any draft already stored under the same
/// `Message-ID` is purged first so autosaves update in place instead of
/// accumulating. Accounts without a Drafts mailbox get one `CREATE`d -
/// servers aren't required to pre-create it, and refusing to autosave on
/// such an account would be a worse surprise than a new folder. Leaves the
/// session SELECTed on the Drafts mailbox; the caller re-selects the
/// user's folder. Returns the draft's `Message-ID`.
async fn save_draft(session: &mut Session<ImapStream>, folders: &[Mailbox], account_id: &AccountId, msg: ComposedMessage, replace: bool) -> Result<String> {
    let path = match drafts_path(folders, account_id) {
        Some(path) => path.to_string(),
        None => {
            tracing::debug!("no Drafts mailbox in folder list; creating one");
            // Tolerate a CREATE failure: the folder may exist server-side
            // already (created by another client since our last LIST, or
            // excluded from it) - the SELECT below is the real test.
            if let Err(e) = session.create("Drafts").await {
                tracing::debug!("CREATE Drafts failed (continuing, it may already exist): {e}");
            }
            "Drafts".to_string()
        }
    };
    let (raw, message_id, _) = build_raw_message(&msg);
    session.select(&path).await?;
    if replace {
        purge_by_message_id(session, &message_id).await?;
    }
    session.append(&path, Some("(\\Draft \\Seen)"), None, raw.as_slice()).await?;
    Ok(message_id)
}

/// Deletes the draft stored under `message_id` from the account's Drafts
/// mailbox (a no-op when the account has no Drafts mailbox or the draft
/// isn't there). Leaves the session SELECTed on the Drafts mailbox; the
/// caller re-selects the user's folder.
async fn delete_draft(session: &mut Session<ImapStream>, folders: &[Mailbox], account_id: &AccountId, message_id: &str) -> Result<()> {
    let Some(path) = drafts_path(folders, account_id) else {
        return Ok(());
    };
    session.select(path).await?;
    purge_by_message_id(session, message_id).await
}

async fn login(config: &AccountConfig, credential: Credential) -> Result<(Session<ImapStream>, bool, bool, bool, bool)> {
    let stream = connect_tls(&config.imap.host, config.imap.port).await?;
    tracing::debug!("login: creating client, reading greeting");
    let mut client = async_imap::Client::new(stream);
    let greeting = client.read_response().await;
    tracing::debug!("login: greeting = {greeting:?}");

    tracing::debug!("login: authenticating as {}", config.imap.username);
    let session = match credential {
        Credential::OAuth2AccessToken(token) => {
            let authenticator = XOAuth2Authenticator::new(&config.imap.username, token);
            client.authenticate("XOAUTH2", authenticator).await
        }
        Credential::Password(password) => client.login(&config.imap.username, password).await,
    };
    tracing::debug!("login: auth attempt complete, ok = {}", session.is_ok());
    let mut session = session.map_err(|(e, _client)| Error::Imap(e))?;

    // Capabilities are stable for the connection's lifetime; read them once
    // after auth so every move batch doesn't re-issue a `CAPABILITY` round
    // trip to re-answer the same question.
    let capabilities = session.capabilities().await?;
    let has_move = capabilities.has_str("MOVE");
    let has_list_status = capabilities.has_str("LIST-STATUS");
    tracing::debug!(has_move, has_list_status, "cached server capabilities after login");

    // CONDSTORE (RFC 7162): enables the per-mailbox `highest_modseq` bookkeeping
    // that incremental sync (`CHANGEDSINCE`) builds on. `ENABLE` applies to the
    // whole connection, so one post-auth command covers every later SELECT -
    // once enabled, each SELECT's OK response carries the folder's current
    // HIGHESTMODSEQ (parsed into `Mailbox::highest_modseq` by async-imap).
    // QRESYNC servers must support CONDSTORE too (RFC 7162 §4), so advertising
    // either capability is enough to ask. A rejection is not fatal: we just
    // fall back to full syncs - and crucially, `condstore` then reads *false*,
    // because a server that refuses `ENABLE CONDSTORE` must never be sent
    // `CHANGEDSINCE` or asked for `HIGHESTMODSEQ` (both are protocol errors on
    // a non-CONDSTORE connection).
    //
    // QRESYNC (RFC 7162 §4): when the server advertises it, also `ENABLE`
    // it so the delta sync's `SELECT ... (QRESYNC (modseq))` is legal. That
    // SELECT reports expunged UIDs via `VANISHED`, replacing the delta path's
    // `UID SEARCH ALL` membership pass. A rejection is a non-fatal downgrade:
    // `qresync` reads *false* and the delta path keeps using the search.
    // RFC 7162 §4 requires QRESYNC servers to support CONDSTORE too, so a
    // server that accepts QRESYNC but refuses CONDSTORE is a protocol
    // violation - `condstore` must stay false in that case (VANISHED and
    // `CHANGEDSINCE` are both CONDSTORE-only), so QRESYNC is only offered
    // once CONDSTORE actually enabled.
    let mut qresync = capabilities.has_str("QRESYNC");
    let mut condstore = capabilities.has_str("CONDSTORE") || qresync;
    if condstore {
        let enabled = if qresync { "ENABLE CONDSTORE QRESYNC" } else { "ENABLE CONDSTORE" };
        match session.run_command_and_check_ok(enabled).await {
            Ok(()) => tracing::debug!(qresync, "CONDSTORE enabled for connection"),
            Err(e) => {
                tracing::debug!("server rejected ENABLE {enabled}, continuing without incremental sync: {e}");
                condstore = false;
                qresync = false;
            }
        }
    }
    Ok((session, has_move, condstore, has_list_status, qresync))
}

/// Builds a `Mailbox` from one LIST response entry, minus the count fields
/// (filled from the STATUS items a LIST-STATUS response pairs with it, or by
/// a later SELECT/STATUS). `None` for a `\NoSelect` name - those aren't
/// selectable mailboxes and get no STATUS items (RFC 5819 §2).
fn mailbox_from_list_parts(account_id: &AccountId, name: &str, delimiter: Option<&str>, attributes: &[NameAttribute]) -> Option<Mailbox> {
    let attrs: Vec<String> = attributes.iter().map(|a| format!("{a:?}")).collect();
    if attrs.iter().any(|a| a.contains("NoSelect")) {
        return None;
    }
    let delimiter = delimiter.and_then(|d| d.chars().next()).unwrap_or('/');
    let display_name = name.rsplit(delimiter).next().unwrap_or(name).to_string();
    let role = role_from_special_use(&attrs, &display_name);

    Some(Mailbox {
        id: MailboxId::new(account_id, name),
        account_id: account_id.clone(),
        name: display_name,
        parent: None, // Populated by the caller from the flat list via delimiter splitting (UI-layer concern).
        delimiter,
        role,
        uidvalidity: UidValidity(0), // Filled in by sync_mailbox() once the folder is SELECTed, or by STATUS below.
        uidnext: 0,
        highest_modseq: None,
        total: 0,
        unread: 0,
        flags: attrs,
        subscribed: true,
    })
}

/// Applies one mailbox's `STATUS` items to `mailbox` - the fields
/// `refresh_folder_counts` reads from a `STATUS` reply, mapped the same way.
fn apply_status_items(mailbox: &mut Mailbox, status: &[StatusAttribute]) {
    for item in status {
        match item {
            StatusAttribute::Messages(n) => mailbox.total = *n,
            StatusAttribute::Unseen(n) => mailbox.unread = *n,
            StatusAttribute::UidNext(n) => mailbox.uidnext = *n,
            StatusAttribute::UidValidity(n) => mailbox.uidvalidity = UidValidity(*n),
            StatusAttribute::HighestModSeq(n) => mailbox.highest_modseq = Some(*n),
            StatusAttribute::Recent(_) => {}
            _ => {}
        }
    }
}

/// The LIST-STATUS (RFC 5819) path: one `LIST "" "*" RETURN (STATUS ...)`
/// round trip whose response stream pairs each LIST entry with a `STATUS`
/// response carrying that mailbox's items - every folder's counts (and its
/// `uidvalidity`/`uidnext`, normally only learnable per-SELECT) arrive in one
/// command instead of ~1 STATUS round trip per folder. The server skips the
/// STATUS response for `\NoSelect` mailboxes (RFC 5819 §2), so those keep
/// LIST-only defaults - the same way `refresh_folder_counts` leaves a failed
/// STATUS alone.
async fn list_mailboxes_with_status(session: &mut Session<ImapStream>, account_id: &AccountId, condstore: bool) -> Result<Vec<Mailbox>> {
    let query = format!("LIST \"\" \"*\" RETURN (STATUS {})", status_count_query(condstore));
    session.run_command(query).await?;
    let mut mailboxes: Vec<Mailbox> = Vec::new();
    let mut statuses: Vec<(String, Vec<StatusAttribute>)> = Vec::new();
    while let Some(response) = session.read_response().await? {
        match response.parsed() {
            Response::MailboxData(MailboxDatum::List { name_attributes, delimiter, name }) => {
                if let Some(mailbox) = mailbox_from_list_parts(account_id, name, delimiter.as_deref(), name_attributes) {
                    mailboxes.push(mailbox);
                }
            }
            Response::MailboxData(MailboxDatum::Status { mailbox, status }) => {
                statuses.push((mailbox.to_string(), status.clone()));
            }
            Response::Done { status: Status::Ok, .. } => break,
            Response::Done {
                status: done, code, information, ..
            } => {
                let message = format!("code: {code:?}, info: {information:?}");
                return Err(Error::Imap(match done {
                    Status::Bad => async_imap::error::Error::Bad(message),
                    _ => async_imap::error::Error::No(message),
                }));
            }
            _ => {}
        }
    }
    for (mailbox_name, status) in statuses {
        if let Some(mailbox) = mailboxes.iter_mut().find(|m| m.name == mailbox_name) {
            apply_status_items(mailbox, &status);
        }
    }
    Ok(mailboxes)
}

/// Lists the account's mailboxes. On a LIST-STATUS (RFC 5819) connection the
/// `STATUS (MESSAGES UNSEEN UIDNEXT UIDVALIDITY[ HIGHESTMODSEQ])` return
/// option rides the single LIST round trip, so the counts (and
/// `uidvalidity`/`uidnext`) for every folder arrive in one response stream
/// instead of ~1 STATUS round trip per folder. Returns `counts_supplied` =
/// whether the LIST answered the counts; when false the caller queues the
/// per-folder STATUS drain as before. Any server rejection - or a response
/// that can't be decoded - falls back to the plain LIST and the drain.
async fn list_mailboxes(session: &mut Session<ImapStream>, account_id: &AccountId, condstore: bool, has_list_status: bool) -> Result<(Vec<Mailbox>, bool)> {
    if has_list_status {
        match list_mailboxes_with_status(session, account_id, condstore).await {
            Ok(mailboxes) => return Ok((mailboxes, true)),
            Err(e) => {
                tracing::debug!("LIST RETURN (STATUS) failed, falling back to the per-folder STATUS drain: {e}");
            }
        }
    }
    let names: Vec<_> = session.list(Some(""), Some("*")).await?.try_collect().await?;
    let mut mailboxes = Vec::with_capacity(names.len());
    for name in &names {
        if let Some(mailbox) = mailbox_from_list_parts(account_id, name.name(), name.delimiter(), name.attributes()) {
            mailboxes.push(mailbox);
        }
    }
    Ok((mailboxes, false))
}

/// The STATUS data items a folder-count refresh asks for. `HIGHESTMODSEQ`
/// (RFC 7162 §3.2.1) rides along on CONDSTORE connections - it's the one
/// pass that visits every folder, so folders that never get SELECTed still
/// learn a modseq from it - but is a protocol error on servers that don't
/// advertise the capability, hence the per-connection selection.
fn status_count_query(condstore: bool) -> &'static str {
    if condstore {
        "(MESSAGES UNSEEN UIDNEXT UIDVALIDITY HIGHESTMODSEQ)"
    } else {
        "(MESSAGES UNSEEN UIDNEXT UIDVALIDITY)"
    }
}

/// One folder's best-effort STATUS count refresh (RFC 3501 §6.3.10): fills
/// `total`/`unread` (and, for free, `uidnext`/`uidvalidity`) from the server
/// and reports whether anything changed. Never fatal: a folder whose STATUS
/// fails (deleted since LIST, or a server that rejects the command) keeps
/// its LIST-only defaults, so unread counts degrade gracefully on servers
/// without a useful STATUS. Note the crate's `Mailbox` type reports the
/// STATUS `unseen` as a *count*, unlike SELECT's "sequence number of the
/// first unseen message", so it maps straight onto `Mailbox::unread`.
///
/// When the server supports CONDSTORE, the query also asks for
/// `HIGHESTMODSEQ` (RFC 7162 §3.2.1: a CONDSTORE server MUST support it as
/// a STATUS data item) - the STATUS drain is the one pass that touches every
/// folder, so it's how folders that never get SELECTed still learn a modseq.
async fn refresh_folder_counts(session: &mut Session<ImapStream>, folder: &mut Mailbox, account_id: &AccountId, condstore: bool) -> bool {
    let Some(path) = folder.id.0.strip_prefix(&format!("{}:", account_id.0)) else {
        return false;
    };
    // HIGHESTMODSEQ is only legal to *ask* for when the capability is
    // advertised; on a non-CONDSTORE server the data item is a protocol
    // error, so the query is built per-connection.
    let query = status_count_query(condstore);
    match session.status(path, query).await {
        Ok(meta) => {
            let mut changed = meta.exists != folder.total || meta.unseen.unwrap_or(0) != folder.unread;
            folder.total = meta.exists;
            if let Some(unseen) = meta.unseen {
                folder.unread = unseen;
            }
            if let Some(next) = meta.uid_next {
                folder.uidnext = next;
            }
            if let Some(validity) = meta.uid_validity {
                folder.uidvalidity = UidValidity(validity);
            }
            if meta.highest_modseq.is_some() {
                changed |= folder.highest_modseq != meta.highest_modseq;
                folder.highest_modseq = meta.highest_modseq;
            }
            changed
        }
        Err(e) => {
            tracing::debug!(mailbox = %folder.id, "STATUS failed for {}, leaving LIST-only defaults: {e}", folder.id);
            false
        }
    }
}

/// The order the main loop refreshes folder counts in: the folder the user is
/// looking at first, then the Inbox, then everything else in list order. The
/// drain is interruptible and a large account's pass takes a while, so what
/// matters is that the counts a user can actually see land first - draining in
/// list order (or, worse, from the back) leaves the open folder's count until
/// last on exactly the accounts where the pass is slowest.
fn queue_folder_counts(folders: &[Mailbox], current: &MailboxId) -> VecDeque<MailboxId> {
    let mut queue: VecDeque<MailboxId> = VecDeque::with_capacity(folders.len());
    let rank = |m: &Mailbox| {
        if m.id == *current {
            0
        } else if matches!(m.role, MailboxRole::Inbox) {
            1
        } else {
            2
        }
    };
    for wanted in 0..=2 {
        queue.extend(folders.iter().filter(|m| rank(m) == wanted).map(|m| m.id.clone()));
    }
    queue
}

/// Copies the count fields a `LIST` can't report (`total`, `unread`, and the
/// `uidnext`/`uidvalidity` that come free with a STATUS) from a previously
/// Copies the count fields a `LIST` can't report (`total`, `unread`, and the
/// `uidnext`/`uidvalidity` that come free with a STATUS) from a previously
/// known folder list onto a freshly listed one, matching by id. A folder
/// list that arrived with counts already (the LIST-STATUS path, RFC 5819)
/// must keep them - its values are authoritative, and overwriting a real 0
/// (a mailbox emptied since the last run) with a stale old count would undo
/// the very freshness the one-round-trip list exists to provide - so with
/// `list_supplied_counts` only the gaps are filled: a `highest_modseq` the
/// server didn't report.
fn carry_counts_forward(folders: &mut [Mailbox], known: &[Mailbox], list_supplied_counts: bool) {
    for folder in folders {
        if let Some(known) = known.iter().find(|m| m.id == folder.id) {
            if list_supplied_counts {
                // A re-list can't always report a modseq either; carry the
                // last learned one so the persisted folder list never loses
                // its incremental-sync baseline just because LIST ran.
                if folder.highest_modseq.is_none() {
                    folder.highest_modseq = known.highest_modseq;
                }
                continue;
            }
            folder.total = known.total;
            folder.unread = known.unread;
            folder.uidnext = known.uidnext;
            folder.uidvalidity = known.uidvalidity;
            // A re-list can't report a modseq either; carry the last learned
            // one so the persisted folder list never loses its incremental-
            // sync baseline just because LIST ran.
            folder.highest_modseq = known.highest_modseq;
        }
    }
}

/// The LIST-STATUS (RFC 5819) counterpart of the STATUS drain's change
/// detection: marks every mailbox whose `total`/`unread`/`highest_modseq`
/// moved against `previous` (the pre-list state), so a later folder switch
/// can't serve a stale cached envelope list through the cache-hit shortcut.
/// Returns whether the open folder's counts moved - the caller decides what
/// to resync, mirroring the drain's immediate resync of the folder on screen.
fn mark_count_changes(previous: &[Mailbox], listed: &[Mailbox], current: &MailboxId, dirty: &mut HashSet<MailboxId>) -> bool {
    let mut open_changed = false;
    for mailbox in listed {
        let Some(old) = previous.iter().find(|m| m.id == mailbox.id) else { continue };
        let changed = old.total != mailbox.total || old.unread != mailbox.unread || old.highest_modseq != mailbox.highest_modseq;
        if !changed {
            continue;
        }
        if mailbox.id == *current {
            open_changed = true;
        } else {
            dirty.insert(mailbox.id.clone());
        }
    }
    open_changed
}

/// Persists the folder list to the cache and emits it to the UI - the one
/// place a mutated `folders` becomes visible. Every count update funnels
/// through here so the on-disk copy and the sidebar can never disagree.
async fn publish_folders(folders: &[Mailbox], account_id: &AccountId, cache: &CacheHandle, events: &async_channel::Sender<AccountEvent>) {
    if let Some(Err(e)) = cache_op(cache, {
        let account_id = account_id.clone();
        let owned = folders.to_vec();
        move |c| c.replace_mailboxes(&account_id, &owned)
    })
    .await
    {
        tracing::warn!("failed to cache mailbox list: {e}");
    }
    let _ = events.send(AccountEvent::FoldersUpdated(folders.to_vec())).await;
}

/// The re-list step shared by the Refresh command and the move/draft-create
/// paths: `list_mailboxes` + cache + a `FoldersUpdated` emit, and re-queue
/// every folder's STATUS count for the main loop's cooperative drain. The
/// drain runs these one round trip at a time, yielding to the command queue
/// before each, so these paths never block the session on a whole
/// STATUS-per-folder pass. On a LIST-STATUS (RFC 5819) connection the LIST
/// already carried every folder's counts, so nothing is queued and the
/// count-change detection the drain would have done runs inline instead.
///
/// The re-list resets every folder to its LIST-only zero counts, so the
/// counts already learned are carried across by id - otherwise a Refresh (or
/// any message move) would visibly blank the whole sidebar until the new pass
/// caught up.
///
/// Returns whether the open folder's counts moved at this re-list (only
/// meaningful on the LIST-STATUS path); most callers sync the open folder
/// right after the re-list anyway, and the two that don't (send, draft-create)
/// resync it on `true` - the drain's immediate-resync counterpart.
// The extra `has_list_status`/`dirty_mailboxes` arguments are what let the
// LIST-STATUS path skip the whole STATUS drain, so the count is worth it.
#[allow(clippy::too_many_arguments)]
async fn relist_folders(
    session: &mut Session<ImapStream>,
    folders: &mut Vec<Mailbox>,
    counts_pending: &mut VecDeque<MailboxId>,
    account_id: &AccountId,
    current: &MailboxId,
    cache: &CacheHandle,
    events: &async_channel::Sender<AccountEvent>,
    condstore: bool,
    has_list_status: bool,
    dirty_mailboxes: &mut HashSet<MailboxId>,
) -> Result<bool> {
    let previous = has_list_status.then(|| folders.clone());
    let (mut relisted, list_counts_supplied) = list_mailboxes(session, account_id, condstore, has_list_status).await?;
    carry_counts_forward(&mut relisted, folders, list_counts_supplied);
    let mut open_changed = false;
    if list_counts_supplied {
        if let Some(previous) = &previous {
            open_changed = mark_count_changes(previous, &relisted, current, dirty_mailboxes);
        }
    }
    *folders = relisted;
    *counts_pending = if list_counts_supplied { VecDeque::new() } else { queue_folder_counts(folders, current) };
    publish_folders(folders, account_id, cache, events).await;
    Ok(open_changed)
}

/// Quotes an IMAP search criterion's argument per RFC 3501 §9 (quoted string):
/// the value is wrapped in double quotes and any embedded `"` or `\` escaped
/// with a backslash, so arbitrary user text can never break out of the string
/// and inject search grammar.
fn imap_quote(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

/// The IMAP `SEARCH` fallback for one mailbox: `UID SEARCH TEXT "<query>"`
/// returns the matching uids, then a `UID FETCH` of those uids' envelopes
/// yields the same `EmailSummary` shape the local index produces. `TEXT`
/// searches headers and body together (subject, addresses, message text) -
/// the same surface the FTS index covers. An empty match set is a valid
/// answer; `sync_mailbox`'s whole-folder strategy is not used here because
/// the server already did the filtering (and re-fetching a whole folder to
/// filter locally would defeat the fallback's purpose).
async fn search_mailbox(session: &mut Session<ImapStream>, account_id: &AccountId, mailbox: &MailboxId, query: &str) -> Result<Vec<EmailSummary>> {
    let search_query = format!("TEXT {}", imap_quote(query));
    let uids = session.uid_search(&search_query).await?;
    if uids.is_empty() {
        return Ok(Vec::new());
    }
    // A sorted UID set gives the fetch (and the summaries) a stable order
    // rather than the HashSet's arbitrary iteration order; `uid_set_chunks`
    // preserves it across the chunked `UID FETCH`es a large match needs.
    let mut uid_list: Vec<Uid> = uids.into_iter().map(Uid).collect();
    uid_list.sort_by_key(|u| u.0);
    let mut fetches: Vec<_> = Vec::new();
    for uid_set in &uid_set_chunks(&uid_list) {
        let chunk: Vec<_> = session
            .uid_fetch(uid_set, "(UID FLAGS ENVELOPE RFC822.SIZE INTERNALDATE BODYSTRUCTURE)")
            .await?
            .try_collect()
            .await?;
        fetches.extend(chunk);
    }
    let mut messages: Vec<EmailSummary> = fetches.iter().filter_map(|f| summary_from_fetch(mailbox, f)).collect();
    let keys = lookout_core::thread::compute_thread_keys(&messages);
    for msg in &mut messages {
        if let Some(key) = keys.get(&msg.uid) {
            msg.thread_key = key.clone();
        }
    }
    tracing::debug!(account = %account_id, mailbox = %mailbox, count = messages.len(), "mailbox search matched");
    Ok(messages)
}

/// SELECTs `folder_path` with the RFC 7162 §3.2.9.2 QRESYNC parameter on a
/// connection where `ENABLE QRESYNC` succeeded, asking the server to report -
/// via untagged `VANISHED` responses - which of the client's *known* UIDs
/// (`known_set`, spelling out the cached set) have been expunged since
/// `modseq`. Returns the expunged UIDs. Once the server has the known list,
/// it does the expunge diff itself, which is what makes this the replacement
/// for the delta path's `UID SEARCH ALL` membership pass.
///
/// `known_set` must fit on one command line: the caller renders it through
/// [`uid_set_chunks`] and only invokes this when it comes back as a single
/// chunk (a dense folder compresses to `1:<n>`; only a pathological sparse
/// set exceeds the budget, in which case the caller falls back to the
/// `UID SEARCH` delta instead). A server whose UIDVALIDITY doesn't match the
/// parameter treats the client as knowing nothing (an empty VANISHED set,
/// RFC 7162 §3.2.9.2) - the caller's `cached ∪ changed − vanished` diff then
/// simply keeps every cached row for one sync, and the next full-sync pass
/// (or a UIDVALIDITY mismatch elsewhere) corrects it, the same way a stale
/// baseline only ever widens a delta.
///
/// The response stream also carries the untagged `EXISTS`/`FLAGS`/OK-code
/// responses that accompany any SELECT; they're drained and discarded here -
/// the folder's actual counts come from the fetch the caller already ran.
/// Any `Err` means the caller should downgrade to the plain CONDSTORE delta.
async fn select_with_qresync(
    session: &mut Session<ImapStream>,
    folder_path: &str,
    uidvalidity: UidValidity,
    modseq: u64,
    known_set: &str,
) -> Result<HashSet<Uid>> {
    // `SELECT` goes through `run_command` rather than `Session::select` -
    // async-imap's select only supports the `(CONDSTORE)` parameter, and
    // QRESYNC's expunge reporting is exactly what this session needs. The
    // raw response loop mirrors `list_mailboxes_with_status`.
    let query = format!("SELECT {} (QRESYNC ({} {modseq} {known_set}))", imap_quote(folder_path), uidvalidity.0);
    session.run_command(query).await?;
    let mut vanished = HashSet::new();
    while let Some(response) = session.read_response().await? {
        match response.parsed() {
            Response::Vanished { uids, .. } => {
                for range in uids {
                    // The server reports expunges as a sequence set of UID
                    // ranges; expand each range into the wanted set. Bounded
                    // by what was actually expunged, never the folder size.
                    vanished.extend((*range.start()..=*range.end()).map(Uid));
                }
            }
            Response::Done { status: Status::Ok, .. } => break,
            Response::Done { status, code, information, .. } => {
                let message = format!("code: {code:?}, info: {information:?}");
                return Err(Error::Imap(match status {
                    Status::Bad => async_imap::error::Error::Bad(message),
                    _ => async_imap::error::Error::No(message),
                }));
            }
            _ => {}
        }
    }
    Ok(vanished)
}

// The extra `session_selected`/`qresync` arguments are what let the IDLE-wake
// path skip its re-SELECT and use the QRESYNC SELECT, so the count is worth it
// for the session-critical helper.
#[allow(clippy::too_many_arguments)]
async fn sync_mailbox(
    session: &mut Session<ImapStream>,
    account_id: &AccountId,
    folder_path: &str,
    mailbox_id: &MailboxId,
    events: &async_channel::Sender<AccountEvent>,
    cache: &CacheHandle,
    folders: &mut Vec<Mailbox>,
    session_selected: Option<&MailboxId>,
    condstore: bool,
    qresync: bool,
) -> Result<()> {
    let is_sent = folders.iter().find(|f| f.id == *mailbox_id).is_some_and(|f| matches!(f.role, MailboxRole::Sent));
    // `None` = a fresh session with nothing SELECTed yet (the connect-time
    // INBOX sync). Otherwise, when the session is already on this folder -
    // the IDLE wake path guarantees it via the pre-IDLE re-select, and
    // `FETCH 1:*` on the selected folder doesn't require being re-selected -
    // skip the SELECT: it would be a round trip on every wake that returns
    // nothing the fetch below doesn't already imply. Its `uidvalidity` is
    // then taken from the folder list, which the SELECT/STATUS paths keep
    // fresh (see the pre-IDLE re-select's write-back above).
    let already_selected = session_selected.is_some_and(|selected| selected == mailbox_id);
    let mailbox_meta = if already_selected { None } else { Some(session.select(folder_path).await?) };
    let uidvalidity = mailbox_meta
        .as_ref()
        .map(|meta| UidValidity(meta.uid_validity.unwrap_or(0)))
        .or_else(|| folders.iter().find(|f| f.id == *mailbox_id).map(|f| f.uidvalidity))
        .unwrap_or(UidValidity(0));
    // The display list shows every message in the folder, so every sync
    // still learns the full current UID set: any window - UID or sequence -
    // silently drops the older mail that large folders like Gmail's All Mail
    // exist to show (the account-global UID counter makes a UID window miss
    // still-present messages outright). But `ENVELOPE`/`BODYSTRUCTURE` never
    // change once a message exists - only `FLAGS` does - so a UID already
    // cached under the mailbox's current `uidvalidity` doesn't need either
    // refetched, just its flags refreshed.
    //
    // On the IDLE-wake path (this sync skipped its SELECT) with CONDSTORE
    // enabled and a baseline modseq known, the flag refresh can itself be a
    // delta: `FETCH 1:* (UID FLAGS MODSEQ) (CHANGEDSINCE <baseline>)` (RFC
    // 7162 §3.2.5.1) returns only messages whose flags changed since the
    // baseline - a steady-state wake costs O(changed) instead of O(mailbox).
    // New arrivals are reported too (their modseq exceeds the baseline), so
    // the delta doubles as the new-mail check. A stale baseline is safe: it
    // only makes the delta wider, never wrong.
    //
    // The one thing `CHANGEDSINCE` cannot report is an expunge - a deleted
    // message is no longer there to be fetched. On a QRESYNC connection the
    // `SELECT (QRESYNC ...)` below reports expunges directly via `VANISHED`;
    // otherwise they're discovered by UID-set diff, and the delta path
    // separately learns the full UID set with `UID SEARCH ALL` - a bare list
    // of numbers, far cheaper than the full flag fetch it replaces, and it
    // doubles as the folder's exists count. The full path needs no extra
    // pass - the full flag fetch *is* the membership set.
    let cached: HashMap<Uid, EmailSummary> = cache_op(cache, {
        let mailbox_id = mailbox_id.clone();
        move |c| c.load_messages_by_uid(&mailbox_id, uidvalidity)
    })
    .await
    .and_then(|r| r.ok())
    .unwrap_or_default();
    let baseline = folders.iter().find(|f| f.id == *mailbox_id).and_then(|f| f.highest_modseq);
    let delta = already_selected && condstore && baseline.is_some();
    let flag_fetches: Vec<_> = if delta {
        // The `MODSEQ` data item rides along so the fetch's responses carry
        // each message's modseq - the baseline can then advance without an
        // extra SELECT/STATUS round trip (see the folder update below).
        let bound = baseline.expect("delta requires a baseline, checked above");
        session.fetch("1:*", &format!("(UID FLAGS MODSEQ) (CHANGEDSINCE {bound})")).await?.try_collect().await?
    } else {
        session.fetch("1:*", "(UID FLAGS)").await?.try_collect().await?
    };
    // The full current UID set. On the plain-CONDSTORE delta it comes from
    // `UID SEARCH ALL`, run *after* the fetch: a message arriving between the
    // two lands in both (its modseq exceeds the baseline, so the fetch
    // reports it too), while one expunged between the two is caught by the
    // search. The other ordering would let an arrival slip through both
    // passes and vanish from this sync's output until the next wake.
    //
    // With QRESYNC the SELECT below replaces that search: the server diffs
    // the client's known (cached) UID set against its own and reports which
    // are gone via `VANISHED`, so `present = cached ∪ changed − vanished` -
    // no full-UID-set round trip, and the ordering is the same fetch-first,
    // which closes the expunge window exactly as the search did (the SELECT
    // reports every expunge since the baseline, including one that raced in
    // between). An arrival that races in between the fetch and the SELECT
    // isn't in this sync's output, but its modseq exceeds the conservative
    // baseline this sync keeps (see the folder update below), so the next
    // wake's `CHANGEDSINCE` reports it - never permanently lost.
    let present_uids: HashSet<Uid> = if delta && qresync {
        let bound = baseline.expect("QRESYNC delta requires a baseline, checked above");
        let known_uids: Vec<Uid> = cached.keys().copied().collect();
        let known = uid_set_chunks(&known_uids);
        // The known list must fit one SELECT command line (uid_set_chunks
        // returns multiple chunks only once the byte budget is exhausted);
        // a set too large for one line falls back to the UID SEARCH delta.
        if known.len() == 1 {
            match select_with_qresync(session, folder_path, uidvalidity, bound, &known[0]).await {
                Ok(vanished) => {
                    let mut present: HashSet<Uid> = cached.keys().copied().collect();
                    present.extend(flag_fetches.iter().filter_map(|f| f.uid.map(Uid)));
                    for &uid in &vanished {
                        present.remove(&uid);
                    }
                    present
                }
                Err(e) => {
                    tracing::debug!("QRESYNC SELECT failed for {folder_path}, falling back to the UID SEARCH delta: {e}");
                    session.uid_search("ALL").await?.into_iter().map(Uid).collect()
                }
            }
        } else {
            session.uid_search("ALL").await?.into_iter().map(Uid).collect()
        }
    } else if delta {
        session.uid_search("ALL").await?.into_iter().map(Uid).collect()
    } else {
        flag_fetches.iter().filter_map(|f| f.uid.map(Uid)).collect()
    };

    let mut messages: Vec<EmailSummary> = Vec::with_capacity(flag_fetches.len() + cached.len());
    for fetch in &flag_fetches {
        let Some(uid) = fetch.uid.map(Uid) else { continue };
        if let Some(cached_summary) = cached.get(&uid) {
            let (flags, keywords) = flags_from_fetch(fetch);
            let mut msg = cached_summary.clone();
            msg.flags = flags;
            msg.keywords = keywords;
            messages.push(msg);
        }
    }
    if delta {
        // Cached messages the delta didn't report are unchanged - carry them
        // over as-is, or the diff below would treat them as expunged. New
        // arrivals are everything the membership set has that the cache
        // doesn't (on the delta path that's authoritative, not the fetch).
        let changed: HashSet<Uid> = flag_fetches.iter().filter_map(|f| f.uid.map(Uid)).collect();
        let mut untouched: Vec<&EmailSummary> = cached.values().filter(|m| !changed.contains(&m.uid)).collect();
        untouched.sort_by_key(|m| m.uid);
        messages.extend(untouched.into_iter().cloned());
        // Drop anything the membership search no longer sees - a message
        // expunged between the delta fetch and the search would otherwise
        // linger as a phantom row in this sync's output (and the cache) until
        // the next wake's membership pass.
        messages.retain(|m| present_uids.contains(&m.uid));
    }
    let new_uids: Vec<Uid> = {
        let cached_uids: HashSet<Uid> = cached.keys().copied().collect();
        present_uids.difference(&cached_uids).copied().collect()
    };
    // Full `ENVELOPE`/`BODYSTRUCTURE` fetch, but only for UIDs this mailbox
    // hasn't cached yet - new arrivals, or the first sync since a
    // `UIDVALIDITY` change invalidated everything cached. `BODYSTRUCTURE`
    // rides along so `summary_from_fetch` can fill in the part structure /
    // `has_attachment` without any body fetch - it's what lets opening a
    // message download only its text parts.
    if !new_uids.is_empty() {
        // An empty pre-sync cache means this is the folder's first sync (or a
        // UIDVALIDITY change wiped everything cached), so the new set *is* the
        // whole folder - `1:*` then covers it in one short command line where
        // spelling out every uid could exceed the server's command-line limit
        // (the same `1:*` the flag fetch above already uses). Otherwise the
        // new arrivals are chunked into server-safe sequence-set lines.
        let fetches: Vec<_> = if cached.is_empty() {
            session
                .uid_fetch("1:*", "(UID FLAGS ENVELOPE RFC822.SIZE INTERNALDATE BODYSTRUCTURE)")
                .await?
                .try_collect()
                .await?
        } else {
            let mut fetches = Vec::new();
            for uid_set in &uid_set_chunks(&new_uids) {
                let chunk: Vec<_> = session
                    .uid_fetch(uid_set, "(UID FLAGS ENVELOPE RFC822.SIZE INTERNALDATE BODYSTRUCTURE)")
                    .await?
                    .try_collect()
                    .await?;
                fetches.extend(chunk);
            }
            fetches
        };
        let mut new_messages: Vec<EmailSummary> = fetches.iter().filter_map(|f| summary_from_fetch(mailbox_id, f)).collect();

        // A Sent folder is never supposed to carry unread mail, regardless of
        // which client (or which path - this app's own send, another
        // device, webmail) put a message there. Only newly-arrived messages
        // are swept here, not every cached one on every sync, so a user who
        // deliberately marks a Sent message unread later isn't fought.
        if is_sent {
            let unseen: Vec<Uid> = new_messages.iter().filter(|m| !m.flags.contains(&SystemFlagBit::Seen)).map(|m| m.uid).collect();
            if !unseen.is_empty() {
                match store_flags(session, &unseen, &[SystemFlagBit::Seen], &[]).await {
                    Ok(()) => {
                        for msg in new_messages.iter_mut() {
                            if unseen.contains(&msg.uid) {
                                msg.flags.insert(SystemFlagBit::Seen);
                            }
                        }
                        if let Some(f) = folders.iter_mut().find(|f| f.id == *mailbox_id) {
                            f.unread = f.unread.saturating_sub(unseen.len() as u32);
                        }
                        publish_folders(folders, account_id, cache, events).await;
                    }
                    Err(e) => tracing::warn!("failed to auto-mark Sent arrival as read: {e}"),
                }
            }
        }

        let notify = new_unread_for_notification(cached.is_empty(), &new_messages);
        if !notify.is_empty() {
            let _ = events
                .send(AccountEvent::NewMessages {
                    mailbox: mailbox_id.clone(),
                    messages: notify,
                })
                .await;
        }

        messages.extend(new_messages);
    }

    // Thread keys only change when the message set's membership changes (a
    // new arrival, an expunge, or a UIDVALIDITY-invalidated re-fetch) or a
    // message's Message-ID changes (a server-side rewrite). On a steady-state
    // sync - the delta path carrying unchanged cached messages - every
    // message keeps the key already serialized into its cached summary, so
    // recomputing the whole O(mailbox) union-find pass would be wasted work.
    // The stability checks: no new UIDs fetched this sync, the same count as
    // the cache (an expunge paired with an arrival keeps the count, so the
    // Message-ID mapping below catches that case), and every message's
    // Message-ID still matching its cached value.
    let keys_are_stable = new_uids.is_empty()
        && messages.len() == cached.len()
        && messages
            .iter()
            .all(|m| cached.get(&m.uid).is_some_and(|c| c.message_id == m.message_id));
    if !keys_are_stable {
        let keys = lookout_core::thread::compute_thread_keys(&messages);
        for msg in &mut messages {
            if let Some(key) = keys.get(&msg.uid) {
                msg.thread_key = key.clone();
            }
        }
    }

    tracing::debug!(
        account = %account_id,
        mailbox = %folder_path,
        exists = mailbox_meta.as_ref().map(|m| m.exists).unwrap_or(messages.len() as u32),
        count = messages.len(),
        new = new_uids.len(),
        reused = messages.len().saturating_sub(new_uids.len()),
        "synced mailbox"
    );
    emit_messages(mailbox_id, uidvalidity, &messages, events, cache).await;

    // Capture the count fields before `fetch_previews` takes `messages`: the
    // unread count comes straight from the flags this sync fetched, and the
    // total from the fetch size (see the folder update after the previews).
    let total = messages.len() as u32;
    let unread = messages.iter().filter(|m| m.is_unread()).count() as u32;

    // Phase two: fill in the snippets this sync is still missing, then emit a
    // second update. Deliberately *after* the first emit - the list paints at
    // the envelope fetch's latency, and previews arrive a beat later rather
    // than holding up first paint.
    if let Err(e) = fetch_previews(session, mailbox_id, uidvalidity, messages, events, cache).await {
        // Never propagated: this function's caller tears the connection down
        // on `Err`, and a malformed message or a server that dislikes partial
        // fetches must not cost the user their IMAP session over a cosmetic
        // snippet.
        tracing::warn!("preview fetch for {mailbox_id} failed: {e}");
    }

    // The open folder's count fields, previously refreshed by a STATUS round
    // trip after the wake (`refresh_one_folder_count`), are computable from
    // what this sync already fetched: the flag list gives `unread`, the fetch
    // size gives `total`, and SELECT (when it ran) gave `uidnext`/`uidvalidity`.
    // Update in place and re-publish only when something changed - steady-state
    // wakes change nothing and cost no event and no round trip.
    if let Some(folder) = folders.iter_mut().find(|f| f.id == *mailbox_id) {
        let mut changed = total != folder.total || unread != folder.unread;
        folder.total = total;
        folder.unread = unread;
        if let Some(meta) = &mailbox_meta {
            if let Some(next) = meta.uid_next {
                changed |= folder.uidnext != next;
                folder.uidnext = next;
            }
            changed |= folder.uidvalidity != uidvalidity;
            folder.uidvalidity = uidvalidity;
            // `highest_modseq` is only populated when CONDSTORE is enabled
            // (each SELECT then reports the folder's current modseq).
            if let Some(modseq) = meta.highest_modseq {
                changed |= folder.highest_modseq != Some(modseq);
                folder.highest_modseq = Some(modseq);
            }
        } else if delta {
            // The delta path learns no SELECT meta (its SELECT was skipped),
            // so the baseline advances from the fetched `MODSEQ`s instead:
            // every message the delta reported carries its own modseq, and
            // the highest of them is a safe next baseline - any message with
            // a modseq above it was returned by this very fetch, so its state
            // is known; a message expunged meanwhile is caught by the
            // membership diff next wake. A fetch that returned nothing
            // advances nothing: the next wake's delta is wider, never wrong.
            if let Some(folder_modseq) = folder.highest_modseq {
                let advanced = flag_fetches.iter().filter_map(|f| f.modseq).fold(folder_modseq, u64::max);
                if advanced > folder_modseq {
                    changed = true;
                    folder.highest_modseq = Some(advanced);
                }
            }
        }
        if changed {
            publish_folders(folders, account_id, cache, events).await;
        }
    }
    Ok(())
}

/// Which of a sync's genuinely-new messages (UIDs `sync_mailbox` didn't find
/// in the pre-sync cache) are worth an `AccountEvent::NewMessages` desktop
/// notification: unread ones, but only once the mailbox has synced before.
/// `cached_is_empty` true means this is the mailbox's first-ever sync (or a
/// `UIDVALIDITY` change invalidated everything cached) - notifying once per
/// message already sitting in the folder would be spam, not a signal, so
/// nothing is returned in that case.
fn new_unread_for_notification(cached_is_empty: bool, new_messages: &[EmailSummary]) -> Vec<EmailSummary> {
    if cached_is_empty {
        return Vec::new();
    }
    new_messages.iter().filter(|m| m.is_unread()).cloned().collect()
}

/// Emits whatever envelope summaries are cached on disk for `mailbox_id`,
/// minus currently-snoozed messages, as a `MessagesUpdated` event. Used for
/// instant paint on folder switch - both when `SyncMailbox` wakes the session
/// (pre-IDLE teardown) and when it's processed out of the drain queue with a
/// cache hit. A no-op when nothing is cached.
async fn emit_cached_messages(cache: &CacheHandle, mailbox_id: &MailboxId, events: &async_channel::Sender<AccountEvent>) {
    let Some(Ok(cached)) = cache_op(cache, {
        let mailbox_id = mailbox_id.clone();
        move |c| c.load_messages(&mailbox_id)
    })
    .await
    else {
        return;
    };
    if !cached.is_empty() {
        let snoozed = cache_op(cache, {
            let mailbox_id = mailbox_id.clone();
            move |c| c.active_snoozed_uids(&mailbox_id, chrono::Utc::now())
        })
        .await
        .and_then(|r| r.ok())
        .unwrap_or_default();
        let filtered: Vec<_> = cached.iter().filter(|m| !snoozed.contains(&m.uid)).cloned().collect();
        if !filtered.is_empty() {
            tracing::debug!(mailbox = %mailbox_id, count = filtered.len(), "emitting cached messages for instant display");
            let _ = events
                .send(AccountEvent::MessagesUpdated {
                    mailbox: mailbox_id.clone(),
                    messages: filtered,
                })
                .await;
        }
    }
}

/// Emits the cached summaries for `mailbox_id` (minus snoozed) after one has
/// been deleted, publishing even an *empty* remaining set. Unlike
/// `emit_cached_messages`'s no-op-on-empty, the empty set is meaningful here:
/// the caller has just removed a message, so "no cached rows left" means the
/// list should be cleared, not "nothing cached yet, don't blank the list".
async fn emit_cached_messages_after_removal(cache: &CacheHandle, mailbox_id: &MailboxId, events: &async_channel::Sender<AccountEvent>) {
    if let Some(Ok(cached)) = cache_op(cache, {
        let mailbox_id = mailbox_id.clone();
        move |c| c.load_messages(&mailbox_id)
    })
    .await
    {
        let snoozed = cache_op(cache, {
            let mailbox_id = mailbox_id.clone();
            move |c| c.active_snoozed_uids(&mailbox_id, chrono::Utc::now())
        })
        .await
        .and_then(|r| r.ok())
        .unwrap_or_default();
        let filtered: Vec<_> = cached.iter().filter(|m| !snoozed.contains(&m.uid)).cloned().collect();
        let _ = events
            .send(AccountEvent::MessagesUpdated {
                mailbox: mailbox_id.clone(),
                messages: filtered,
            })
            .await;
    }
}

/// Caches `messages` and publishes them to the UI, minus anything currently
/// snoozed. Snoozed messages are still fetched and cached normally - only
/// what's emitted is filtered, so a snooze is purely a display concern.
async fn emit_messages(mailbox_id: &MailboxId, uidvalidity: UidValidity, messages: &[EmailSummary], events: &async_channel::Sender<AccountEvent>, cache: &CacheHandle) {
    let mut messages = messages.to_vec();
    // The write, the address-book feed, and the snooze read in one blocking
    // hop: `replace_messages` is the session's heaviest cache write (a
    // whole-folder diff), and this function runs at the end of every sync.
    let cached = cache_op(cache, {
        let mailbox_id = mailbox_id.clone();
        let messages = messages.clone();
        move |c| {
            (
                c.replace_messages(&mailbox_id, uidvalidity, &messages),
                c.record_addresses(&messages),
                c.active_snoozed_uids(&mailbox_id, chrono::Utc::now()),
            )
        }
    })
    .await;
    if let Some((replace, addresses, snoozed)) = cached {
        if let Err(e) = replace {
            tracing::warn!("failed to cache messages for {mailbox_id}: {e}");
        }
        // Feeds the composer's recipient autocomplete. Kept here rather than
        // in `replace_messages` because the address book is cumulative -
        // `replace_messages` syncs a mailbox's window to the server's set,
        // and addresses must accumulate across those syncs.
        if let Err(e) = addresses {
            tracing::warn!("failed to record addresses for {mailbox_id}: {e}");
        }
        if let Ok(snoozed) = snoozed {
            messages.retain(|m| !snoozed.contains(&m.uid));
        }
    }
    let _ = events
        .send(AccountEvent::MessagesUpdated {
            mailbox: mailbox_id.clone(),
            messages,
        })
        .await;
}

/// Fetches list-row snippets for the newest `PREVIEW_FETCH_LIMIT` messages in
/// `messages` that don't have one yet, and re-publishes the enriched set.
///
/// A no-op (no round trip, no second event) when every message already has a
/// preview, which is the steady state once a mailbox has been synced once.
async fn fetch_previews(
    session: &mut Session<ImapStream>,
    mailbox_id: &MailboxId,
    uidvalidity: UidValidity,
    mut messages: Vec<EmailSummary>,
    events: &async_channel::Sender<AccountEvent>,
    cache: &CacheHandle,
) -> Result<()> {
    let mut wanted: Vec<Uid> = messages.iter().filter(|m| m.preview.is_none()).map(|m| m.uid).collect();
    if wanted.is_empty() {
        return Ok(());
    }
    // Newest first: the top of the list is what the user is looking at.
    wanted.sort_by_key(|uid| std::cmp::Reverse(uid.0));
    wanted.truncate(PREVIEW_FETCH_LIMIT);

    let query = format!("(UID BODY.PEEK[]<0.{PREVIEW_FETCH_BYTES}>)");
    let mut fetches: Vec<_> = Vec::new();
    for uid_set in &uid_set_chunks(&wanted) {
        let chunk: Vec<_> = session.uid_fetch(uid_set, &query).await?.try_collect().await?;
        fetches.extend(chunk);
    }

    let mut previews = std::collections::HashMap::new();
    for fetch in &fetches {
        let (Some(uid), Some(raw)) = (fetch.uid, fetch.body()) else { continue };
        if let Some(preview) = preview_from_raw(raw) {
            previews.insert(Uid(uid), preview);
        }
    }
    if previews.is_empty() {
        return Ok(());
    }

    for msg in &mut messages {
        if msg.preview.is_none() {
            msg.preview = previews.get(&msg.uid).cloned();
        }
    }
    tracing::debug!(mailbox = %mailbox_id, count = previews.len(), "fetched message previews");
    emit_messages(mailbox_id, uidvalidity, &messages, events, cache).await;
    Ok(())
}

/// Fetches the full raw RFC 5322 message of `uid` in whatever mailbox is
/// currently SELECTed. This is the *fallback* body path, used when a
/// message's summary carries no `BODYSTRUCTURE`-derived part structure (see
/// `fetch_body_partial` for the normal path). Uses `BODY.PEEK[]` rather than
/// `BODY[]`/`RFC822` so reading a message doesn't implicitly set `\Seen`
/// server-side - the UI layer decides if/when to mark as read, matching
/// Bulwark's configurable mark-as-read-delay behavior.
async fn fetch_body(session: &mut Session<ImapStream>, uid: Uid) -> Result<Option<Vec<u8>>> {
    let fetches: Vec<_> = session.uid_fetch(uid.0.to_string(), "BODY.PEEK[]").await?.try_collect().await?;
    let Some(fetch) = fetches.into_iter().find(|f| f.uid == Some(uid.0)) else {
        return Ok(None);
    };
    Ok(fetch.body().map(|body| body.to_vec()))
}

/// Fetches a message's whole raw RFC 5322 bytes, serving a previously-fetched
/// copy from the flat-file raw-message cache when one exists and storing
/// freshly-fetched bytes back into it. `uidvalidity` is passed through so a
/// recycled uid can never resolve to another message's `.eml`.
///
/// Used by both `AccountCommand::FetchRawMessage` (the .eml export path) and
/// the whole-message fallback inside `fetch_body_cached` - which already
/// downloads the full `BODY.PEEK[]` when a partial fetch isn't possible, so
/// persisting those bytes to the export cache costs nothing extra.
async fn fetch_raw_message_cached(cache: &CacheHandle, session: &mut Session<ImapStream>, mailbox: &MailboxId, uid: Uid, uidvalidity: UidValidity) -> Result<Option<Vec<u8>>> {
    if let Some(cached) = cache_op(cache, {
        let mailbox = mailbox.clone();
        move |c| c.load_raw_message(&mailbox, uid, uidvalidity)
    })
    .await
    {
        match cached {
            Ok(Some(bytes)) => {
                tracing::debug!(?mailbox, uid = uid.0, "FetchRawMessage: served from disk cache");
                return Ok(Some(bytes));
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(?mailbox, uid = uid.0, "failed to read cached raw message: {e}"),
        }
    }
    let Some(raw) = fetch_body(session, uid).await? else {
        return Ok(None);
    };
    if let Some(Err(e)) = cache_op(cache, {
        let mailbox = mailbox.clone();
        let raw = raw.clone();
        move |c| c.store_raw_message(&mailbox, uid, uidvalidity, &raw)
    })
    .await
    {
        tracing::warn!(?mailbox, uid = uid.0, "failed to cache raw message: {e}");
    }
    Ok(Some(raw))
}

/// The `SectionPath` that a `BODY.PEEK[<part_number>]` request resolves to in
/// the `FETCH` response. `BODY.PEEK[1.2]` parses back into the same
/// `SectionPath::Part` value the server's response carries, so `Fetch::section`
/// can match it by equality.
fn part_section_path(part_number: &str) -> SectionPath {
    SectionPath::Part(part_number.split('.').filter_map(|n| n.parse().ok()).collect(), None)
}

/// Assembles one message's [`EmailBody`] from its `FETCH` response, given the
/// message's `BODYSTRUCTURE`-derived part list: the raw `BODY.PEEK[HEADER]`
/// bytes become `EmailBody::headers`, and each text/calendar part's section
/// that the server returned becomes the body's decoded text. Shared by the
/// single-message `fetch_body_partial` and the prefetch's batched fetch.
///
/// Returns `None` (rather than an error) when the response carries no text
/// part for the message; the caller falls back to a whole-message fetch
/// rather than showing an empty pane.
fn body_from_fetch(uid: Uid, fetch: &Fetch, parts: &[BodyPart]) -> Option<EmailBody> {
    let headers = fetch
        .section(&SectionPath::Full(MessageSection::Header))
        .map(crate::body::parse_headers_section)
        .unwrap_or_default();

    let mut fetched: Vec<(String, Vec<u8>)> = Vec::new();
    for part in parts.iter().filter(|p| p.is_text() || p.is_calendar()) {
        if let Some(bytes) = fetch.section(&part_section_path(&part.part_number)) {
            fetched.push((part.part_number.clone(), bytes.to_vec()));
        }
    }
    if fetched.is_empty() {
        return None;
    }
    Some(crate::body::assemble_body_from_parts(uid, headers, parts, &fetched))
}

/// Fetches just the parts a message viewer needs: the full header block and
/// the bytes of every `text/plain`/`text/html` part, each by its
/// `BODYSTRUCTURE`-derived section path, plus the `text/calendar` part (the
/// iMIP payload the reading pane's banner acts on). Attachment parts (images,
/// documents, ...) are *never* downloaded - `EmailBody::parts` carries their
/// metadata for a later on-demand fetch. One `UID FETCH` round trip covers
/// the header and all fetched parts.
///
/// Returns `None` (rather than an error) when the message has no text parts
/// to fetch or the server didn't return the requested sections; the caller
/// falls back to a whole-message fetch rather than showing an empty pane.
async fn fetch_body_partial(session: &mut Session<ImapStream>, uid: Uid, parts: &[BodyPart]) -> Result<Option<EmailBody>> {
    let text_parts: Vec<&BodyPart> = parts.iter().filter(|p| p.is_text() || p.is_calendar()).collect();
    if text_parts.is_empty() {
        return Ok(None);
    }

    let mut query = String::from("(BODY.PEEK[HEADER]");
    for part in &text_parts {
        query.push_str(&format!(" BODY.PEEK[{}]", part.part_number));
    }
    query.push(')');

    let fetches: Vec<_> = session.uid_fetch(uid.0.to_string(), &query).await?.try_collect().await?;
    let Some(fetch) = fetches.into_iter().find(|f| f.uid == Some(uid.0)) else {
        return Ok(None);
    };
    Ok(body_from_fetch(uid, &fetch, parts))
}

/// Fetches the text parts of a whole prefetch batch in **one** `UID FETCH`
/// round trip: the full header block and every text/calendar part's bytes for
/// each message, by the `BODYSTRUCTURE`-derived section paths the envelope
/// pass learned (`pf.structures`). The query's part list is the *union* of
/// the batch's part numbers - a message that lacks a part the union mentions
/// simply gets no section back for it. Responses are matched back to their
/// messages by the (explicitly requested) UID data item, so no per-message
/// ordering is assumed.
///
/// Bodies that landed in the cache since the envelope pass (a user click, a
/// previous run) are skipped with one `has_bodies` query rather than
/// re-downloaded - a single blocking-pool hop, where the per-message path
/// paid a `load_body` JSON decode per uid. A message whose response carries
/// no text part degrades to a whole-message fetch, the same fallback
/// `fetch_body_cached` applies. Returns the number of bodies stored.
async fn fetch_bodies_batch(
    cache: &CacheHandle,
    session: &mut Session<ImapStream>,
    mailbox: &MailboxId,
    uidvalidity: UidValidity,
    batch: &[Uid],
    structures: &HashMap<Uid, Vec<BodyPart>>,
) -> Result<usize> {
    if batch.is_empty() {
        return Ok(0);
    }

    // Drop uids whose body landed in the cache since the envelope pass.
    let have = cache_op(cache, {
        let mailbox = mailbox.clone();
        let batch = batch.to_vec();
        move |c| c.has_bodies(&mailbox, &batch, uidvalidity)
    })
    .await
    .and_then(|r| r.ok())
    .unwrap_or_default();
    let batch: Vec<Uid> = batch.iter().copied().filter(|uid| !have.contains(uid)).collect();
    if batch.is_empty() {
        return Ok(0);
    }

    // Union of the batch's text/calendar part numbers - one query item per
    // distinct part path. A message without any text part never matches a
    // section and falls back to a whole-message fetch below.
    let mut part_numbers: Vec<&str> = Vec::new();
    for parts in batch.iter().filter_map(|uid| structures.get(uid)) {
        for part in parts.iter().filter(|p| p.is_text() || p.is_calendar()) {
            if !part_numbers.contains(&part.part_number.as_str()) {
                part_numbers.push(part.part_number.as_str());
            }
        }
    }
    if part_numbers.is_empty() {
        return Ok(0);
    }
    let mut query = String::from("(UID BODY.PEEK[HEADER]");
    for number in &part_numbers {
        query.push_str(" BODY.PEEK[");
        query.push_str(number);
        query.push(']');
    }
    query.push(')');

    // The batch is at most `PREFETCH_BATCH_SIZE` uids, so `uid_set_chunks`
    // yields a single (range-compressed) chunk, keeping the command line well
    // under servers' length limits.
    let set = uid_set_chunks(&batch)[0].clone();
    let fetches: Vec<_> = session.uid_fetch(&set, &query).await?.try_collect().await?;

    let mut bodies: Vec<(Uid, EmailBody)> = Vec::new();
    for fetch in &fetches {
        let Some(uid) = fetch.uid.map(Uid) else { continue };
        let Some(parts) = structures.get(&uid) else { continue };
        if let Some(body) = body_from_fetch(uid, fetch, parts) {
            bodies.push((uid, body));
        }
    }

    // Messages the batch response carried no text part for degrade to the
    // whole-message fetch, exactly as the per-message path does.
    let missing: HashSet<Uid> = batch.iter().copied().filter(|uid| !bodies.iter().any(|(u, _)| u == uid)).collect();
    for uid in &missing {
        match fetch_raw_message_cached(cache, session, mailbox, *uid, uidvalidity).await {
            Ok(Some(raw)) => {
                if let Some(body) = parse_body(*uid, &raw) {
                    bodies.push((*uid, body));
                }
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(?uid, "prefetch: whole-message fallback failed: {e}"),
        }
    }

    let stored = bodies.len();
    if stored > 0 {
        if let Some(Err(e)) = cache_op(cache, {
            let mailbox = mailbox.clone();
            move |c| c.store_bodies(&mailbox, uidvalidity, &bodies)
        })
        .await
        {
            tracing::warn!(?mailbox, "prefetch: failed to cache bodies: {e}");
        }
    }
    Ok(stored)
}

/// Fetches one attachment part's wire bytes for the on-demand
/// `AccountCommand::FetchAttachment`, keyed by its BODYSTRUCTURE-derived part
/// number, and returns them *transfer-decoded* (base64/quoted-printable undone
/// via `transfer_part_bytes`). An embedded `message/rfc822` attachment is
/// returned whole, not re-parsed. Returns `None` if the server didn't return
/// the section (rather than erroring), so the caller can no-op gracefully.
async fn fetch_attachment_part(session: &mut Session<ImapStream>, uid: Uid, part: &BodyPart) -> Result<Option<Vec<u8>>> {    // `BODY.PEEK[1.2]` parses back into the same `SectionPath::Part` the
    // server's response carries, exactly as `fetch_body_partial` relies on.
    let path = part_section_path(&part.part_number);
    let query = format!("(BODY.PEEK[{}])", part.part_number);
    let fetches: Vec<_> = session.uid_fetch(uid.0.to_string(), &query).await?.try_collect().await?;
    let Some(fetch) = fetches.into_iter().find(|f| f.uid == Some(uid.0)) else {
        return Ok(None);
    };
    let Some(bytes) = fetch.section(&path) else {
        return Ok(None);
    };
    Ok(Some(crate::body::transfer_part_bytes(part, bytes)))
}

/// Fallback for an attachment part whose section no longer exists on the
/// server: fetch the whole message (serving the raw-message cache when one
/// is already stored) and re-derive the part from its actual bytes via
/// `find_part_in_raw`, which matches by `cid` (inline images), filename, or
/// content type instead of the stale section path. A part structure can go
/// stale when the message is rewritten server-side after its body or
/// summary was cached (mailing-list/gateway/proxy rewrites) - the part is
/// still there, just not at the `BODY[<n>]` path the cache remembers.
/// Returns `Ok(None)` when the part genuinely can't be found, which the
/// caller reports as a missing part.
async fn fetch_stale_part_fallback(
    cache: &CacheHandle,
    session: &mut Session<ImapStream>,
    mailbox: &MailboxId,
    uid: Uid,
    uidvalidity: UidValidity,
    part: &BodyPart,
) -> Result<Option<Vec<u8>>> {
    let Some(raw) = fetch_raw_message_cached(cache, session, mailbox, uid, uidvalidity).await? else {
        return Ok(None);
    };
    let Some(bytes) = crate::body::find_part_in_raw(&raw, part) else {
        return Ok(None);
    };
    tracing::warn!(?mailbox, uid = uid.0, part = %part.part_number, "FetchAttachment: stale part structure - re-derived the part from the whole message");
    Ok(Some(bytes))
}

/// Resolves the body of `uid` in the currently SELECTed mailbox, serving a
/// previously-fetched copy from the on-disk cache when one exists (no network
/// round trip) and storing freshly-fetched bodies back into it, so re-opening
/// a message doesn't re-download it. `uidvalidity` is passed through to the
/// cache so a recycled uid can never resolve to another message's body.
///
/// The fetch itself is `BODYSTRUCTURE`-driven when a part structure is
/// available - `known_structure` (the prefetch path already learned it) or
/// the message's cached summary - and falls back to a whole-message
/// `BODY.PEEK[]` fetch otherwise. The result is an assembled [`EmailBody`]
/// either way.
async fn fetch_body_cached(
    cache: &CacheHandle,
    session: &mut Session<ImapStream>,
    mailbox: &MailboxId,
    uid: Uid,
    uidvalidity: UidValidity,
    known_structure: Option<&[BodyPart]>,
) -> Result<Option<EmailBody>> {
    if let Some(cached) = cache_op(cache, {
        let mailbox = mailbox.clone();
        move |c| c.load_body(&mailbox, uid, uidvalidity)
    })
    .await
    {
        match cached {
            Ok(Some(body)) => {
                tracing::debug!(?mailbox, uid = uid.0, "FetchBody: served from disk cache");
                return Ok(Some(body));
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(?mailbox, uid = uid.0, "failed to read cached message body: {e}"),
        }
    }
    // The part structure that makes a partial fetch possible: prefer the
    // caller-provided one (the prefetch learns it in its envelope pass), else
    // the cached summary's (the open-folder path synced it). Messages whose
    // summaries predate BODYSTRUCTURE fetching - or servers that never
    // returned one - fall back to the whole-message fetch below.
    let structure = match known_structure {
        Some(structure) => Some(structure.to_vec()),
        None => cache_op(cache, {
            let mailbox = mailbox.clone();
            move |c| c.load_summary(&mailbox, uid)
        })
        .await
        .and_then(|r| r.ok())
        .flatten()
        .and_then(|s| s.structure),
    };
    let body = match &structure {
        Some(parts) => match fetch_body_partial(session, uid, parts).await {
            // A partial fetch that fails - or yields nothing readable (a
            // weird server, a structure that didn't match reality, or text
            // parts that didn't decode) - must degrade to the whole-message
            // fetch, not an empty reading pane.
            Ok(Some(body)) if body.text_body.is_some() || body.html_body.is_some() => Some(body),
            _ => fetch_raw_message_cached(cache, session, mailbox, uid, uidvalidity)
                .await?
                .and_then(|raw| parse_body(uid, &raw)),
        },
        None => fetch_raw_message_cached(cache, session, mailbox, uid, uidvalidity)
            .await?
            .and_then(|raw| parse_body(uid, &raw)),
    };
    if let Some(body) = &body {
        if let Some(Err(e)) = cache_op(cache, {
            let mailbox = mailbox.clone();
            let body = body.clone();
            move |c| c.store_body(&mailbox, uid, uidvalidity, &body)
        })
        .await
        {
            tracing::warn!(?mailbox, uid = uid.0, "failed to cache message body: {e}");
        }
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mailbox(account_id: &AccountId, name: &str, role: MailboxRole, unread: u32) -> Mailbox {
        Mailbox {
            id: MailboxId::new(account_id, name),
            account_id: account_id.clone(),
            name: name.to_string(),
            parent: None,
            delimiter: '/',
            role,
            uidvalidity: UidValidity(1),
            uidnext: 1,
            highest_modseq: None,
            total: 0,
            unread,
            flags: vec![],
            subscribed: true,
        }
    }

    #[test]
    fn count_queue_puts_the_open_folder_first_then_the_inbox() {
        let account = AccountId("acc".into());
        let folders = vec![
            mailbox(&account, "Archive", MailboxRole::Archive, 0),
            mailbox(&account, "INBOX", MailboxRole::Inbox, 0),
            mailbox(&account, "Work", MailboxRole::Custom, 0),
        ];
        let open = MailboxId::new(&account, "Work");
        let queue: Vec<MailboxId> = queue_folder_counts(&folders, &open).into();
        assert_eq!(
            queue,
            vec![MailboxId::new(&account, "Work"), MailboxId::new(&account, "INBOX"), MailboxId::new(&account, "Archive"),]
        );
    }

    #[test]
    fn count_queue_covers_every_folder_exactly_once() {
        // The open folder is also the Inbox here - it must not be queued
        // twice by matching both the "current" and the "inbox" rank.
        let account = AccountId("acc".into());
        let folders = vec![mailbox(&account, "INBOX", MailboxRole::Inbox, 0), mailbox(&account, "Work", MailboxRole::Custom, 0)];
        let queue: Vec<MailboxId> = queue_folder_counts(&folders, &MailboxId::new(&account, "INBOX")).into();
        assert_eq!(queue, vec![MailboxId::new(&account, "INBOX"), MailboxId::new(&account, "Work")]);
    }

    fn summary(uid: u32, mailbox: &MailboxId, seen: bool) -> EmailSummary {
        let mut flags = std::collections::BTreeSet::new();
        if seen {
            flags.insert(SystemFlagBit::Seen);
        }
        EmailSummary {
            uid: Uid(uid),
            mailbox: mailbox.clone(),
            message_id: None,
            in_reply_to: None,
            references: Vec::new(),
            thread_key: lookout_core::thread::ThreadKey(format!("thread-{uid}")),
            subject: Some(format!("Subject {uid}")),
            from: Vec::new(),
            to: Vec::new(),
            cc: Vec::new(),
            date: chrono::Utc::now(),
            flags,
            keywords: std::collections::BTreeSet::new(),
            size: 0,
            has_attachment: false,
            has_calendar: false,
            preview: None,
            structure: None,
        }
    }

    #[test]
    fn first_sync_never_notifies() {
        // An empty pre-sync cache means this mailbox has never synced before
        // (or UIDVALIDITY changed) - every "new" message is really just the
        // whole folder, not something worth a notification.
        let account = AccountId("acc".into());
        let mailbox = MailboxId::new(&account, "INBOX");
        let new_messages = vec![summary(1, &mailbox, false), summary(2, &mailbox, false)];
        assert!(new_unread_for_notification(true, &new_messages).is_empty());
    }

    #[test]
    fn resync_notifies_only_unread_new_messages() {
        let account = AccountId("acc".into());
        let mailbox = MailboxId::new(&account, "INBOX");
        let new_messages = vec![summary(1, &mailbox, false), summary(2, &mailbox, true)];
        let notify = new_unread_for_notification(false, &new_messages);
        assert_eq!(notify.iter().map(|m| m.uid).collect::<Vec<_>>(), vec![Uid(1)]);
    }

    #[test]
    fn resync_with_no_new_messages_notifies_nothing() {
        assert!(new_unread_for_notification(false, &[]).is_empty());
    }

    #[test]
    fn a_relist_keeps_the_counts_already_learned() {
        // LIST reports no counts, so a re-list arrives all-zero; without the
        // carry-over the sidebar would blank on every Refresh and message move.
        let account = AccountId("acc".into());
        let known = vec![mailbox(&account, "INBOX", MailboxRole::Inbox, 7)];
        let mut relisted = vec![mailbox(&account, "INBOX", MailboxRole::Inbox, 0), mailbox(&account, "New", MailboxRole::Custom, 0)];
        carry_counts_forward(&mut relisted, &known, false);
        assert_eq!(relisted[0].unread, 7);
        // A folder that's new since the last list has nothing to carry over
        // and keeps its zeros until the drain reaches it.
        assert_eq!(relisted[1].unread, 0);
    }

    #[test]
    fn a_list_status_relist_keeps_its_fresh_counts() {
        // A LIST-STATUS list carries authoritative counts and must not be
        // overwritten by the carry-over - a mailbox emptied since the last
        // run reads a real 0, and the stale old count must not come back.
        let account = AccountId("acc".into());
        let mut known = vec![mailbox(&account, "INBOX", MailboxRole::Inbox, 7)];
        let mut relisted = vec![mailbox(&account, "INBOX", MailboxRole::Inbox, 3), mailbox(&account, "New", MailboxRole::Custom, 0)];
        relisted[0].total = 3;
        carry_counts_forward(&mut relisted, &known, true);
        assert_eq!(relisted[0].unread, 3, "the fresh LIST-STATUS count wins");
        assert_eq!(relisted[0].total, 3, "the fresh LIST-STATUS total wins");
        // A modseq the server didn't report is still carried - the incremental
        // sync baseline must survive a re-list.
        known[0].highest_modseq = Some(4_200_000_000);
        let mut relisted = vec![mailbox(&account, "INBOX", MailboxRole::Inbox, 3)];
        carry_counts_forward(&mut relisted, &known, true);
        assert_eq!(relisted[0].highest_modseq, Some(4_200_000_000));
        // ... but a reported one wins.
        let mut relisted = vec![mailbox(&account, "INBOX", MailboxRole::Inbox, 3)];
        relisted[0].highest_modseq = Some(5_000_000_000);
        carry_counts_forward(&mut relisted, &known, true);
        assert_eq!(relisted[0].highest_modseq, Some(5_000_000_000));
    }

    #[test]
    fn a_relist_carries_the_learned_modseq_forward() {
        // LIST can't report `highest_modseq`, so a re-list must not wipe the
        // baseline the incremental-sync paths persist per folder.
        let account = AccountId("acc".into());
        let mut known = vec![mailbox(&account, "INBOX", MailboxRole::Inbox, 0)];
        let mut relisted = vec![mailbox(&account, "INBOX", MailboxRole::Inbox, 0)];
        relisted[0].highest_modseq = Some(4_200_000_000);
        carry_counts_forward(&mut relisted, &known, false);
        assert_eq!(relisted[0].highest_modseq, None);

        let mut relisted = vec![mailbox(&account, "INBOX", MailboxRole::Inbox, 0)];
        known[0].highest_modseq = Some(4_200_000_000);
        carry_counts_forward(&mut relisted, &known, false);
        assert_eq!(relisted[0].highest_modseq, Some(4_200_000_000));
    }

    #[test]
    fn status_query_asks_for_modseq_only_on_condstore_connections() {
        assert_eq!(status_count_query(true), "(MESSAGES UNSEEN UIDNEXT UIDVALIDITY HIGHESTMODSEQ)");
        assert_eq!(status_count_query(false), "(MESSAGES UNSEEN UIDNEXT UIDVALIDITY)");
    }

    #[test]
    fn uid_set_chunks_sorts_dedups_and_compresses_contiguous_runs() {
        let chunks = uid_set_chunks(&[Uid(3), Uid(1), Uid(2), Uid(2), Uid(7), Uid(5), Uid(6)]);
        assert_eq!(chunks, vec!["1:3,5:7"]);
    }

    #[test]
    fn uid_set_chunks_sparse_uids_stay_comma_joined() {
        assert_eq!(uid_set_chunks(&[Uid(1), Uid(3), Uid(5)]), vec!["1,3,5"]);
    }

    #[test]
    fn uid_set_chunks_a_dense_big_folder_is_one_short_line() {
        // 100k contiguous uids must compress to a single tiny sequence set,
        // not 100k comma-joined numbers on one doomed command line.
        let uids: Vec<Uid> = (1..=100_000).map(Uid).collect();
        assert_eq!(uid_set_chunks(&uids), vec!["1:100000"]);
    }

    #[test]
    fn uid_set_chunks_splits_oversized_sparse_sets_below_the_byte_budget() {
        // 9-digit uids every other number: no run compresses, and 5000 of
        // them render past `UID_SET_BYTE_BUDGET`, so the set must split into
        // multiple lines, each under the budget and together covering exactly
        // the input.
        let uids: Vec<Uid> = (0..5000).map(|i| Uid(10_000_001 + i * 2)).collect();
        let chunks = uid_set_chunks(&uids);
        assert!(chunks.len() > 1, "expected the set to split, got {} chunk(s)", chunks.len());
        for chunk in &chunks {
            assert!(chunk.len() <= UID_SET_BYTE_BUDGET, "chunk of {} bytes exceeds the budget", chunk.len());
            for part in chunk.split(',') {
                let parsed: u32 = part.parse().unwrap_or_else(|_| panic!("chunk holds a non-UID token: {part:?}"));
                assert_eq!(parsed % 2, 1, "chunk lost a uid or gained one: {parsed}");
            }
        }
        let union: Vec<u32> = chunks.iter().flat_map(|chunk| chunk.split(',')).map(|part| part.parse().unwrap()).collect();
        assert_eq!(union, uids.iter().map(|u| u.0).collect::<Vec<_>>());
    }

    #[test]
    fn uid_set_chunks_empty_input_sends_no_command() {
        assert!(uid_set_chunks(&[]).is_empty());
    }

    #[test]
    fn mark_count_changes_flags_moved_counts_and_skips_unchanged() {
        let account = AccountId("acc".into());
        let inbox = MailboxId::new(&account, "INBOX");
        let archive = MailboxId::new(&account, "Archive");
        let current = inbox.clone();
        let previous = vec![mailbox(&account, "INBOX", MailboxRole::Inbox, 2), mailbox(&account, "Archive", MailboxRole::Archive, 5)];
        let mut listed = vec![mailbox(&account, "INBOX", MailboxRole::Inbox, 2), mailbox(&account, "Archive", MailboxRole::Archive, 6)];
        listed[0].total = 9; // the open folder's total moved too
        let mut dirty = HashSet::new();
        let open_changed = mark_count_changes(&previous, &listed, &current, &mut dirty);
        assert!(open_changed, "the open folder's count moved");
        assert_eq!(dirty, HashSet::from([archive]), "only the off-screen moved mailbox is marked");

        // Unchanged counts mark nothing.
        let mut dirty = HashSet::new();
        let open_changed = mark_count_changes(&previous, &previous, &current, &mut dirty);
        assert!(!open_changed);
        assert!(dirty.is_empty());
    }

    #[test]
    fn mark_count_changes_treats_a_modseq_advance_as_a_change() {
        let account = AccountId("acc".into());
        let inbox = MailboxId::new(&account, "INBOX");
        let previous = vec![mailbox(&account, "INBOX", MailboxRole::Inbox, 2)];
        let mut listed = vec![mailbox(&account, "INBOX", MailboxRole::Inbox, 2)];
        listed[0].highest_modseq = Some(4_200_000_000);
        let mut dirty = HashSet::new();
        // The open folder changed -> reported, not dirtied.
        assert!(mark_count_changes(&previous, &listed, &inbox, &mut dirty));
        assert!(dirty.is_empty());
        // An off-screen folder with a fresh modseq is dirtied - the same
        // `changed` semantics as `refresh_folder_counts`.
        let work = MailboxId::new(&account, "Work");
        let previous = vec![mailbox(&account, "INBOX", MailboxRole::Inbox, 2), mailbox(&account, "Work", MailboxRole::Custom, 2)];
        let mut listed = vec![mailbox(&account, "Work", MailboxRole::Custom, 2)];
        listed[0].highest_modseq = Some(4_200_000_000);
        let mut dirty = HashSet::new();
        assert!(!mark_count_changes(&previous, &listed, &inbox, &mut dirty));
        assert_eq!(dirty, HashSet::from([work]));
    }

    #[test]
    fn apply_status_items_fills_the_count_fields() {
        let account = AccountId("acc".into());
        let mut m = mailbox(&account, "INBOX", MailboxRole::Inbox, 0);
        apply_status_items(
            &mut m,
            &[
                StatusAttribute::Messages(42),
                StatusAttribute::Unseen(7),
                StatusAttribute::UidNext(100),
                StatusAttribute::UidValidity(1),
                StatusAttribute::HighestModSeq(12345),
            ],
        );
        assert_eq!(m.total, 42);
        assert_eq!(m.unread, 7);
        assert_eq!(m.uidnext, 100);
        assert_eq!(m.uidvalidity, UidValidity(1));
        assert_eq!(m.highest_modseq, Some(12345));
        // Items not requested leave the LIST-only defaults alone.
        let mut m = mailbox(&account, "INBOX", MailboxRole::Inbox, 0);
        apply_status_items(&mut m, &[StatusAttribute::Messages(3)]);
        assert_eq!(m.total, 3);
        assert_eq!(m.unread, 0);
        assert_eq!(m.highest_modseq, None);
    }

    #[test]
    fn mailbox_from_list_parts_skips_noselect_and_maps_the_rest() {
        let account = AccountId("acc".into());
        assert!(mailbox_from_list_parts(&account, "INBOX", Some("/"), &[]).is_some());
        assert!(mailbox_from_list_parts(&account, "NoAccess", Some("/"), &[NameAttribute::NoSelect]).is_none());
        let m = mailbox_from_list_parts(&account, "Work/Sent", Some("/"), &[NameAttribute::Sent]).unwrap();
        assert_eq!(m.id, MailboxId::new(&account, "Work/Sent"));
        assert_eq!(m.name, "Sent");
        assert_eq!(m.delimiter, '/');
        assert_eq!(m.role, MailboxRole::Sent);
    }
}
