use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use async_imap::Session;
use futures::TryStreamExt;
use lookout_core::mailbox::role_from_special_use;
use lookout_core::{AccountId, BodyPart, EmailBody, EmailSummary, Mailbox, MailboxId, MailboxRole, SystemFlagBit, Uid, UidValidity};

use crate::auth::XOAuth2Authenticator;
use crate::body::{parse_body, preview_from_raw};
use crate::config::{AccountConfig, Credential};
use crate::connection::{connect_tls, ImapStream};
use crate::envelope::summary_from_fetch;
use crate::error::{Error, Result};
use crate::send::{build_raw_message, send_smtp, ComposedMessage};

/// How many of the most recent messages the background body prefetch queues
/// for download on a folder it hasn't warmed yet. Bounds the *body* warm-up
/// only - the display/envelope sync fetches the whole folder (see
/// `sync_mailbox`) - so a folder's newest N get bodies downloaded in batches
/// while anything older fetches on demand. Since the prefetch learns each
/// message's `BODYSTRUCTURE` in its envelope pass, "download" here means the
/// text parts only - attachments are never fetched for the cache. Full
/// CONDSTORE/QRESYNC incremental sync is Phase 2 - see the module docs.
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
    let cache = match crate::cache::Cache::open(&config.account_id) {
        Ok(cache) => Some(cache),
        Err(e) => {
            tracing::warn!("couldn't open local cache, continuing without it: {e}");
            None
        }
    };

    // Fast first paint: emit whatever's cached from the previous session
    // before the network connection even starts. This is immediately
    // superseded by live data once the connection succeeds - the cache is
    // never treated as authoritative (see `Cache`'s doc comment).
    if let Some(cache) = &cache {
        if let Ok(folders) = cache.load_mailboxes(&config.account_id) {
            if !folders.is_empty() {
                let _ = events.send(AccountEvent::FoldersUpdated(folders)).await;
            }
        }
        let inbox_id = MailboxId::new(&config.account_id, "INBOX");
        if let Ok(messages) = cache.load_messages(&inbox_id) {
            if !messages.is_empty() {
                let _ = events.send(AccountEvent::MessagesUpdated { mailbox: inbox_id, messages }).await;
            }
        }
        // One-time FTS backfill, on this worker thread rather than the UI
        // thread: `Cache::open` deliberately doesn't do it (see that method's
        // note), because the app also opens a read-side handle from the main
        // thread at connect time and backfilling a large pre-search cache -
        // re-parsing every cached body - would block startup. This runs
        // before the connection is attempted, so it doesn't delay the cached
        // first paint above, and it's a cheap no-op once the index exists.
        if let Err(e) = cache.backfill_search_index() {
            tracing::warn!("failed to backfill the search index: {e}");
        }
    }

    let mut backoff = Duration::from_secs(1);
    const MAX_BACKOFF: Duration = Duration::from_secs(60);

    loop {
        let _ = events.send(AccountEvent::ConnectionStateChanged(ConnectionState::Connecting)).await;
        match connect_and_run(&config, credentials.as_ref(), &commands, &events, cache.as_ref()).await {
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
        // immediately rather than looping back into another connect attempt.
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            cmd = commands.recv() => {
                if matches!(cmd, Ok(AccountCommand::Shutdown)) {
                    let _ = events.send(AccountEvent::ConnectionStateChanged(ConnectionState::Disconnected)).await;
                    return;
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
    cache: Option<&crate::cache::Cache>,
) -> Result<ShutdownReason> {
    let credential = credentials.imap_credential().await.map_err(Error::LoginFailed)?;
    let mut session = login(config, credential).await?;

    let account_id = config.account_id.clone();
    let mut folders = list_mailboxes(&mut session, &account_id).await?;
    // Fast first paint: the folder tree shows immediately and the INBOX sync
    // that fills the message list starts right away. The per-folder STATUS
    // counts are queued for the main loop's cooperative drain (see below)
    // rather than issued here, so a large account's count pass never stalls
    // the session for commands.
    //
    // LIST reports no counts at all, so the ones learned last run are carried
    // over from the cache first - otherwise this emit would visibly blank
    // every count the pre-connect cache replay just painted, and the sidebar
    // would flash counts -> zeros -> counts on every launch.
    if let Some(cache) = cache {
        if let Ok(known) = cache.load_mailboxes(&account_id) {
            carry_counts_forward(&mut folders, &known);
        }
    }
    publish_folders(&folders, &account_id, cache, events).await;

    let inbox_id = MailboxId::new(&account_id, "INBOX");
    sync_mailbox(&mut session, &account_id, "INBOX", &inbox_id, events, cache).await?;

    // Folders still awaiting their STATUS count, drained cooperatively below.
    // Held as ids rather than indices so a re-list that reorders (or shortens)
    // the folder list can't leave the queue pointing at the wrong mailbox.
    // Re-filled by `relist_folders` whenever the list changes.
    let mut counts_pending: VecDeque<MailboxId> = queue_folder_counts(&folders, &inbox_id);

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
        let wake = match commands.try_recv() {
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
                    session.select(&current_mailbox_name).await?;
                    session_selected = current_mailbox_id.clone();
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
        };

        let _ = events.send(AccountEvent::ConnectionStateChanged(ConnectionState::Busy)).await;

        let mut woke_on_command = None;
        match wake {
            // A server notification during IDLE (EXISTS/EXPUNGE/etc) means
            // the currently-selected mailbox changed; re-fetch its envelope
            // set. This is a full re-fetch rather than a CONDSTORE delta -
            // see `sync_mailbox` and the module docs.
            Wake::Idle(Ok(async_imap::extensions::idle::IdleResponse::Timeout)) => {}
            Wake::Idle(Ok(_)) => {
                sync_mailbox(&mut session, &account_id, &current_mailbox_name, &current_mailbox_id, events, cache).await?;
                // New mail just landed in (or was expunged from) the open
                // folder; refresh its count so the sidebar matches the list.
                refresh_one_folder_count(&mut session, &mut folders, &account_id, &current_mailbox_id, cache, events).await;
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
                    relist_folders(&mut session, &mut folders, &mut counts_pending, &account_id, &current_mailbox_id, cache, events).await?;
                    sync_mailbox(&mut session, &account_id, &current_mailbox_name, &current_mailbox_id, events, cache).await?;
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
                        let cached = cache.is_some_and(|c| c.has_messages(&current_mailbox_id).unwrap_or(false));
                        if cached {
                            tracing::debug!(mailbox = %current_mailbox_id, "SyncMailbox: cache hit, emitting cached messages without IMAP sync");
                            emit_cached_messages(cache, &current_mailbox_id, events).await;
                            continue;
                        }
                        sync_mailbox(&mut session, &account_id, &current_mailbox_name, &current_mailbox_id, events, cache).await?;
                        session_selected = current_mailbox_id.clone();
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
                    let fetched = match cache {
                        Some(c) => match c.load_attachment(&mailbox, uid, uidvalidity, &part.part_number) {
                            Ok(Some(bytes)) => {
                                tracing::debug!(?mailbox, uid = uid.0, part = %part.part_number, "FetchAttachment: served from disk cache");
                                Ok(Some(bytes))
                            }
                            Ok(None) => fetch_attachment_part(&mut session, uid, &part).await,
                            Err(e) => {
                                tracing::warn!(?mailbox, uid = uid.0, part = %part.part_number, "failed to read cached attachment: {e}");
                                fetch_attachment_part(&mut session, uid, &part).await
                            }
                        },
                        None => fetch_attachment_part(&mut session, uid, &part).await,
                    };
                    let bytes = match fetched {
                        Ok(Some(bytes)) => Some(bytes),
                        // `None` means the server didn't return the requested
                        // section - a part number that doesn't match the
                        // server's view of the message.
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
                        if let Some(cache) = cache {
                            if let Err(e) = cache.store_attachment(&mailbox, uid, uidvalidity, &part.part_number, &bytes) {
                                tracing::warn!(?mailbox, uid = uid.0, part = %part.part_number, "failed to cache attachment bytes: {e}");
                            }
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
                    Ok(()) => {
                        let _ = events.send(AccountEvent::SendCompleted).await;
                    }
                    Err(e) => {
                        let _ = events.send(AccountEvent::Error(format!("Couldn't send message: {e}"))).await;
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
                        relist_folders(&mut session, &mut folders, &mut counts_pending, &account_id, &current_mailbox_id, cache, events).await?;
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
                    match move_message_to_role(&mut session, &folders, &account_id, &[uid], role).await {
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
                            if let Some(cache) = cache {
                                if let Err(e) = cache.delete_message(&mailbox, uid) {
                                    tracing::warn!("failed to drop moved message from cache: {e}");
                                }
                                emit_cached_messages_after_removal(cache, &mailbox, events).await;
                            }
                            relist_folders(&mut session, &mut folders, &mut counts_pending, &account_id, &current_mailbox_id, cache, events).await?;
                            sync_mailbox(&mut session, &account_id, &current_mailbox_name, &current_mailbox_id, events, cache).await?;
                            session_selected = current_mailbox_id.clone();
                        }
                        Err(e) => {
                            let _ = events.send(AccountEvent::Error(format!("Couldn't move message: {e}"))).await;
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
                    match move_message_to_role(&mut session, &folders, &account_id, &uids, role).await {
                        Ok(()) => {
                            let _ = events.send(AccountEvent::MessageMoved { role }).await;
                            if let Some(cache) = cache {
                                for uid in &uids {
                                    if let Err(e) = cache.delete_message(&mailbox, *uid) {
                                        tracing::warn!("failed to drop moved message from cache: {e}");
                                    }
                                }
                                emit_cached_messages_after_removal(cache, &mailbox, events).await;
                            }
                            relist_folders(&mut session, &mut folders, &mut counts_pending, &account_id, &current_mailbox_id, cache, events).await?;
                            sync_mailbox(&mut session, &account_id, &current_mailbox_name, &current_mailbox_id, events, cache).await?;
                            session_selected = current_mailbox_id.clone();
                        }
                        Err(e) => {
                            let _ = events.send(AccountEvent::Error(format!("Couldn't move messages: {e}"))).await;
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
                    match move_uids_to_path(&mut session, &uids, &target_path).await {
                        Ok(()) => {
                            // `Custom` here is just a marker for the generic
                            // "Moved" toast - the target was an arbitrary
                            // folder, not a special-use role.
                            let _ = events.send(AccountEvent::MessageMoved { role: MailboxRole::Custom }).await;
                            if let Some(cache) = cache {
                                for uid in &uids {
                                    if let Err(e) = cache.delete_message(&mailbox, *uid) {
                                        tracing::warn!("failed to drop moved message from cache: {e}");
                                    }
                                }
                                emit_cached_messages_after_removal(cache, &mailbox, events).await;
                            }
                            relist_folders(&mut session, &mut folders, &mut counts_pending, &account_id, &current_mailbox_id, cache, events).await?;
                            sync_mailbox(&mut session, &account_id, &current_mailbox_name, &current_mailbox_id, events, cache).await?;
                            session_selected = current_mailbox_id.clone();
                        }
                        Err(e) => {
                            let _ = events.send(AccountEvent::Error(format!("Couldn't move messages: {e}"))).await;
                        }
                    }
                }
                AccountCommand::SnoozeMessage { mailbox, uid, until } => {
                    if let Some(cache) = cache {
                        if let Err(e) = cache.snooze_message(&mailbox, uid, until) {
                            tracing::warn!("failed to record snooze: {e}");
                        }
                    }
                    let _ = events.send(AccountEvent::MessageSnoozed).await;
                    sync_mailbox(&mut session, &account_id, &current_mailbox_name, &current_mailbox_id, events, cache).await?;
                    session_selected = current_mailbox_id.clone();
                }
                AccountCommand::SnoozeMessages { mailbox, uids, until } => {
                    if let Some(cache) = cache {
                        for uid in &uids {
                            if let Err(e) = cache.snooze_message(&mailbox, *uid, until) {
                                tracing::warn!("failed to record snooze: {e}");
                            }
                        }
                    }
                    let _ = events.send(AccountEvent::MessageSnoozed).await;
                    sync_mailbox(&mut session, &account_id, &current_mailbox_name, &current_mailbox_id, events, cache).await?;
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
                            let patched = match cache {
                                Some(cache) => match cache.update_flags(&mailbox, uid, &add, &remove) {
                                    Ok(patched) => patched,
                                    Err(e) => {
                                        tracing::warn!("failed to update cached flags: {e}");
                                        false
                                    }
                                },
                                None => false,
                            };
                            if patched {
                                emit_cached_messages(cache, &mailbox, events).await;
                            } else if mailbox == current_mailbox_id {
                                // No cache (or the uid fell outside the
                                // cached window): re-sync so the UI still
                                // sees the new flags.
                                sync_mailbox(&mut session, &account_id, &current_mailbox_name, &current_mailbox_id, events, cache).await?;
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
                            let _ = events.send(AccountEvent::Error(format!("Couldn't update message flags: {e}"))).await;
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
                            let all_patched = match cache {
                                Some(cache) => uids.iter().all(|uid| cache.update_flags(&mailbox, *uid, &add, &remove).unwrap_or(false)),
                                None => false,
                            };
                            if all_patched {
                                emit_cached_messages(cache, &mailbox, events).await;
                            } else if mailbox == current_mailbox_id {
                                sync_mailbox(&mut session, &account_id, &current_mailbox_name, &current_mailbox_id, events, cache).await?;
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
                            let _ = events.send(AccountEvent::Error(format!("Couldn't update message flags: {e}"))).await;
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
                            let patched = match cache {
                                Some(cache) => match cache.update_keywords(&mailbox, uid, &add, &remove) {
                                    Ok(patched) => patched,
                                    Err(e) => {
                                        tracing::warn!("failed to update cached keywords: {e}");
                                        false
                                    }
                                },
                                None => false,
                            };
                            if patched {
                                emit_cached_messages(cache, &mailbox, events).await;
                            } else if mailbox == current_mailbox_id {
                                // No cache (or the uid fell outside the
                                // cached window): re-sync so the UI still
                                // sees the new keywords.
                                sync_mailbox(&mut session, &account_id, &current_mailbox_name, &current_mailbox_id, events, cache).await?;
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
                            let all_patched = match cache {
                                Some(cache) => uids.iter().all(|uid| cache.update_keywords(&mailbox, *uid, &add, &remove).unwrap_or(false)),
                                None => false,
                            };
                            if all_patched {
                                emit_cached_messages(cache, &mailbox, events).await;
                            } else if mailbox == current_mailbox_id {
                                sync_mailbox(&mut session, &account_id, &current_mailbox_name, &current_mailbox_id, events, cache).await?;
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
                dirty |= refresh_folder_counts(&mut session, &mut folders[index], &account_id).await;
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
                    let fetch_from = mailbox_meta.exists.saturating_sub(INITIAL_FETCH_LIMIT - 1).max(1);
                    let seq_range = format!("{fetch_from}:*");
                    let fetches: Vec<_> = session.fetch(&seq_range, "(UID BODYSTRUCTURE)").await?.try_collect().await?;

                    // Collect UIDs, newest first.
                    let mut uids: Vec<Uid> = fetches.iter().filter_map(|f| f.uid.map(Uid)).collect();
                    uids.sort_by_key(|u| std::cmp::Reverse(u.0));

                    // Filter out already-cached bodies.
                    if let Some(cache) = cache {
                        uids.retain(|uid| !cache.has_body(&pf.mailboxes[pf.current], *uid, pf.uidvalidity).unwrap_or(false));
                    }

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

                // Fetch up to PREFETCH_BATCH_SIZE bodies, yielding to user
                // commands between each fetch so they are never blocked for
                // more than one body download.
                if !pf.pending_uids.is_empty() {
                    // Check before starting body fetches.
                    if !commands.is_empty() {
                        continue;
                    }
                    let batch: Vec<Uid> = pf.pending_uids.drain(..pf.pending_uids.len().min(PREFETCH_BATCH_SIZE)).collect();
                    let mut fetched = 0usize;
                    for (i, uid) in batch.iter().enumerate() {
                        // A user command may have arrived during the previous
                        // body fetch. If so, put back the remaining UIDs and
                        // break out so the command is processed promptly.
                        if i > 0 && !commands.is_empty() {
                            pf.pending_uids.splice(0..0, batch[i..].iter().cloned());
                            break;
                        }
                        match fetch_body_cached(
                            cache,
                            &mut session,
                            &pf.mailboxes[pf.current],
                            *uid,
                            pf.uidvalidity,
                            pf.structures.get(uid).map(Vec::as_slice),
                        )
                        .await
                        {
                            Ok(Some(_)) => fetched += 1,
                            Ok(None) => {}
                            Err(e) => {
                                tracing::warn!(?uid, "prefetch: body fetch failed: {e}");
                            }
                        }
                    }
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

/// Joins a UID set into the comma-separated sequence-set syntax IMAP's
/// `UID`-prefixed commands accept in place of a single UID - a batch of N
/// messages costs one `STORE`/`MOVE`/`COPY` round trip instead of N.
fn join_uids(uids: &[Uid]) -> String {
    uids.iter().map(|u| u.0.to_string()).collect::<Vec<_>>().join(",")
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
    let uid_set = join_uids(uids);
    for (op, flags) in [('+', add), ('-', remove)] {
        if flags.is_empty() {
            continue;
        }
        let list = flags.join(" ");
        let query = format!("{op}FLAGS.SILENT ({list})");
        let _: Vec<_> = session.uid_store(uid_set.clone(), &query).await?.try_collect().await?;
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
async fn move_message_to_role(session: &mut Session<ImapStream>, folders: &[Mailbox], account_id: &AccountId, uids: &[Uid], role: MailboxRole) -> Result<()> {
    let Some(target) = folders.iter().find(|m| m.role == role) else {
        return Err(Error::NoSuchFolder(role));
    };
    let Some(path) = target.id.0.strip_prefix(&format!("{}:", account_id.0)) else {
        return Ok(());
    };
    move_uids_to_path(session, uids, path).await
}

/// The IMAP move itself, shared by the role-based (`MoveMessages`) and
/// explicit-target (`MoveMessagesTo`) paths: one MOVE over the joined UID
/// set (RFC 6851) when the server advertises it, else COPY + STORE
/// `\Deleted` + EXPUNGE.
async fn move_uids_to_path(session: &mut Session<ImapStream>, uids: &[Uid], path: &str) -> Result<()> {
    let uid_set = join_uids(uids);
    let caps = session.capabilities().await?;
    if caps.has_str("MOVE") {
        session.uid_mv(uid_set, path).await?;
    } else {
        session.uid_copy(uid_set.clone(), path).await?;
        let _: Vec<_> = session.uid_store(uid_set, "+FLAGS (\\Deleted)").await?.try_collect().await?;
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
/// the way JMAP's EmailSubmission does.
async fn send_message(config: &AccountConfig, credentials: &dyn CredentialProvider, session: &mut Session<ImapStream>, folders: &[Mailbox], msg: ComposedMessage) -> Result<()> {
    let (raw, _message_id, recipients) = build_raw_message(&msg);

    let smtp_credential = credentials.smtp_credential().await.map_err(Error::LoginFailed)?;
    send_smtp(&config.smtp, smtp_credential, &msg.from, &recipients, &raw).await?;

    let Some(sent) = folders.iter().find(|m| matches!(m.role, lookout_core::MailboxRole::Sent)) else {
        tracing::warn!("no Sent mailbox found; message was sent but not archived");
        return Ok(());
    };
    let Some(path) = sent.id.0.strip_prefix(&format!("{}:", config.account_id.0)) else {
        return Ok(());
    };
    if let Err(e) = session.append(path, Some("(\\Seen)"), None, raw.as_slice()).await {
        tracing::warn!("message was sent but APPEND to Sent failed: {e}");
    }
    Ok(())
}

/// The account's Drafts folder path from the latest folder list, or `None`
/// if it has no Drafts mailbox (yet).
fn drafts_path<'a>(folders: &'a [Mailbox], account_id: &AccountId) -> Option<&'a str> {
    let drafts = folders.iter().find(|m| matches!(m.role, MailboxRole::Drafts))?;
    drafts.id.0.strip_prefix(&format!("{}:", account_id.0))
}

/// Permanently removes every message in the *currently selected* mailbox
/// whose `Message-ID` header equals `message_id` (bare id, no brackets).
/// `UID SEARCH HEADER` + `\Deleted` + EXPUNGE rather than MOVE so it works
/// on servers without RFC 6851; the EXPUNGE-everything-`\Deleted` caveat
/// from `move_message_to_role` applies, and is harmless here because this
/// crate only ever sets `\Deleted` on the very uids it just flagged.
async fn purge_by_message_id(session: &mut Session<ImapStream>, message_id: &str) -> Result<()> {
    let uids = session.uid_search(format!("HEADER Message-Id <{message_id}>")).await?;
    if uids.is_empty() {
        return Ok(());
    }
    let uid_set = uids.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
    let _: Vec<_> = session.uid_store(uid_set, "+FLAGS.SILENT (\\Deleted)").await?.try_collect().await?;
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

async fn login(config: &AccountConfig, credential: Credential) -> Result<Session<ImapStream>> {
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

    session.map_err(|(e, _client)| Error::Imap(e))
}

async fn list_mailboxes(session: &mut Session<ImapStream>, account_id: &AccountId) -> Result<Vec<Mailbox>> {
    let names: Vec<_> = session.list(Some(""), Some("*")).await?.try_collect().await?;

    let mut mailboxes = Vec::with_capacity(names.len());
    for name in &names {
        let attrs: Vec<String> = name.attributes().iter().map(|a| format!("{a:?}")).collect();
        if attrs.iter().any(|a| a.contains("NoSelect")) {
            continue;
        }
        let delimiter = name.delimiter().and_then(|d| d.chars().next()).unwrap_or('/');
        let display_name = name.name().rsplit(delimiter).next().unwrap_or(name.name()).to_string();
        let role = role_from_special_use(&attrs, &display_name);

        mailboxes.push(Mailbox {
            id: MailboxId::new(account_id, name.name()),
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
        });
    }
    Ok(mailboxes)
}

/// One folder's best-effort STATUS count refresh (RFC 3501 §6.3.10): fills
/// `total`/`unread` (and, for free, `uidnext`/`uidvalidity`) from the server
/// and reports whether anything changed. Never fatal: a folder whose STATUS
/// fails (deleted since LIST, or a server that rejects the command) keeps
/// its LIST-only defaults, so unread counts degrade gracefully on servers
/// without a useful STATUS. Note the crate's `Mailbox` type reports the
/// STATUS `unseen` as a *count*, unlike SELECT's "sequence number of the
/// first unseen message", so it maps straight onto `Mailbox::unread`.
async fn refresh_folder_counts(session: &mut Session<ImapStream>, folder: &mut Mailbox, account_id: &AccountId) -> bool {
    let Some(path) = folder.id.0.strip_prefix(&format!("{}:", account_id.0)) else {
        return false;
    };
    match session.status(path, "(MESSAGES UNSEEN UIDNEXT UIDVALIDITY)").await {
        Ok(meta) => {
            let changed = meta.exists != folder.total || meta.unseen.unwrap_or(0) != folder.unread;
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
/// known folder list onto a freshly listed one, matching by id. A folder
/// that's new since the last list keeps its zeros until the drain reaches it.
/// Without this, every re-list - on connect, on Refresh, after a message move
/// - would reset the whole sidebar to no counts and then fill it back in.
fn carry_counts_forward(folders: &mut [Mailbox], known: &[Mailbox]) {
    for folder in folders {
        if let Some(known) = known.iter().find(|m| m.id == folder.id) {
            folder.total = known.total;
            folder.unread = known.unread;
            folder.uidnext = known.uidnext;
            folder.uidvalidity = known.uidvalidity;
        }
    }
}

/// Persists the folder list to the cache and emits it to the UI - the one
/// place a mutated `folders` becomes visible. Every count update funnels
/// through here so the on-disk copy and the sidebar can never disagree.
async fn publish_folders(folders: &[Mailbox], account_id: &AccountId, cache: Option<&crate::cache::Cache>, events: &async_channel::Sender<AccountEvent>) {
    if let Some(cache) = cache {
        if let Err(e) = cache.replace_mailboxes(account_id, folders) {
            tracing::warn!("failed to cache mailbox list: {e}");
        }
    }
    let _ = events.send(AccountEvent::FoldersUpdated(folders.to_vec())).await;
}

/// The re-list step shared by the Refresh command and the move/draft-create
/// paths: `list_mailboxes` + cache + a `FoldersUpdated` emit, and re-queue
/// every folder's STATUS count for the main loop's cooperative drain. The
/// drain runs these one round trip at a time, yielding to the command queue
/// before each, so these paths never block the session on a whole
/// STATUS-per-folder pass.
///
/// The re-list resets every folder to its LIST-only zero counts, so the
/// counts already learned are carried across by id - otherwise a Refresh (or
/// any message move) would visibly blank the whole sidebar until the new pass
/// caught up.
async fn relist_folders(
    session: &mut Session<ImapStream>,
    folders: &mut Vec<Mailbox>,
    counts_pending: &mut VecDeque<MailboxId>,
    account_id: &AccountId,
    current: &MailboxId,
    cache: Option<&crate::cache::Cache>,
    events: &async_channel::Sender<AccountEvent>,
) -> Result<()> {
    let mut relisted = list_mailboxes(session, account_id).await?;
    carry_counts_forward(&mut relisted, folders);
    *folders = relisted;
    *counts_pending = queue_folder_counts(folders, current);
    publish_folders(folders, account_id, cache, events).await;
    Ok(())
}

/// Refreshes one folder's server-side counts in place and re-emits the
/// folder list when they changed. Used after an IDLE notification so the
/// open folder's sidebar count tracks the new-mail/expunge that just
/// happened instead of waiting for the next full drain. Best-effort: a
/// failed STATUS leaves the existing count and emits nothing.
async fn refresh_one_folder_count(
    session: &mut Session<ImapStream>,
    folders: &mut [Mailbox],
    account_id: &AccountId,
    mailbox_id: &MailboxId,
    cache: Option<&crate::cache::Cache>,
    events: &async_channel::Sender<AccountEvent>,
) {
    let Some(index) = folders.iter().position(|m| m.id == *mailbox_id) else { return };
    if !refresh_folder_counts(session, &mut folders[index], account_id).await {
        return;
    }
    publish_folders(folders, account_id, cache, events).await;
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
    // rather than the HashSet's arbitrary iteration order.
    let mut uid_list: Vec<u32> = uids.into_iter().collect();
    uid_list.sort_unstable();
    let uid_set = uid_list.iter().map(|u| u.to_string()).collect::<Vec<_>>().join(",");
    let fetches: Vec<_> = session
        .uid_fetch(&uid_set, "(UID FLAGS ENVELOPE RFC822.SIZE INTERNALDATE BODYSTRUCTURE)")
        .await?
        .try_collect()
        .await?;
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

async fn sync_mailbox(
    session: &mut Session<ImapStream>,
    account_id: &AccountId,
    folder_path: &str,
    mailbox_id: &MailboxId,
    events: &async_channel::Sender<AccountEvent>,
    cache: Option<&crate::cache::Cache>,
) -> Result<()> {
    let mailbox_meta = session.select(folder_path).await?;
    let uidvalidity = UidValidity(mailbox_meta.uid_validity.unwrap_or(0));
    // The display list shows every message in the folder, so fetch it all:
    // any window - UID or sequence - silently drops the older mail that large
    // folders like Gmail's All Mail exist to show (the account-global UID
    // counter makes a UID window miss still-present messages outright). Full
    // CONDSTORE/QRESYNC incremental sync is Phase 2 - until then every sync
    // is a full re-fetch of the folder's envelope set.
    // `BODYSTRUCTURE` rides along so `summary_from_fetch` can fill in the
    // part structure / `has_attachment` without any body fetch - it's what
    // lets opening a message download only its text parts.
    let fetches: Vec<_> = session
        .fetch("1:*", "(UID FLAGS ENVELOPE RFC822.SIZE INTERNALDATE BODYSTRUCTURE)")
        .await?
        .try_collect()
        .await?;

    let mut messages: Vec<EmailSummary> = fetches.iter().filter_map(|f| summary_from_fetch(mailbox_id, f)).collect();

    let keys = lookout_core::thread::compute_thread_keys(&messages);
    for msg in &mut messages {
        if let Some(key) = keys.get(&msg.uid) {
            msg.thread_key = key.clone();
        }
    }

    // Carry cached snippets forward before `replace_messages` wipes them.
    // The envelope fetch above can't produce a preview, so without this every
    // resync would blank the whole list and re-fetch every body.
    if let Some(cache) = cache {
        match cache.load_previews(mailbox_id) {
            Ok(previews) if !previews.is_empty() => {
                for msg in &mut messages {
                    msg.preview = previews.get(&msg.uid).cloned();
                }
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("failed to load cached previews for {mailbox_id}: {e}"),
        }
    }

    tracing::debug!(account = %account_id, mailbox = %folder_path, exists = mailbox_meta.exists, count = messages.len(), "synced mailbox");
    emit_messages(mailbox_id, uidvalidity, &messages, events, cache).await;

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
    Ok(())
}

/// Emits whatever envelope summaries are cached on disk for `mailbox_id`,
/// minus currently-snoozed messages, as a `MessagesUpdated` event. Used for
/// instant paint on folder switch - both when `SyncMailbox` wakes the session
/// (pre-IDLE teardown) and when it's processed out of the drain queue with a
/// cache hit. A no-op when nothing is cached.
async fn emit_cached_messages(cache: Option<&crate::cache::Cache>, mailbox_id: &MailboxId, events: &async_channel::Sender<AccountEvent>) {
    let Some(cache) = cache else { return };
    if let Ok(cached) = cache.load_messages(mailbox_id) {
        if !cached.is_empty() {
            let snoozed = cache.active_snoozed_uids(mailbox_id, chrono::Utc::now()).unwrap_or_default();
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
}

/// Emits the cached summaries for `mailbox_id` (minus snoozed) after one has
/// been deleted, publishing even an *empty* remaining set. Unlike
/// `emit_cached_messages`'s no-op-on-empty, the empty set is meaningful here:
/// the caller has just removed a message, so "no cached rows left" means the
/// list should be cleared, not "nothing cached yet, don't blank the list".
async fn emit_cached_messages_after_removal(cache: &crate::cache::Cache, mailbox_id: &MailboxId, events: &async_channel::Sender<AccountEvent>) {
    if let Ok(cached) = cache.load_messages(mailbox_id) {
        let snoozed = cache.active_snoozed_uids(mailbox_id, chrono::Utc::now()).unwrap_or_default();
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
async fn emit_messages(
    mailbox_id: &MailboxId,
    uidvalidity: UidValidity,
    messages: &[EmailSummary],
    events: &async_channel::Sender<AccountEvent>,
    cache: Option<&crate::cache::Cache>,
) {
    let mut messages = messages.to_vec();
    if let Some(cache) = cache {
        if let Err(e) = cache.replace_messages(mailbox_id, uidvalidity, &messages) {
            tracing::warn!("failed to cache messages for {mailbox_id}: {e}");
        }
        // Feeds the composer's recipient autocomplete. Kept here rather than
        // in `replace_messages` because the address book is cumulative -
        // `replace_messages` wipes and rewrites a mailbox's window each sync,
        // and addresses must survive that.
        if let Err(e) = cache.record_addresses(&messages) {
            tracing::warn!("failed to record addresses for {mailbox_id}: {e}");
        }
        if let Ok(snoozed) = cache.active_snoozed_uids(mailbox_id, chrono::Utc::now()) {
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
    cache: Option<&crate::cache::Cache>,
) -> Result<()> {
    let mut wanted: Vec<Uid> = messages.iter().filter(|m| m.preview.is_none()).map(|m| m.uid).collect();
    if wanted.is_empty() {
        return Ok(());
    }
    // Newest first: the top of the list is what the user is looking at.
    wanted.sort_by_key(|uid| std::cmp::Reverse(uid.0));
    wanted.truncate(PREVIEW_FETCH_LIMIT);

    let uid_set = wanted.iter().map(|u| u.0.to_string()).collect::<Vec<_>>().join(",");
    let query = format!("(UID BODY.PEEK[]<0.{PREVIEW_FETCH_BYTES}>)");
    let fetches: Vec<_> = session.uid_fetch(&uid_set, &query).await?.try_collect().await?;

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
async fn fetch_raw_message_cached(
    cache: Option<&crate::cache::Cache>,
    session: &mut Session<ImapStream>,
    mailbox: &MailboxId,
    uid: Uid,
    uidvalidity: UidValidity,
) -> Result<Option<Vec<u8>>> {
    if let Some(cache) = cache {
        match cache.load_raw_message(mailbox, uid, uidvalidity) {
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
    if let Some(cache) = cache {
        if let Err(e) = cache.store_raw_message(mailbox, uid, uidvalidity, &raw) {
            tracing::warn!(?mailbox, uid = uid.0, "failed to cache raw message: {e}");
        }
    }
    Ok(Some(raw))
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

    let headers = fetch
        .section(&async_imap::imap_proto::types::SectionPath::Full(async_imap::imap_proto::types::MessageSection::Header))
        .map(crate::body::parse_headers_section)
        .unwrap_or_default();

    let mut fetched: Vec<(String, Vec<u8>)> = Vec::new();
    for part in &text_parts {
        // `BODY.PEEK[1.2]` parses back into the same `SectionPath::Part`
        // value the server's response carries, so `Fetch::section` can match
        // it by equality.
        let path = async_imap::imap_proto::types::SectionPath::Part(part.part_number.split('.').filter_map(|n| n.parse().ok()).collect(), None);
        if let Some(bytes) = fetch.section(&path) {
            fetched.push((part.part_number.clone(), bytes.to_vec()));
        }
    }
    if fetched.is_empty() {
        return Ok(None);
    }

    Ok(Some(crate::body::assemble_body_from_parts(uid, headers, parts, &fetched)))
}

/// Fetches one attachment part's wire bytes for the on-demand
/// `AccountCommand::FetchAttachment`, keyed by its BODYSTRUCTURE-derived part
/// number, and returns them *transfer-decoded* (base64/quoted-printable undone
/// via `transfer_part_bytes`). An embedded `message/rfc822` attachment is
/// returned whole, not re-parsed. Returns `None` if the server didn't return
/// the section (rather than erroring), so the caller can no-op gracefully.
async fn fetch_attachment_part(session: &mut Session<ImapStream>, uid: Uid, part: &BodyPart) -> Result<Option<Vec<u8>>> {
    // `BODY.PEEK[1.2]` parses back into the same `SectionPath::Part` the
    // server's response carries, exactly as `fetch_body_partial` relies on.
    let path = async_imap::imap_proto::types::SectionPath::Part(part.part_number.split('.').filter_map(|n| n.parse().ok()).collect(), None);
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
    cache: Option<&crate::cache::Cache>,
    session: &mut Session<ImapStream>,
    mailbox: &MailboxId,
    uid: Uid,
    uidvalidity: UidValidity,
    known_structure: Option<&[BodyPart]>,
) -> Result<Option<EmailBody>> {
    if let Some(cache) = cache {
        match cache.load_body(mailbox, uid, uidvalidity) {
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
        None => cache.and_then(|c| c.load_summary(mailbox, uid).ok().flatten()).and_then(|s| s.structure),
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
    if let Some(cache) = cache {
        if let Some(body) = &body {
            if let Err(e) = cache.store_body(mailbox, uid, uidvalidity, body) {
                tracing::warn!(?mailbox, uid = uid.0, "failed to cache message body: {e}");
            }
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

    #[test]
    fn a_relist_keeps_the_counts_already_learned() {
        // LIST reports no counts, so a re-list arrives all-zero; without the
        // carry-over the sidebar would blank on every Refresh and message move.
        let account = AccountId("acc".into());
        let known = vec![mailbox(&account, "INBOX", MailboxRole::Inbox, 7)];
        let mut relisted = vec![mailbox(&account, "INBOX", MailboxRole::Inbox, 0), mailbox(&account, "New", MailboxRole::Custom, 0)];
        carry_counts_forward(&mut relisted, &known);
        assert_eq!(relisted[0].unread, 7);
        // A folder that's new since the last list has nothing to carry over
        // and keeps its zeros until the drain reaches it.
        assert_eq!(relisted[1].unread, 0);
    }
}
