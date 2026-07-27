use std::time::Duration;

use async_imap::Session;
use futures::TryStreamExt;
use lookout_core::mailbox::role_from_special_use;
use lookout_core::{AccountId, EmailBody, EmailSummary, Mailbox, MailboxId, Uid, UidValidity};

use crate::auth::XOAuth2Authenticator;
use crate::body::parse_body;
use crate::config::{AccountConfig, Credential};
use crate::connection::{connect_tls, ImapStream};
use crate::envelope::summary_from_fetch;
use crate::error::{Error, Result};

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
    FetchBody { mailbox: MailboxId, uid: Uid },
    /// Force a folder-list + current-mailbox resync outside of IDLE's own cadence.
    Refresh,
    Shutdown,
}

#[derive(Debug)]
pub enum AccountEvent {
    ConnectionStateChanged(ConnectionState),
    FoldersUpdated(Vec<Mailbox>),
    MessagesUpdated { mailbox: MailboxId, messages: Vec<EmailSummary> },
    BodyFetched { mailbox: MailboxId, uid: Uid, body: EmailBody },
    Error(String),
}

/// Fetches a fresh credential immediately before each (re)connect attempt.
/// `lookout-mail` never caches credentials itself; the app crate implements
/// this trait against `lookout-goa`, keeping this crate free of D-Bus
/// concerns and independently testable (see the `imap_integration` test).
#[async_trait::async_trait]
pub trait CredentialProvider: Send + Sync {
    async fn imap_credential(&self) -> std::result::Result<Credential, String>;
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
    let mut backoff = Duration::from_secs(1);
    const MAX_BACKOFF: Duration = Duration::from_secs(60);

    loop {
        let _ = events.send(AccountEvent::ConnectionStateChanged(ConnectionState::Connecting)).await;
        match connect_and_run(&config, credentials.as_ref(), &commands, &events).await {
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

        tokio::time::sleep(backoff).await;
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
) -> Result<ShutdownReason> {
    let credential = credentials.imap_credential().await.map_err(Error::LoginFailed)?;
    let mut session = login(config, credential).await?;

    let account_id = config.account_id.clone();
    let folders = list_mailboxes(&mut session, &account_id).await?;
    let _ = events.send(AccountEvent::FoldersUpdated(folders.clone())).await;

    let inbox_id = MailboxId::new(&account_id, "INBOX");
    sync_mailbox(&mut session, &account_id, "INBOX", &inbox_id, events).await?;

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
                sync_mailbox(&mut session, &account_id, &current_mailbox_name, &current_mailbox_id, events).await?;
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
                    let folders = list_mailboxes(&mut session, &account_id).await?;
                    let _ = events.send(AccountEvent::FoldersUpdated(folders)).await;
                    sync_mailbox(&mut session, &account_id, &current_mailbox_name, &current_mailbox_id, events).await?;
                }
                AccountCommand::SyncMailbox(mailbox_id) => {
                    // MailboxId is "<account_id>:<folder path>"; recover the folder path.
                    if let Some(path) = mailbox_id.0.strip_prefix(&format!("{}:", account_id.0)) {
                        current_mailbox_name = path.to_string();
                        current_mailbox_id = mailbox_id;
                        sync_mailbox(&mut session, &account_id, &current_mailbox_name, &current_mailbox_id, events)
                            .await?;
                    }
                }
                AccountCommand::FetchBody { mailbox, uid } => {
                    if mailbox != current_mailbox_id {
                        tracing::warn!(
                            "FetchBody requested for a mailbox other than the currently selected one; ignoring"
                        );
                        continue;
                    }
                    if let Some(body) = fetch_body(&mut session, uid).await? {
                        let _ = events.send(AccountEvent::BodyFetched { mailbox, uid, body }).await;
                    }
                }
            }
        }
    }
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
) -> Result<()> {
    let mailbox_meta = session.select(folder_path).await?;
    let uid_next = mailbox_meta.uid_next.unwrap_or(1);
    let fetch_from = uid_next.saturating_sub(INITIAL_FETCH_LIMIT).max(1);
    let uid_range = format!("{fetch_from}:*");

    let fetches: Vec<_> =
        session.uid_fetch(&uid_range, "(UID FLAGS ENVELOPE RFC822.SIZE INTERNALDATE)").await?.try_collect().await?;

    let mut messages: Vec<EmailSummary> = fetches.iter().filter_map(|f| summary_from_fetch(mailbox_id, f)).collect();

    let keys = lookout_core::thread::compute_thread_keys(&messages);
    for msg in &mut messages {
        if let Some(key) = keys.get(&msg.uid) {
            msg.thread_key = key.clone();
        }
    }

    tracing::debug!(account = %account_id, mailbox = %folder_path, count = messages.len(), "synced mailbox");
    let _ = events.send(AccountEvent::MessagesUpdated { mailbox: mailbox_id.clone(), messages }).await;
    Ok(())
}

/// Fetches and parses the full body of `uid` in whatever mailbox is
/// currently SELECTed. Uses `BODY.PEEK[]` rather than `BODY[]`/`RFC822` so
/// reading a message doesn't implicitly set `\Seen` server-side - the UI
/// layer decides if/when to mark as read, matching Bulwark's configurable
/// mark-as-read-delay behavior.
async fn fetch_body(session: &mut Session<ImapStream>, uid: Uid) -> Result<Option<EmailBody>> {
    let fetches: Vec<_> = session.uid_fetch(uid.0.to_string(), "BODY.PEEK[]").await?.try_collect().await?;
    let Some(fetch) = fetches.into_iter().find(|f| f.uid == Some(uid.0)) else {
        return Ok(None);
    };
    let Some(raw) = fetch.body() else {
        return Ok(None);
    };
    Ok(parse_body(uid, raw))
}
