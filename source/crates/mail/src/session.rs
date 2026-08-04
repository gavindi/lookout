use std::time::Duration;

use async_imap::Session;
use futures::TryStreamExt;
use lookout_core::mailbox::role_from_special_use;
use lookout_core::{AccountId, EmailBody, EmailSummary, Mailbox, MailboxId, MailboxRole, Uid, UidValidity};

use crate::auth::XOAuth2Authenticator;
use crate::body::{parse_body, preview_from_raw};
use crate::config::{AccountConfig, Credential};
use crate::connection::{connect_tls, ImapStream};
use crate::envelope::summary_from_fetch;
use crate::error::{Error, Result};
use crate::send::{build_raw_message, send_smtp, ComposedMessage};

/// How many of the most recent messages to fetch on initial folder sync.
/// Cheap (envelope-only) so this can be generous; full CONDSTORE/QRESYNC
/// incremental sync is Phase 2 - see the module docs.
const INITIAL_FETCH_LIMIT: u32 = 200;

/// How long a single IDLE wait runs before we re-enter it purely as a
/// keepalive, well under RFC 2177's ~29-minute server timeout. On-demand
/// commands don't wait for this timeout: the IDLE wait future is raced
/// against `commands.recv()` directly (see `connect_and_run`'s main loop),
/// and dropping the `Handle`'s `StopSource` cancels the wait immediately,
/// so a command arriving mid-IDLE is picked up right away.
const IDLE_SLICE: Duration = Duration::from_secs(25 * 60);

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
    /// Select a mailbox and (re)fetch its most recent envelopes.
    SyncMailbox(MailboxId),
    /// Fetch the full body of a message in the *currently selected* mailbox
    /// (fetching a body from a different mailbox would require a SELECT,
    /// which would drop out of IDLE on that other folder - out of scope for
    /// Phase 1, where only the open folder's messages are readable).
    FetchBody {
        mailbox: MailboxId,
        uid: Uid,
    },
    /// Force a folder-list + current-mailbox resync outside of IDLE's own cadence.
    Refresh,
    /// Send a composed message over SMTP, then `APPEND` it to the account's
    /// Sent mailbox (two explicit steps - IMAP has no JMAP-style implicit
    /// filing on submit). If no Sent mailbox can be identified, the message
    /// is still sent; only the archival copy is skipped (logged as a warning).
    SendMessage(ComposedMessage),
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
    /// Client-side only - IMAP has no native snooze. Records `until` in the
    /// local cache and hides the message from `MessagesUpdated` until that
    /// time passes.
    SnoozeMessage {
        mailbox: MailboxId,
        uid: Uid,
        until: chrono::DateTime<chrono::Utc>,
    },
    Shutdown,
}

#[derive(Debug)]
pub enum AccountEvent {
    ConnectionStateChanged(ConnectionState),
    FoldersUpdated(Vec<Mailbox>),
    MessagesUpdated { mailbox: MailboxId, messages: Vec<EmailSummary> },
    BodyFetched { mailbox: MailboxId, uid: Uid, body: EmailBody },
    SendCompleted,
    MessageMoved { role: MailboxRole },
    MessageSnoozed,
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
                let _ = events.send(AccountEvent::Error(e.to_string())).await;
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
    if let Some(cache) = cache {
        if let Err(e) = cache.replace_mailboxes(&account_id, &folders) {
            tracing::warn!("failed to cache mailbox list: {e}");
        }
    }
    let _ = events.send(AccountEvent::FoldersUpdated(folders.clone())).await;

    let inbox_id = MailboxId::new(&account_id, "INBOX");
    sync_mailbox(&mut session, &account_id, "INBOX", &inbox_id, events, cache).await?;

    let mut current_mailbox_name = "INBOX".to_string();
    let mut current_mailbox_id = inbox_id;

    loop {
        let _ = events.send(AccountEvent::ConnectionStateChanged(ConnectionState::Idle)).await;

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
        enum Wake {
            Idle(std::result::Result<async_imap::extensions::idle::IdleResponse, async_imap::error::Error>),
            Command(AccountCommand),
            ChannelClosed,
        }
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
            if let Some(cache) = cache {
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
        }

        drop(stop_source);
        session = handle.done().await?;

        let _ = events.send(AccountEvent::ConnectionStateChanged(ConnectionState::Busy)).await;

        let mut woke_on_command = None;
        match wake {
            // A server notification during IDLE (EXISTS/EXPUNGE/etc) means
            // the currently-selected mailbox changed; re-fetch its envelope
            // window. This is a full bounded re-fetch rather than a
            // CONDSTORE delta - see INITIAL_FETCH_LIMIT's doc comment.
            Wake::Idle(Ok(async_imap::extensions::idle::IdleResponse::Timeout)) => {}
            Wake::Idle(Ok(_)) => {
                sync_mailbox(&mut session, &account_id, &current_mailbox_name, &current_mailbox_id, events, cache).await?;
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
                    folders = list_mailboxes(&mut session, &account_id).await?;
                    if let Some(cache) = cache {
                        if let Err(e) = cache.replace_mailboxes(&account_id, &folders) {
                            tracing::warn!("failed to cache mailbox list: {e}");
                        }
                    }
                    let _ = events.send(AccountEvent::FoldersUpdated(folders.clone())).await;
                    sync_mailbox(&mut session, &account_id, &current_mailbox_name, &current_mailbox_id, events, cache).await?;
                }
                AccountCommand::SyncMailbox(mailbox_id) => {
                    // MailboxId is "<account_id>:<folder path>"; recover the folder path.
                    if let Some(path) = mailbox_id.0.strip_prefix(&format!("{}:", account_id.0)) {
                        current_mailbox_name = path.to_string();
                        current_mailbox_id = mailbox_id;
                        sync_mailbox(&mut session, &account_id, &current_mailbox_name, &current_mailbox_id, events, cache).await?;
                    }
                }
                AccountCommand::FetchBody { mailbox, uid } => {
                    if mailbox != current_mailbox_id {
                        tracing::warn!("FetchBody requested for a mailbox other than the currently selected one; ignoring");
                        continue;
                    }
                    // The mailbox's uidvalidity guards the body cache against
                    // serving a stale body for a recycled uid after the
                    // mailbox was re-created (see `Cache::load_body`). The
                    // `UidValidity(0)` fallback (mailbox not in the list) is
                    // a deliberate cache-miss sentinel: no row can match 0.
                    let uidvalidity = folders.iter().find(|m| m.id == mailbox).map(|m| m.uidvalidity).unwrap_or(UidValidity(0));
                    let started = std::time::Instant::now();
                    let raw = fetch_body_cached(cache, &mut session, &mailbox, uid, uidvalidity).await?;
                    tracing::debug!(?mailbox, uid = uid.0, elapsed_ms = started.elapsed().as_millis(), "FetchBody: raw message ready");
                    if let Some(raw) = raw {
                        if let Some(body) = parse_body(uid, &raw) {
                            let _ = events.send(AccountEvent::BodyFetched { mailbox, uid, body }).await;
                        }
                    }
                }
                AccountCommand::SendMessage(msg) => match send_message(config, credentials, &mut session, &folders, msg).await {
                    Ok(()) => {
                        let _ = events.send(AccountEvent::SendCompleted).await;
                    }
                    Err(e) => {
                        let _ = events.send(AccountEvent::Error(format!("Couldn't send message: {e}"))).await;
                    }
                },
                // Already connected - nothing to reconnect. This variant
                // only does something useful while backed off between
                // connection attempts, see `run_account_session`.
                AccountCommand::Reconnect => {}
                AccountCommand::MoveMessage { mailbox, uid, role } => {
                    if mailbox != current_mailbox_id {
                        tracing::warn!("MoveMessage requested for a mailbox other than the currently selected one; ignoring");
                        continue;
                    }
                    match move_message_to_role(&mut session, &folders, &account_id, uid, role).await {
                        Ok(()) => {
                            let _ = events.send(AccountEvent::MessageMoved { role }).await;
                            folders = list_mailboxes(&mut session, &account_id).await?;
                            if let Some(cache) = cache {
                                if let Err(e) = cache.replace_mailboxes(&account_id, &folders) {
                                    tracing::warn!("failed to cache mailbox list: {e}");
                                }
                            }
                            let _ = events.send(AccountEvent::FoldersUpdated(folders.clone())).await;
                            sync_mailbox(&mut session, &account_id, &current_mailbox_name, &current_mailbox_id, events, cache).await?;
                        }
                        Err(e) => {
                            let _ = events.send(AccountEvent::Error(format!("Couldn't move message: {e}"))).await;
                        }
                    }
                }
                AccountCommand::SnoozeMessage { mailbox, uid, until } => {
                    if mailbox != current_mailbox_id {
                        tracing::warn!("SnoozeMessage requested for a mailbox other than the currently selected one; ignoring");
                        continue;
                    }
                    if let Some(cache) = cache {
                        if let Err(e) = cache.snooze_message(&mailbox, uid, until) {
                            tracing::warn!("failed to record snooze: {e}");
                        }
                    }
                    let _ = events.send(AccountEvent::MessageSnoozed).await;
                    sync_mailbox(&mut session, &account_id, &current_mailbox_name, &current_mailbox_id, events, cache).await?;
                }
            }
        }
    }
}

/// Moves `uid` from the currently selected mailbox into the account's
/// mailbox with special-use role `role`, via IMAP MOVE (RFC 6851) if the
/// server advertises it, else COPY + STORE `\Deleted` + EXPUNGE.
async fn move_message_to_role(session: &mut Session<ImapStream>, folders: &[Mailbox], account_id: &AccountId, uid: Uid, role: MailboxRole) -> Result<()> {
    let Some(target) = folders.iter().find(|m| m.role == role) else {
        return Err(Error::NoSuchFolder(role));
    };
    let Some(path) = target.id.0.strip_prefix(&format!("{}:", account_id.0)) else {
        return Ok(());
    };
    let caps = session.capabilities().await?;
    if caps.has_str("MOVE") {
        session.uid_mv(uid.0.to_string(), path).await?;
    } else {
        session.uid_copy(uid.0.to_string(), path).await?;
        let _: Vec<_> = session.uid_store(uid.0.to_string(), "+FLAGS (\\Deleted)").await?.try_collect().await?;
        // NB: expunges every \Deleted-flagged message in the currently
        // selected mailbox, not just this one - a documented, accepted
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
    if let Err(e) = session.append(path, Some("\\Seen"), None, raw.as_slice()).await {
        tracing::warn!("message was sent but APPEND to Sent failed: {e}");
    }
    Ok(())
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
            uidvalidity: UidValidity(0), // Filled in by sync_mailbox() once the folder is SELECTed.
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

async fn sync_mailbox(
    session: &mut Session<ImapStream>,
    account_id: &AccountId,
    folder_path: &str,
    mailbox_id: &MailboxId,
    events: &async_channel::Sender<AccountEvent>,
    cache: Option<&crate::cache::Cache>,
) -> Result<()> {
    let mailbox_meta = session.select(folder_path).await?;
    let uid_next = mailbox_meta.uid_next.unwrap_or(1);
    let uidvalidity = UidValidity(mailbox_meta.uid_validity.unwrap_or(0));
    let fetch_from = uid_next.saturating_sub(INITIAL_FETCH_LIMIT).max(1);
    let uid_range = format!("{fetch_from}:*");

    let fetches: Vec<_> = session.uid_fetch(&uid_range, "(UID FLAGS ENVELOPE RFC822.SIZE INTERNALDATE)").await?.try_collect().await?;

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

    tracing::debug!(account = %account_id, mailbox = %folder_path, count = messages.len(), "synced mailbox");
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
/// currently SELECTed. Uses `BODY.PEEK[]` rather than `BODY[]`/`RFC822` so
/// reading a message doesn't implicitly set `\Seen` server-side - the UI
/// layer decides if/when to mark as read, matching Bulwark's configurable
/// mark-as-read-delay behavior. The raw bytes (not a parsed body) are
/// returned because they're also what the body cache stores: a cache hit
/// re-parses them, which is far cheaper than the network fetch it avoids.
async fn fetch_body(session: &mut Session<ImapStream>, uid: Uid) -> Result<Option<Vec<u8>>> {
    let fetches: Vec<_> = session.uid_fetch(uid.0.to_string(), "BODY.PEEK[]").await?.try_collect().await?;
    let Some(fetch) = fetches.into_iter().find(|f| f.uid == Some(uid.0)) else {
        return Ok(None);
    };
    Ok(fetch.body().map(|body| body.to_vec()))
}

/// Resolves the raw body of `uid` in the currently SELECTed mailbox, serving
/// a previously-fetched copy from the on-disk cache when one exists (no
/// network round trip) and storing freshly-fetched bodies back into it, so
/// re-opening a message doesn't re-download it. `uidvalidity` is passed
/// through to the cache so a recycled uid can never resolve to another
/// message's body.
async fn fetch_body_cached(
    cache: Option<&crate::cache::Cache>,
    session: &mut Session<ImapStream>,
    mailbox: &MailboxId,
    uid: Uid,
    uidvalidity: UidValidity,
) -> Result<Option<Vec<u8>>> {
    if let Some(cache) = cache {
        match cache.load_body(mailbox, uid, uidvalidity) {
            Ok(Some(raw)) => {
                tracing::debug!(?mailbox, uid = uid.0, "FetchBody: served from disk cache");
                return Ok(Some(raw));
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(?mailbox, uid = uid.0, "failed to read cached message body: {e}"),
        }
    }
    let raw = fetch_body(session, uid).await?;
    if let Some(cache) = cache {
        if let Some(raw) = &raw {
            if let Err(e) = cache.store_body(mailbox, uid, uidvalidity, raw) {
                tracing::warn!(?mailbox, uid = uid.0, "failed to cache message body: {e}");
            }
        }
    }
    Ok(raw)
}
