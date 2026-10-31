//! The Graph mail account-session actor (MAIL_GRAPH_OVERVIEW.md item 5.7.5,
//! A-06).
//!
//! A second implementation of the mail session seam: `run_account_session`'s
//! `AccountCommand`/`AccountEvent` contract, driven by Microsoft Graph instead
//! of IMAP/SMTP. The app's event loop, message list, reading pane, composer and
//! outbox are untouched - only the transport behind the seam changes.
//!
//! What this actor owns:
//!
//! - **Sync.** The folder hierarchy through `/me/mailFolders/delta` (the plain
//!   list is top-level only) and each folder's messages through
//!   `/me/mailFolders/{id}/messages/delta`. There is no push: change
//!   notifications need a public HTTPS `notificationUrl` a desktop app cannot
//!   host (R1), so the actor polls - the open folder on every
//!   [`GraphMailSessionConfig::poll_interval`] and one other folder per tick in
//!   a rotation. Each folder's `@odata.deltaLink` is persisted in the cache
//!   (item 5.7.8), so a restart resumes incrementally instead of re-reading
//!   every folder; an expired cursor is cleared and the folder resnapshots
//!   under the same Uids.
//! - **Identity.** Graph ids map onto the IMAP-shaped `MailboxId`/`Uid`/
//!   `UidValidity` through items 5.7.3's helpers and the cache's `graph_uids`
//!   map, so a moved message keeps its Uid and the rest of Lookout never learns
//!   the transport changed.
//! - **Cache.** The same per-account `lookout_mail::Cache` an IMAP account uses
//!   (item 5.7.4): `upsert_messages` for a delta, `delete_messages` for
//!   tombstones, and `apply_remote_mail_properties` for categories, importance,
//!   Focused/Other and the follow-up flag.
//! - **Writes.** Native Graph `PATCH`/`POST`s replace the overlay+mirror dance:
//!   read state, flag, categories, importance, Focused/Other, move, copy,
//!   delete, snooze (local), drafts, folder operations and the outbox
//!   (`sendMail`).
//!
//! Known gaps, reconciled in item 5.7.7: no IDLE (poll latency), no remote
//! `$search` parity (local FTS5 stays primary), no IMAP keywords/Sieve, no Pin
//! (`\Flagged` is a follow-up flag on IMAP but Graph has no separate Pin), and
//! `MoveMailbox` has no Graph counterpart.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use lookout_core::{AccountId, BodyPart, EmailAddress, EmailSummary, Importance, InferenceClassification, Mailbox, MailboxId, MailboxRole, SystemFlagBit, Uid, UidValidity};
use lookout_mail::body::parse_body;
use lookout_mail::session::{outbox_items, AccountCommand, AccountEvent, ConnectionState, DraftReplace, FocusedScope};
use lookout_mail::{build_draft_raw_message, Cache, ComposedMessage, OutboxMessage, RemoteMailProperties};

use crate::error::GraphError;

use super::{
    allocation_order, attachment_id_from_part, body_part_from_attachment, email_body_from_graph, email_summary_from_graph, graph_folder_id, graph_mailbox_id, mailbox_from_graph,
    MessagePatch, MessageSummaryDelta, WellKnownFolders,
};

/// How often the session polls when the caller doesn't pick an interval.
/// Graph has no push (R1), so this is the new-mail latency.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(45);

/// How many polls happen between folder-hierarchy delta reads. Message deltas
/// don't carry counts; the folder delta refreshes them (and picks up new,
/// renamed and deleted folders) on this slower cadence.
const FOLDER_REFRESH_POLLS: u32 = 4;

/// How many outbox retries before a row is left `failed` for the user, the
/// same budget `lookout-mail`'s session uses (item 1.14).
const MAX_OUTBOX_ATTEMPTS: u32 = 5;

/// Most results a `SearchMailbox` fallback asks Graph for.
const MAX_SEARCH_RESULTS: usize = 200;

/// The account-wide cache handle, mirroring `lookout_mail::session`'s own.
/// Cache work runs on the blocking pool so SQLite and JSON never stall the
/// async worker this actor shares with every other account.
type CacheHandle = Arc<Option<Cache>>;

/// Everything the actor needs to speak for one account beyond the HTTP client.
#[derive(Clone, Debug)]
pub struct GraphMailSessionConfig {
    pub account_id: AccountId,
    /// Interprets a follow-up flag's due date (see `super`'s module notes).
    pub zone: Option<chrono_tz::Tz>,
    /// How often the open folder is polled. New-mail latency is this, plus one
    /// delta round trip - there is no push (R1).
    pub poll_interval: Duration,
    /// The account's own addresses and names, for `@mentions` detection in
    /// previews (M-25) - the same context the IMAP session installs.
    pub mention_addresses: Vec<String>,
    pub mention_names: Vec<String>,
}

impl GraphMailSessionConfig {
    pub fn new(account_id: AccountId) -> Self {
        GraphMailSessionConfig {
            account_id,
            zone: None,
            poll_interval: DEFAULT_POLL_INTERVAL,
            mention_addresses: Vec::new(),
            mention_names: Vec::new(),
        }
    }
}

/// Runs one Graph mail account's session on the calling task (spawn this onto
/// the shared worker, as `run_account_session` is). Returns when the app sends
/// `Shutdown` or drops both command channels.
///
/// `commands` is the background queue and `interactive_commands` the
/// interactive one, with the same priority contract as the IMAP session: a
/// user's message click is never queued behind a sync. Unlike IMAP a Graph
/// error is per call, not per connection, so the actor keeps running and
/// retries on its poll cadence; a consent failure is surfaced once.
pub async fn run_graph_mail_session(
    config: GraphMailSessionConfig,
    client: super::MailClient,
    commands: async_channel::Receiver<AccountCommand>,
    interactive_commands: async_channel::Receiver<AccountCommand>,
    events: async_channel::Sender<AccountEvent>,
) {
    // Opening runs the file open, pragmas, DDL and migrations, so it goes to
    // the blocking pool like every other cache call (`cache_op`).
    let open_id = config.account_id.clone();
    let cache: CacheHandle = Arc::new(match tokio::task::spawn_blocking(move || Cache::open(&open_id)).await {
        Ok(Ok(cache)) => Some(cache),
        Ok(Err(e)) => {
            tracing::warn!("couldn't open the local cache, continuing without it: {e}");
            None
        }
        Err(e) => {
            tracing::warn!("the local cache open task failed, continuing without it: {e}");
            None
        }
    });
    if let Some(cache) = cache.as_ref() {
        cache.set_mention_matcher(lookout_core::MentionMatcher::new(&config.mention_addresses, &config.mention_names));
    }

    // Search-index backfill runs off the connect path, as the IMAP session's
    // does. Maintenance runs *before* the first sync and is awaited: it prunes
    // `graph_uids` rows no cached message uses (item 5.7.3), and a concurrent
    // prune could otherwise drop a mapping between this session allocating it
    // and writing the message that uses it.
    let backfill = cache.clone();
    tokio::task::spawn_blocking(move || {
        if let Some(cache) = backfill.as_ref() {
            if let Err(e) = cache.backfill_search_index() {
                tracing::warn!("failed to backfill the search index: {e}");
            }
        }
    });
    let maintenance_cache = cache.clone();
    let maintenance = tokio::task::spawn_blocking(move || match maintenance_cache.as_ref() {
        Some(cache) => cache.run_maintenance(),
        None => Ok(()),
    });
    match maintenance.await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!("failed to run cache maintenance: {e}"),
        Err(e) => tracing::warn!("cache maintenance task failed: {e}"),
    }

    let mut session = GraphMailSession {
        account_id: config.account_id,
        client,
        cache,
        events,
        zone: config.zone,
        poll_interval: config.poll_interval,
        next_poll: Utc::now(),
        polls_since_folder_refresh: 0,
        folders: Vec::new(),
        well_known: WellKnownFolders::default(),
        folder_delta_cursor: None,
        folder_cursors: HashMap::new(),
        current: None,
        category_names: HashMap::new(),
        copy_ledger: HashMap::new(),
        next_outbox_due: None,
        next_snooze_wake: None,
        poll_sweep: 0,
    };

    // Fast first paint - cached folders, the open folder's messages, the
    // outbox and the flagged list - before any network round trip.
    session.replay_cache().await;
    session.emit_sieve_advertised().await;

    let _ = session.events.send(AccountEvent::ConnectionStateChanged(ConnectionState::Connecting)).await;
    session.initial_sync().await;

    loop {
        // Drain whatever is already queued before waiting: interactive first,
        // so a click is answered ahead of background work, and both ahead of
        // the outbox drain, so a cancel queued before a row comes due wins.
        let mut shutdown = false;
        while let Ok(command) = interactive_commands.try_recv() {
            if !session.handle_command(command).await {
                shutdown = true;
                break;
            }
        }
        if shutdown {
            break;
        }
        while let Ok(command) = commands.try_recv() {
            if !session.handle_command(command).await {
                shutdown = true;
                break;
            }
        }
        if shutdown {
            break;
        }

        session.service_due().await;

        let sleep = session.next_sleep();
        tokio::select! {
            biased;
            received = interactive_commands.recv() => match received {
                Ok(command) => {
                    if !session.handle_command(command).await {
                        break;
                    }
                }
                Err(_) => break,
            },
            received = commands.recv() => match received {
                Ok(command) => {
                    if !session.handle_command(command).await {
                        break;
                    }
                }
                Err(_) => break,
            },
            _ = tokio::time::sleep(sleep) => session.on_deadline().await,
        }
    }

    let _ = session.events.send(AccountEvent::ConnectionStateChanged(ConnectionState::Disconnected)).await;
}

/// One account's live session state. Methods are the seam's command handlers.
struct GraphMailSession {
    account_id: AccountId,
    client: super::MailClient,
    cache: CacheHandle,
    events: async_channel::Sender<AccountEvent>,
    zone: Option<chrono_tz::Tz>,
    poll_interval: Duration,
    /// When the next poll runs.
    next_poll: DateTime<Utc>,
    polls_since_folder_refresh: u32,
    folders: Vec<Mailbox>,
    well_known: WellKnownFolders,
    /// The folder-hierarchy delta cursor (`/me/mailFolders/delta`).
    folder_delta_cursor: Option<String>,
    /// Each folder's messages delta cursor, loaded from and persisted to the
    /// cache (item 5.7.8). A restart resumes from these; an expired one is
    /// cleared and the folder resyncs under the same Uids (item 5.7.3's
    /// stable validity).
    folder_cursors: HashMap<MailboxId, String>,
    current: Option<MailboxId>,
    /// Lookout category key → Graph category display name, learned from Graph
    /// deltas. Graph categories are names, the command contract carries keys;
    /// a key Graph hasn't shown yet falls back to the key itself.
    category_names: HashMap<String, String>,
    /// Where this session's copies landed, for `RevertCopy`: the originals'
    /// `(from, to, uid)` → the copies' synthetic Uids in `to`.
    copy_ledger: HashMap<(MailboxId, MailboxId, Uid), Vec<Uid>>,
    next_outbox_due: Option<DateTime<Utc>>,
    next_snooze_wake: Option<DateTime<Utc>>,
    /// Rotates which non-open folder the next poll syncs.
    poll_sweep: usize,
}

impl GraphMailSession {
    // --- Startup -----------------------------------------------------------------

    /// Replays the cache before the first network call, so a launch paints the
    /// folder tree, the open folder's mail, the outbox and the flagged list
    /// immediately - exactly as the IMAP session does.
    async fn replay_cache(&mut self) {
        if let Some(Ok(folders)) = cache_op(&self.cache, {
            let account_id = self.account_id.clone();
            move |cache| cache.load_mailboxes(&account_id)
        })
        .await
        {
            if !folders.is_empty() {
                self.folders = folders;
                let _ = self.events.send(AccountEvent::FoldersUpdated(self.folders.clone())).await;
            }
        }
        // Resume each folder from its persisted delta cursor (item 5.7.8): a
        // restart then delivers only what changed instead of re-reading every
        // folder in full. An expired cursor is handled in `sync_mailbox`.
        if let Some(Ok(cursors)) = cache_op(&self.cache, |cache| cache.graph_folder_cursors()).await {
            self.folder_cursors = cursors;
        }
        self.current = self
            .folders
            .iter()
            .find(|folder| folder.role == MailboxRole::Inbox)
            .or_else(|| self.folders.first())
            .map(|folder| folder.id.clone());
        if let Some(current) = self.current.clone() {
            self.emit_mailbox(&current, false).await;
        }
        self.emit_flagged().await;
        self.emit_outbox().await;
        self.next_outbox_due = cache_op(&self.cache, |cache| cache.next_outbox_not_before()).await.and_then(|r| r.ok()).flatten();
        self.next_snooze_wake = cache_op(&self.cache, |cache| cache.next_snooze_wake()).await.and_then(|r| r.ok()).flatten();
    }

    /// The first folder + message sync. A failure leaves the session running
    /// with whatever the cache showed and retries on the poll cadence.
    async fn initial_sync(&mut self) {
        if let Err(e) = self.sync_folders(true).await {
            tracing::warn!("graph mail: the first folder sync failed, will retry: {e}");
            let _ = self
                .events
                .send(AccountEvent::ConnectionStateChanged(ConnectionState::Error {
                    message: e.to_string(),
                    retryable: true,
                }))
                .await;
            return;
        }
        self.next_poll = Utc::now() + self.poll_interval;
        if let Some(inbox) = self.folders.iter().find(|folder| folder.role == MailboxRole::Inbox).map(|folder| folder.id.clone()) {
            self.current = Some(inbox.clone());
            self.sync_mailbox(&inbox, true).await;
        }
        let _ = self.events.send(AccountEvent::ConnectionStateChanged(ConnectionState::Idle)).await;
    }

    /// Graph exposes no ManageSieve; the rules UI must hide the Sieve backend
    /// (R6, reconciled in 5.7.7).
    async fn emit_sieve_advertised(&self) {
        let _ = self.events.send(AccountEvent::SieveAdvertised(false)).await;
    }

    // --- Folder sync -------------------------------------------------------------

    /// Reads the folder delta and merges it into the folder list. `initial`
    /// also resolves the well-known names, which is what tells a localised
    /// folder ("Posteingang") from a custom one.
    async fn sync_folders(&mut self, initial: bool) -> Result<(), GraphError> {
        let mut delta = match self.client.own_mail_folders_delta(self.folder_delta_cursor.as_deref()).await {
            Ok(delta) => delta,
            Err(e) if e.is_delta_reset() => {
                tracing::info!("graph mail: the folder delta cursor expired, resyncing the hierarchy");
                self.folder_delta_cursor = None;
                self.client.own_mail_folders_delta(None).await?
            }
            Err(e) => return Err(e),
        };
        if initial || self.well_known == WellKnownFolders::default() {
            // A mailbox without an Archive folder answers 404 for it, which
            // the client already turns into `None`; a total failure past that
            // only costs the role guess, not the sync.
            match self.client.well_known_folders().await {
                Ok(well_known) => self.well_known = well_known,
                Err(e) => tracing::warn!("graph mail: couldn't resolve the well-known folders, falling back to names: {e}"),
            }
        }

        let validity = cache_op(&self.cache, |cache| cache.graph_uid_validity())
            .await
            .and_then(|r| r.ok())
            .unwrap_or(UidValidity(0));
        let incoming = std::mem::take(&mut delta.folders);
        let changed = initial || !incoming.is_empty() || !delta.removals.is_empty();
        // Add/update first, so a parent that arrived in the same page is
        // already present when the second pass links it.
        for folder in &incoming {
            let id = graph_mailbox_id(&self.account_id, &folder.id);
            let mailbox = mailbox_from_graph(folder, &self.account_id, id.clone(), None, self.well_known.role_of(folder), validity);
            match self.folders.iter_mut().find(|known| known.id == id) {
                Some(existing) => *existing = mailbox,
                None => self.folders.push(mailbox),
            }
        }
        // Graph has no delimiter and reports the mailbox root as a top-level
        // folder's parent; only a parent that is a real folder row is kept.
        for folder in &incoming {
            let id = graph_mailbox_id(&self.account_id, &folder.id);
            let parent = folder.parent_id.as_deref().map(|parent| graph_mailbox_id(&self.account_id, parent));
            let parent = parent.filter(|parent| self.folders.iter().any(|known| known.id == *parent));
            if let Some(child) = self.folders.iter_mut().find(|known| known.id == id) {
                child.parent = parent;
            }
        }
        for removed in &delta.removals {
            let id = graph_mailbox_id(&self.account_id, removed);
            if let Some(index) = self.folders.iter().position(|folder| folder.id == id) {
                self.folders.remove(index);
                self.forget_mailbox(&id).await;
            }
        }
        if let Some(cursor) = delta.delta_link {
            self.folder_delta_cursor = Some(cursor);
        }

        // A parent that was deleted in this same page must not linger on a
        // child row.
        let known: HashSet<MailboxId> = self.folders.iter().map(|folder| folder.id.clone()).collect();
        for folder in &mut self.folders {
            folder.parent = folder.parent.clone().filter(|parent| known.contains(parent));
        }

        if changed {
            self.publish_folders().await;
        }
        // A newly-created folder becomes the open one only if nothing else is;
        // otherwise leave the user's selection alone.
        if self.current.is_none() {
            self.current = self.folders.first().map(|folder| folder.id.clone());
        }
        Ok(())
    }

    /// Drops a deleted folder's cached messages. Graph folder ids aren't paths,
    /// so this forgets the folder by its own id - never `forget_mailbox_tree`,
    /// whose subtree walk is delimiter-based (item 5.7.4 rule 4).
    async fn forget_mailbox(&mut self, id: &MailboxId) {
        self.folder_cursors.remove(id);
        if let Some(Err(e)) = cache_op(&self.cache, {
            let id = id.clone();
            move |cache| cache.clear_graph_folder_cursor(&id)
        })
        .await
        {
            tracing::warn!(%id, "couldn't clear a deleted folder's Graph delta cursor: {e}");
        }
        let cached = cache_op(&self.cache, {
            let id = id.clone();
            move |cache| cache.load_messages(&id)
        })
        .await
        .and_then(|r| r.ok())
        .unwrap_or_default();
        let uids: Vec<Uid> = cached.iter().map(|message| message.uid).collect();
        if !uids.is_empty() {
            if let Some(Err(e)) = cache_op(&self.cache, {
                let id = id.clone();
                move |cache| cache.delete_messages(&id, &uids)
            })
            .await
            {
                tracing::warn!("failed to drop a deleted folder's cached messages: {e}");
            }
        }
    }

    async fn publish_folders(&mut self) {
        if let Some(Err(e)) = cache_op(&self.cache, {
            let account_id = self.account_id.clone();
            let folders = self.folders.clone();
            move |cache| cache.replace_mailboxes(&account_id, &folders)
        })
        .await
        {
            tracing::warn!("failed to cache the folder list: {e}");
        }
        let _ = self.events.send(AccountEvent::FoldersUpdated(self.folders.clone())).await;
    }

    fn folder_id(&self, role: MailboxRole) -> Option<MailboxId> {
        self.folders.iter().find(|folder| folder.role == role).map(|folder| folder.id.clone())
    }

    fn role_of(&self, id: &MailboxId) -> MailboxRole {
        self.folders.iter().find(|folder| folder.id == *id).map(|folder| folder.role).unwrap_or(MailboxRole::Custom)
    }

    // --- Message sync ------------------------------------------------------------

    /// Syncs one folder's messages delta into the cache and repaints it. A
    /// failed sync is logged and returns; the next poll retries.
    async fn sync_mailbox(&mut self, mailbox: &MailboxId, announce: bool) -> bool {
        if announce {
            let _ = self.events.send(AccountEvent::MailboxSyncStarted { mailbox: mailbox.clone() }).await;
        }
        let Some(folder_id) = graph_folder_id(&self.account_id, mailbox) else {
            tracing::warn!(%mailbox, "not a folder of this account");
            return false;
        };
        let folder_id = folder_id.to_string();

        // The pre-sync set decides new-arrival events. `None` means this folder
        // has no cache baseline yet: connecting must not replay its history
        // through notifications or rules.
        let before = cache_op(&self.cache, {
            let mailbox = mailbox.clone();
            move |cache| cache.load_messages(&mailbox)
        })
        .await
        .and_then(|r| r.ok());
        let baseline: Option<HashSet<Uid>> = before
            .as_ref()
            .filter(|messages| !messages.is_empty())
            .map(|messages| messages.iter().map(|m| m.uid).collect());

        let mut cursor = self.folder_cursors.get(mailbox).cloned();
        // Captured before the read, so a local property write made while the
        // delta is in flight keeps its `updated_at` guard (item 5.7.4 rule 2).
        let taken_at = Utc::now();
        let mut delta = loop {
            match self.client.folder_messages_delta(&folder_id, cursor.as_deref(), self.zone).await {
                Ok(delta) => break delta,
                Err(e) if e.is_delta_reset() && cursor.is_some() => {
                    tracing::info!(%mailbox, "graph mail: an expired message delta cursor, resyncing the folder");
                    self.folder_cursors.remove(mailbox);
                    // Clear the persisted cursor too, so a restart while the
                    // snapshot below is failing doesn't retry the dead link.
                    if let Some(Err(clear_error)) = cache_op(&self.cache, {
                        let mailbox = mailbox.clone();
                        move |cache| cache.clear_graph_folder_cursor(&mailbox)
                    })
                    .await
                    {
                        tracing::warn!(%mailbox, "couldn't clear the expired Graph delta cursor: {clear_error}");
                    }
                    cursor = None;
                }
                Err(e) => {
                    tracing::warn!(%mailbox, "graph mail: folder sync failed: {e}");
                    let _ = self
                        .events
                        .send(AccountEvent::ConnectionStateChanged(ConnectionState::Error {
                            message: e.to_string(),
                            retryable: true,
                        }))
                        .await;
                    return false;
                }
            }
        };
        let next_cursor = delta.delta_link.take();

        // A read that started from no cursor is the folder's whole current
        // set (a first sync, or a 410 reset), so stale cached rows must go
        // too; an incremental page only reports changes.
        let full = cursor.is_none();
        let changed = full || !delta.upserts.is_empty() || !delta.removals.is_empty();
        let stored = self.store_delta(mailbox, delta, taken_at, full).await;
        // Advance the cursor only after the page it describes has actually
        // been stored (item 5.7.8): persisting it before a failed cache write
        // would skip those changes on the next poll.
        if stored.is_some() {
            if let Some(link) = next_cursor {
                self.folder_cursors.insert(mailbox.clone(), link.clone());
                if let Some(Err(e)) = cache_op(&self.cache, {
                    let mailbox = mailbox.clone();
                    move |cache| cache.set_graph_folder_cursor(&mailbox, &link)
                })
                .await
                {
                    tracing::warn!(%mailbox, "couldn't persist the Graph delta cursor: {e}");
                }
            }
        }
        if announce || (self.current.as_ref() == Some(mailbox) && changed) {
            self.emit_mailbox(mailbox, true).await;
        }

        if let Some(before) = baseline {
            if let Some(new) = stored.map(|stored| stored.into_iter().filter(|message| !before.contains(&message.uid)).collect::<Vec<EmailSummary>>()) {
                if !new.is_empty() {
                    let unread: Vec<EmailSummary> = new.iter().filter(|message| message.is_unread()).cloned().collect();
                    if !unread.is_empty() {
                        let _ = self
                            .events
                            .send(AccountEvent::NewMessages {
                                mailbox: mailbox.clone(),
                                messages: unread,
                            })
                            .await;
                    }
                    let _ = self
                        .events
                        .send(AccountEvent::NewMailArrived {
                            mailbox: mailbox.clone(),
                            messages: new,
                        })
                        .await;
                }
            }
        }
        true
    }

    /// Writes one folder delta into the cache and returns the upserted rows.
    /// New Uids go oldest-first so Uid order follows arrival, as on IMAP.
    async fn store_delta(&mut self, mailbox: &MailboxId, mut delta: MessageSummaryDelta, taken_at: DateTime<Utc>, full: bool) -> Option<Vec<EmailSummary>> {
        // A folder delta is scoped to one folder; a record naming a different
        // parent is a moved/stale item the destination's own delta reports.
        // (`parentFolderId` is absent from some responses, which is kept.)
        let folder = graph_folder_id(&self.account_id, mailbox).map(str::to_string);
        delta.upserts.retain(|message| message.folder_id.is_none() || message.folder_id == folder);
        for message in &delta.upserts {
            for name in &message.categories {
                self.category_names.insert(lookout_core::sanitize_tag_key(name), name.clone());
            }
        }
        allocation_order(&mut delta.upserts);
        let graph_ids: Vec<String> = delta.upserts.iter().map(|message| message.graph_id.clone()).collect();
        if graph_ids.is_empty() && delta.removals.is_empty() && !full {
            return Some(Vec::new());
        }
        let cache = self.cache.clone();
        let mailbox_owned = mailbox.clone();
        let removals = delta.removals.clone();
        let messages = delta.upserts.clone();
        let stored = cache_op(&cache, move |cache| -> lookout_mail::Result<Vec<EmailSummary>> {
            let validity = cache.graph_uid_validity()?;
            let ids: Vec<&str> = graph_ids.iter().map(String::as_str).collect();
            let uids = cache.graph_uids(&ids)?;
            let matcher = cache.mention_matcher();
            let summaries: Vec<EmailSummary> = messages
                .iter()
                .zip(uids.iter())
                .map(|(message, uid)| {
                    let mut summary = email_summary_from_graph(message, mailbox_owned.clone(), *uid);
                    // The delta carries only `bodyPreview`; that is enough for
                    // the list's `@mention` badge (the IMAP session derives it
                    // from a preview too), without a per-message body read.
                    if let Some(preview) = message.preview.as_deref() {
                        summary.mentions = matcher.matches(preview);
                    }
                    summary
                })
                .collect();
            if full {
                // The whole current set: a stale row from before a cache
                // reset (or a message deleted while offline) has no tombstone
                // in a from-scratch delta, so diff against the snapshot.
                cache.replace_messages(&mailbox_owned, validity, &summaries)?;
            } else {
                cache.upsert_messages(&mailbox_owned, validity, &summaries)?;
            }
            // Categories, importance, Focused/Other and the flag ride the
            // existing 5.4 overlays, with `delta_cursor = None` so the 5.4
            // property sync keeps its own cursor (item 5.7.4 rule 2).
            cache.apply_remote_mail_properties(&remote_properties(&messages), &[], None, taken_at)?;
            // Tombstones are Graph ids; look up only ids this cache knows, so
            // a message deleted before it was ever synced doesn't burn a Uid.
            let removed = cache.graph_uids_existing(&removals)?;
            let removed_uids: Vec<Uid> = removed.into_values().collect();
            cache.delete_messages(&mailbox_owned, &removed_uids)?;
            Ok(summaries)
        })
        .await;
        match stored {
            Some(Ok(summaries)) => Some(summaries),
            Some(Err(e)) => {
                tracing::warn!(%mailbox, "failed to store the Graph delta: {e}");
                None
            }
            None => None,
        }
    }

    /// Emits the folder's cached rows (minus snoozed) as `MessagesUpdated`.
    /// `allow_empty` publishes an empty set, which a sync must do so a folder
    /// emptied elsewhere clears the list.
    async fn emit_mailbox(&mut self, mailbox: &MailboxId, allow_empty: bool) {
        let loaded = cache_op(&self.cache, {
            let mailbox = mailbox.clone();
            move |cache| -> lookout_mail::Result<(Vec<EmailSummary>, HashSet<Uid>)> {
                let messages = cache.load_messages(&mailbox)?;
                if let Err(e) = cache.record_addresses(&messages) {
                    tracing::warn!("failed to record addresses: {e}");
                }
                let now = Utc::now();
                if let Err(e) = cache.purge_expired_snoozed(now) {
                    tracing::warn!("failed to purge expired snoozes: {e}");
                }
                let snoozed = cache.active_snoozed_uids(&mailbox, now)?;
                Ok((messages, snoozed))
            }
        })
        .await
        .and_then(|result| result.ok());
        let Some((messages, snoozed)) = loaded else { return };
        let filtered: Vec<EmailSummary> = messages.into_iter().filter(|message| !snoozed.contains(&message.uid)).collect();
        if filtered.is_empty() && !allow_empty {
            return;
        }
        let _ = self
            .events
            .send(AccountEvent::MessagesUpdated {
                mailbox: mailbox.clone(),
                messages: filtered,
            })
            .await;
        // A sync can bring flags set by another client, so the Tasks view's
        // flagged-mail list is re-read too (the IMAP session does the same).
        self.emit_flagged().await;
    }

    async fn emit_flagged(&self) {
        if let Some(Ok(messages)) = cache_op(&self.cache, |cache| cache.flagged_messages()).await {
            let _ = self.events.send(AccountEvent::FlaggedMessages { messages }).await;
        }
    }

    async fn emit_outbox(&self) {
        if let Some(Ok(entries)) = cache_op(&self.cache, |cache| cache.all_outbox()).await {
            let _ = self.events.send(AccountEvent::OutboxContents { items: outbox_items(entries) }).await;
        }
    }

    // --- Polling -----------------------------------------------------------------

    fn next_sleep(&self) -> Duration {
        let now = Utc::now();
        let mut next = self.next_poll;
        if let Some(due) = self.next_outbox_due {
            next = next.min(due);
        }
        if let Some(wake) = self.next_snooze_wake {
            next = next.min(wake);
        }
        let millis = (next - now).num_milliseconds().max(250) as u64;
        Duration::from_millis(millis)
    }

    async fn on_deadline(&mut self) {
        if Utc::now() >= self.next_poll {
            self.poll().await;
            self.next_poll = Utc::now() + self.poll_interval;
        }
    }

    /// One poll: the open folder, one other folder in rotation, and every
    /// `FOLDER_REFRESH_POLLS` the hierarchy delta (for counts and new/deleted
    /// folders). Graph has no push; this is the whole delivery mechanism.
    async fn poll(&mut self) {
        self.polls_since_folder_refresh += 1;
        if self.polls_since_folder_refresh >= FOLDER_REFRESH_POLLS || self.folders.is_empty() {
            self.polls_since_folder_refresh = 0;
            if let Err(e) = self.sync_folders(false).await {
                tracing::warn!("graph mail: folder delta poll failed: {e}");
            }
        }
        if self.current.is_none() {
            self.current = self
                .folders
                .iter()
                .find(|folder| folder.role == MailboxRole::Inbox)
                .or_else(|| self.folders.first())
                .map(|folder| folder.id.clone());
        }
        // The open folder and the Inbox are always polled: the Inbox is where
        // new mail matters most, and with no push (R1) a rotation alone could
        // leave it minutes behind. Other folders take their turn below.
        let mut polled: Vec<MailboxId> = Vec::new();
        if let Some(current) = self.current.clone() {
            polled.push(current);
        }
        if let Some(inbox) = self.folder_id(MailboxRole::Inbox) {
            if !polled.contains(&inbox) {
                polled.push(inbox);
            }
        }
        for mailbox in &polled {
            self.sync_mailbox(mailbox, false).await;
        }
        self.poll_background_folder(&polled).await;
    }

    /// Syncs one folder other than those already polled this tick, in
    /// rotation. With N folders each gets a delta every N polls; the open
    /// folder and the Inbox are covered every poll.
    async fn poll_background_folder(&mut self, already: &[MailboxId]) {
        if self.folders.is_empty() {
            return;
        }
        let start = self.poll_sweep % self.folders.len();
        self.poll_sweep = self.poll_sweep.wrapping_add(1);
        for offset in 0..self.folders.len() {
            let folder = &self.folders[(start + offset) % self.folders.len()];
            if already.contains(&folder.id) {
                continue;
            }
            let id = folder.id.clone();
            self.sync_mailbox(&id, false).await;
            return;
        }
    }

    /// Delivers due outbox rows and wakes expired snoozes, mirroring the IMAP
    /// session's main-loop prelude.
    async fn service_due(&mut self) {
        let now = Utc::now();
        let snoozed = self.next_snooze_wake.is_some_and(|wake| wake <= now);
        if snoozed {
            let woken = cache_op(&self.cache, move |cache| cache.purge_expired_snoozed(now))
                .await
                .and_then(|r| r.ok())
                .unwrap_or_default();
            for mailbox in woken {
                self.emit_mailbox(&mailbox, false).await;
            }
            self.next_snooze_wake = cache_op(&self.cache, |cache| cache.next_snooze_wake()).await.and_then(|r| r.ok()).flatten();
        }
        if self.next_outbox_due.is_some_and(|due| due <= now) {
            self.drain_outbox().await;
        }
    }

    // --- The outbox --------------------------------------------------------------

    async fn drain_outbox(&mut self) {
        let due = cache_op(&self.cache, {
            let now = Utc::now();
            move |cache| cache.due_outbox(now)
        })
        .await
        .and_then(|r| r.ok())
        .unwrap_or_default();
        // One Sent resync for the whole batch rather than one per message.
        let mut sync_sent = false;
        for entry in due {
            let id = entry.id;
            let attempts = entry.attempts;
            let decoded = serde_json::from_slice::<OutboxMessage>(&entry.payload)
                .ok()
                .and_then(|message| message.raw().map(|raw| (message, raw)));
            let Some((message, raw)) = decoded else {
                tracing::warn!(id, "dropping an undecodable outbox row");
                cache_op(&self.cache, move |cache| cache.delete_outbox(id)).await;
                continue;
            };
            if message.request_delivery_receipt {
                // DSN is an SMTP envelope service (RFC 3461); Graph's send has
                // no equivalent, so the request is dropped rather than
                // silently pretended (M-27). The composer still offers the
                // switch for this transport, as the doc's C-07 row notes.
                tracing::warn!(id, "graph mail cannot request a delivery receipt; sending without one");
            }
            let raw = with_bcc_header(raw, &message.bcc);
            match self.client.send_mime(&raw).await {
                Ok(()) => {
                    // Delete the row as soon as the transport accepted the
                    // message, before the draft cleanup and Sent sync: a crash
                    // or quit in between must not resend it on the next start.
                    cache_op(&self.cache, move |cache| cache.delete_outbox(id)).await;
                    let _ = self.events.send(AccountEvent::OutboxSent { id }).await;
                    if let Some(draft_id) = message.draft_message_id.as_deref() {
                        if let Err(e) = self.delete_draft(draft_id).await {
                            tracing::warn!("message sent but deleting its draft failed: {e}");
                        }
                    }
                    // Exchange files the sent copy in Sent Items; sync it so
                    // the cache isn't a message behind - once after the loop.
                    sync_sent = true;
                }
                Err(e) => {
                    let retry_at = Utc::now() + outbox_retry_delay(attempts + 1);
                    let state = cache_op(&self.cache, move |cache| cache.outbox_attempt_failed(id, retry_at, MAX_OUTBOX_ATTEMPTS)).await;
                    match state {
                        Some(Ok(lookout_mail::OutboxState::Pending)) => tracing::warn!(id, "graph mail send failed, will retry: {e}"),
                        _ => {
                            tracing::warn!(id, "graph mail send failed permanently: {e}");
                            let _ = self
                                .events
                                .send(AccountEvent::OutboxFailed {
                                    id,
                                    client_token: None,
                                    message: format!("Couldn't send message: {e}"),
                                })
                                .await;
                        }
                    }
                }
            }
        }
        if sync_sent {
            if let Some(sent) = self.folder_id(MailboxRole::Sent) {
                self.sync_mailbox(&sent, false).await;
                if self.current.as_ref() == Some(&sent) {
                    self.emit_mailbox(&sent, false).await;
                }
            }
        }
        self.next_outbox_due = cache_op(&self.cache, |cache| cache.next_outbox_not_before()).await.and_then(|r| r.ok()).flatten();
        self.emit_outbox().await;
    }

    // --- Command handling --------------------------------------------------------

    /// Applies one command. Returns `false` for `Shutdown`.
    async fn handle_command(&mut self, command: AccountCommand) -> bool {
        match command {
            AccountCommand::Shutdown => return false,
            AccountCommand::SyncMailbox(mailbox) => {
                self.current = Some(mailbox.clone());
                self.emit_mailbox(&mailbox, false).await;
                let _ = self.events.send(AccountEvent::ConnectionStateChanged(ConnectionState::Busy)).await;
                self.sync_mailbox(&mailbox, true).await;
                let _ = self.events.send(AccountEvent::ConnectionStateChanged(ConnectionState::Idle)).await;
            }
            AccountCommand::Refresh => {
                let _ = self.events.send(AccountEvent::ConnectionStateChanged(ConnectionState::Busy)).await;
                if let Err(e) = self.sync_folders(false).await {
                    tracing::warn!("graph mail: refresh failed: {e}");
                }
                if let Some(current) = self.current.clone() {
                    self.sync_mailbox(&current, true).await;
                }
                let _ = self.events.send(AccountEvent::ConnectionStateChanged(ConnectionState::Idle)).await;
            }
            AccountCommand::FetchBody { mailbox, uid } => self.fetch_body(&mailbox, uid).await,
            AccountCommand::FetchAttachment { mailbox, uid, part } => self.fetch_attachment(&mailbox, uid, &part).await,
            AccountCommand::FetchRawMessage { mailbox, uid } => self.fetch_raw_message(&mailbox, uid).await,
            AccountCommand::StoreFlags { mailbox, uid, add, remove } => {
                if let Err(message) = self.store_flags(&mailbox, &[uid], &add, &remove).await {
                    let _ = self
                        .events
                        .send(AccountEvent::StoreFlagsFailed {
                            mailbox,
                            uids: vec![uid],
                            message: format!("Couldn't update message flags: {message}"),
                        })
                        .await;
                }
            }
            AccountCommand::StoreFlagsMany { mailbox, uids, add, remove } => {
                if uids.is_empty() {
                    return true;
                }
                if let Err(message) = self.store_flags(&mailbox, &uids, &add, &remove).await {
                    let _ = self
                        .events
                        .send(AccountEvent::StoreFlagsFailed {
                            mailbox,
                            uids,
                            message: format!("Couldn't update message flags: {message}"),
                        })
                        .await;
                }
            }
            AccountCommand::StoreKeywords { mailbox, uid, add, remove } => self.store_keywords(&mailbox, &[uid], &add, &remove).await,
            AccountCommand::StoreKeywordsMany { mailbox, uids, add, remove } => self.store_keywords(&mailbox, &uids, &add, &remove).await,
            AccountCommand::SetJunk { mailbox, uids, junk } => self.set_junk(&mailbox, &uids, junk).await,
            AccountCommand::SetFollowUp { mailbox, uids, flag } => self.set_follow_up(&mailbox, &uids, flag).await,
            AccountCommand::SetImportance { mailbox, uids, importance } => self.set_importance(&mailbox, &uids, importance).await,
            AccountCommand::SetFocused {
                mailbox,
                uids,
                classification,
                scope,
            } => self.set_focused(&mailbox, &uids, classification, scope).await,
            AccountCommand::MoveMessage { mailbox, uid, role } => self.move_to_role(&mailbox, &[uid], role).await,
            AccountCommand::MoveMessages { mailbox, uids, role } => self.move_to_role(&mailbox, &uids, role).await,
            AccountCommand::MoveMessagesTo { mailbox, uids, target } => self.move_to_mailbox(&mailbox, &uids, &target, MailboxRole::Custom).await,
            AccountCommand::RevertMove { moved_from, moved_to, uids } => self.move_to_mailbox(&moved_to, &uids, &moved_from, MailboxRole::Custom).await,
            AccountCommand::CopyMessagesTo { mailbox, uids, target } => self.copy_messages(&mailbox, &uids, &target).await,
            AccountCommand::RevertCopy { copied_from, copied_to, uids } => self.revert_copy(&copied_from, &copied_to, &uids).await,
            AccountCommand::EmptyMailbox { mailbox } => self.empty_mailbox(&mailbox).await,
            AccountCommand::MarkMailboxRead { mailbox } => self.mark_mailbox_read(&mailbox).await,
            AccountCommand::CreateMailbox { parent, name } => self.create_mailbox(parent.as_ref(), &name).await,
            AccountCommand::RenameMailbox { mailbox, new_name } => self.rename_mailbox(&mailbox, &new_name).await,
            AccountCommand::MoveMailbox { .. } => {
                let _ = self
                    .events
                    .send(AccountEvent::Error(
                        "Moving a folder isn't supported on Graph mail - recreate it under the new parent instead".to_string(),
                    ))
                    .await;
            }
            AccountCommand::DeleteMailbox { mailbox } => self.delete_mailbox(&mailbox).await,
            AccountCommand::SnoozeMessage { mailbox, uid, until } => self.snooze(&mailbox, &[uid], until).await,
            AccountCommand::SnoozeMessages { mailbox, uids, until } => self.snooze(&mailbox, &uids, until).await,
            AccountCommand::UnsnoozeMessages { mailbox, uids } => self.unsnooze(&mailbox, &uids).await,
            AccountCommand::WakeSnoozed { mailbox, uids } => self.unsnooze(&mailbox, &uids).await,
            AccountCommand::SendMessage {
                message,
                not_before,
                client_token,
            } => self.queue_send(*message, not_before, client_token).await,
            AccountCommand::CancelOutbox { id } => self.cancel_outbox(id).await,
            AccountCommand::RescheduleOutbox { id, not_before } => self.reschedule_outbox(id, not_before).await,
            AccountCommand::SaveDraft { msg, replace } => self.save_draft(*msg, replace).await,
            AccountCommand::DeleteDraft { message_id } => {
                if let Err(e) = self.delete_draft(&message_id).await {
                    tracing::warn!("couldn't delete the draft: {e}");
                }
            }
            AccountCommand::SearchMailbox { mailbox, query } => self.search_mailbox(&mailbox, &query).await,
            AccountCommand::ApplyRemoteMailProperties {
                properties,
                removed_graph_ids,
                delta_cursor,
                taken_at,
            } => {
                let applied = matches!(
                    cache_op(&self.cache, move |cache| cache.apply_remote_mail_properties(
                        &properties,
                        &removed_graph_ids,
                        delta_cursor.as_deref(),
                        taken_at
                    ))
                    .await,
                    Some(Ok(()))
                );
                if applied {
                    if let Some(current) = self.current.clone() {
                        self.emit_mailbox(&current, false).await;
                    }
                    self.emit_flagged().await;
                }
            }
            AccountCommand::ApplyInferenceOverrides { overrides, taken_at } => {
                if let Some(Err(e)) = cache_op(&self.cache, move |cache| cache.apply_inference_overrides(&overrides, taken_at)).await {
                    tracing::warn!("failed to apply Focused/Other overrides: {e}");
                }
            }
            AccountCommand::ClearInferenceOverride { sender } => {
                if let Some(Err(e)) = cache_op(&self.cache, move |cache| cache.clear_inference_override(&sender)).await {
                    tracing::warn!("failed to clear a Focused/Other override: {e}");
                }
            }
            AccountCommand::ConfigureLocalFocus { .. } => {
                // Graph classifications come from Outlook itself; the local
                // classifier (item 2.12) is only for non-Microsoft accounts.
            }
            AccountCommand::SetPrefetchPolicy(_) | AccountCommand::Reconnect | AccountCommand::PrefetchBodies => {}
            AccountCommand::PutSieveScript { .. } => {
                let _ = self
                    .events
                    .send(AccountEvent::SieveFailed {
                        message: "Sieve rules aren't available on Graph mail".to_string(),
                    })
                    .await;
            }
            AccountCommand::FetchSieveCapabilities => {
                let _ = self
                    .events
                    .send(AccountEvent::SieveCapabilities {
                        reachable: false,
                        vacation: false,
                        date: false,
                        relational: false,
                    })
                    .await;
            }
        }
        true
    }

    // --- Reads -------------------------------------------------------------------

    async fn fetch_body(&mut self, mailbox: &MailboxId, uid: Uid) {
        let validity = self.validity().await;
        if let Some(body) = cache_op(&self.cache, {
            let mailbox = mailbox.clone();
            move |cache| cache.load_body(&mailbox, uid, validity)
        })
        .await
        .and_then(|result| result.ok())
        .flatten()
        {
            let _ = self
                .events
                .send(AccountEvent::BodyFetched {
                    mailbox: mailbox.clone(),
                    uid,
                    body,
                })
                .await;
            return;
        }
        let Some(graph_id) = self.graph_id(uid).await else {
            let _ = self
                .events
                .send(AccountEvent::Error(format!("Couldn't load the message: no Graph id for UID {}", uid.0)))
                .await;
            return;
        };
        let Some((message, raw)) = self.client.get_message(&graph_id, self.zone).await.ok().flatten() else {
            let _ = self.events.send(AccountEvent::Error("Couldn't load the message: it no longer exists".to_string())).await;
            return;
        };
        let attachments = self.client.list_attachments(&graph_id).await.unwrap_or_default();

        // Prefer the MIME read: it is the only form that carries the exact
        // text/html parts, iMIP `text/calendar` and the headers (R3), and
        // item 5.7.1's spike exists to prove its fidelity. A mailbox where
        // `/$value` isn't available falls back to Graph's JSON body.
        let mut body = match self.client.get_mime(&graph_id).await.ok().flatten().and_then(|bytes| parse_body(uid, &bytes)) {
            Some(mut mime) => {
                // Part numbers stay Graph attachment ids so `FetchAttachment`
                // can resolve them (item 5.7.3); the MIME structure's own
                // section paths would address an IMAP mailbox instead.
                mime.parts = attachments.iter().map(body_part_from_attachment).collect();
                mime
            }
            None => email_body_from_graph(&raw, uid, &attachments),
        };
        body.uid = uid;
        if let Some(Err(e)) = cache_op(&self.cache, {
            let mailbox = mailbox.clone();
            let body = body.clone();
            move |cache| cache.store_body(&mailbox, uid, validity, &body)
        })
        .await
        {
            tracing::warn!("failed to cache a Graph body: {e}");
        }
        let _ = message;
        let _ = self
            .events
            .send(AccountEvent::BodyFetched {
                mailbox: mailbox.clone(),
                uid,
                body,
            })
            .await;
    }

    async fn fetch_attachment(&mut self, mailbox: &MailboxId, uid: Uid, part: &BodyPart) {
        let validity = self.validity().await;
        if let Some(bytes) = cache_op(&self.cache, {
            let mailbox = mailbox.clone();
            let part_number = part.part_number.clone();
            move |cache| cache.load_attachment(&mailbox, uid, validity, &part_number)
        })
        .await
        .and_then(|result| result.ok())
        .flatten()
        {
            let _ = self
                .events
                .send(AccountEvent::PartFetched {
                    mailbox: mailbox.clone(),
                    uid,
                    part: part.clone(),
                    bytes,
                })
                .await;
            return;
        }
        let failure = |message: String| AccountEvent::PartFetchFailed {
            mailbox: mailbox.clone(),
            uid,
            part_number: part.part_number.clone(),
            message,
        };
        let Some(attachment_id) = attachment_id_from_part(&part.part_number) else {
            let _ = self.events.send(failure("the attachment id is malformed".to_string())).await;
            return;
        };
        let Some(graph_id) = self.graph_id(uid).await else {
            let _ = self.events.send(failure("the message is no longer synced".to_string())).await;
            return;
        };
        match self.client.get_attachment_bytes(&graph_id, &attachment_id).await {
            Ok(Some(bytes)) => {
                if let Some(Err(e)) = cache_op(&self.cache, {
                    let mailbox = mailbox.clone();
                    let part_number = part.part_number.clone();
                    let bytes = bytes.clone();
                    move |cache| cache.store_attachment(&mailbox, uid, validity, &part_number, &bytes)
                })
                .await
                {
                    tracing::warn!("failed to cache a Graph attachment: {e}");
                }
                let _ = self
                    .events
                    .send(AccountEvent::PartFetched {
                        mailbox: mailbox.clone(),
                        uid,
                        part: part.clone(),
                        bytes,
                    })
                    .await;
            }
            Ok(None) => {
                let _ = self.events.send(failure("the attachment no longer exists".to_string())).await;
            }
            Err(e) => {
                let _ = self.events.send(failure(e.to_string())).await;
            }
        }
    }

    async fn fetch_raw_message(&mut self, mailbox: &MailboxId, uid: Uid) {
        let validity = self.validity().await;
        if let Some(bytes) = cache_op(&self.cache, {
            let mailbox = mailbox.clone();
            move |cache| cache.load_raw_message(&mailbox, uid, validity)
        })
        .await
        .and_then(|result| result.ok())
        .flatten()
        {
            let _ = self
                .events
                .send(AccountEvent::RawMessageFetched {
                    mailbox: mailbox.clone(),
                    uid,
                    bytes,
                })
                .await;
            return;
        }
        let Some(graph_id) = self.graph_id(uid).await else {
            let _ = self
                .events
                .send(AccountEvent::RawMessageFetchFailed {
                    mailbox: mailbox.clone(),
                    uid,
                    message: "the message is no longer synced".to_string(),
                })
                .await;
            return;
        };
        match self.client.get_mime(&graph_id).await {
            Ok(Some(bytes)) => {
                if let Some(Err(e)) = cache_op(&self.cache, {
                    let mailbox = mailbox.clone();
                    let bytes = bytes.clone();
                    move |cache| cache.store_raw_message(&mailbox, uid, validity, &bytes)
                })
                .await
                {
                    tracing::warn!("failed to cache a raw Graph message: {e}");
                }
                let _ = self
                    .events
                    .send(AccountEvent::RawMessageFetched {
                        mailbox: mailbox.clone(),
                        uid,
                        bytes,
                    })
                    .await;
            }
            Ok(None) => {
                let _ = self
                    .events
                    .send(AccountEvent::RawMessageFetchFailed {
                        mailbox: mailbox.clone(),
                        uid,
                        message: "the message no longer exists".to_string(),
                    })
                    .await;
            }
            Err(e) => {
                let _ = self
                    .events
                    .send(AccountEvent::RawMessageFetchFailed {
                        mailbox: mailbox.clone(),
                        uid,
                        message: e.to_string(),
                    })
                    .await;
            }
        }
    }

    // --- Native property writes --------------------------------------------------

    /// `\Seen` is Graph's `isRead`; the other system flags (Pin, Answered,
    /// Draft, Deleted) have no Graph property, so they're left to the cache or
    /// the folder operation the caller sends alongside. Returns a message on
    /// failure.
    async fn store_flags(&mut self, mailbox: &MailboxId, uids: &[Uid], add: &[SystemFlagBit], remove: &[SystemFlagBit]) -> Result<(), String> {
        let Some(is_read) = read_state(add, remove) else {
            return Ok(());
        };
        let ids = self.graph_ids(uids).await;
        for uid in uids {
            let Some(graph_id) = ids.get(uid) else { continue };
            let patch = MessagePatch {
                is_read: Some(is_read),
                ..Default::default()
            };
            if let Err(e) = self.client.update_message(graph_id, &patch, self.zone).await {
                tracing::warn!(%mailbox, "graph mail: couldn't set the read state: {e}");
                return Err(e.to_string());
            }
        }
        // The server is authoritative now; patch the cache so the list repaints
        // without a re-fetch.
        if let Some(Err(e)) = cache_op(&self.cache, {
            let mailbox = mailbox.clone();
            let uids = uids.to_vec();
            let add = add.to_vec();
            let remove = remove.to_vec();
            move |cache| cache.update_flags_many(&mailbox, &uids, &add, &remove)
        })
        .await
        {
            tracing::warn!("failed to cache the read state: {e}");
        }
        self.emit_mailbox(mailbox, false).await;
        self.adjust_unread(mailbox, if is_read { -(uids.len() as i64) } else { uids.len() as i64 }).await;
        Ok(())
    }

    async fn set_follow_up(&mut self, mailbox: &MailboxId, uids: &[Uid], flag: lookout_core::FollowUpFlag) {
        if uids.is_empty() {
            return;
        }
        let summaries = self.summaries(mailbox, uids).await;
        let ids = self.graph_ids(uids).await;
        for uid in uids {
            let Some(graph_id) = ids.get(uid) else { continue };
            let patch = MessagePatch {
                flag: Some(flag.clone()),
                ..Default::default()
            };
            if let Err(e) = self.client.update_message(graph_id, &patch, self.zone).await {
                tracing::warn!(%mailbox, "graph mail: couldn't set the follow-up flag: {e}");
                let _ = self
                    .events
                    .send(AccountEvent::FollowUpFailed {
                        mailbox: mailbox.clone(),
                        uids: uids.to_vec(),
                        message: format!("Couldn't flag the message: {e}"),
                    })
                    .await;
                return;
            }
        }
        // Graph is authoritative, but the cache's overlay keeps the list
        // correct until the next delta reports the change.
        if let Some(Err(e)) = cache_op(&self.cache, {
            let summaries = summaries.clone();
            let flag = flag.clone();
            move |cache| cache.set_local_follow_ups(&summaries, &flag, Utc::now())
        })
        .await
        {
            tracing::warn!("failed to cache the follow-up flag: {e}");
        }
        let message_ids: Vec<String> = summaries.iter().filter_map(|summary| summary.message_id.clone()).collect();
        let _ = self.events.send(AccountEvent::FollowUpSet { message_ids, flag }).await;
        self.emit_mailbox(mailbox, false).await;
        self.emit_flagged().await;
    }

    async fn set_importance(&mut self, mailbox: &MailboxId, uids: &[Uid], importance: Importance) {
        if uids.is_empty() {
            return;
        }
        let summaries = self.summaries(mailbox, uids).await;
        let ids = self.graph_ids(uids).await;
        for uid in uids {
            let Some(graph_id) = ids.get(uid) else { continue };
            let patch = MessagePatch {
                importance: Some(importance),
                ..Default::default()
            };
            if let Err(e) = self.client.update_message(graph_id, &patch, self.zone).await {
                let _ = self.events.send(AccountEvent::Error(format!("Couldn't set the message importance: {e}"))).await;
                return;
            }
        }
        if let Some(Err(e)) = cache_op(&self.cache, {
            let summaries = summaries.clone();
            move |cache| cache.set_local_importance(&summaries, importance, Utc::now())
        })
        .await
        {
            tracing::warn!("failed to cache the importance: {e}");
        }
        let message_ids: Vec<String> = summaries.iter().filter_map(|summary| summary.message_id.clone()).collect();
        let _ = self.events.send(AccountEvent::ImportanceSet { message_ids, importance }).await;
        self.emit_mailbox(mailbox, false).await;
    }

    async fn set_focused(&mut self, mailbox: &MailboxId, uids: &[Uid], classification: InferenceClassification, scope: FocusedScope) {
        if uids.is_empty() {
            return;
        }
        let summaries = self.summaries(mailbox, uids).await;
        let ids = self.graph_ids(uids).await;
        for uid in uids {
            let Some(graph_id) = ids.get(uid) else { continue };
            let patch = MessagePatch {
                classification: Some(classification),
                ..Default::default()
            };
            if let Err(e) = self.client.update_message(graph_id, &patch, self.zone).await {
                let _ = self
                    .events
                    .send(AccountEvent::Error(format!("Couldn't change the Focused/Other classification: {e}")))
                    .await;
                return;
            }
        }
        if let Some(Err(e)) = cache_op(&self.cache, {
            let summaries = summaries.clone();
            move |cache| cache.set_local_inference(&summaries, classification, Utc::now())
        })
        .await
        {
            tracing::warn!("failed to cache the Focused/Other classification: {e}");
        }
        // "Always move…" records the per-sender override natively.
        let mut sender: Option<EmailAddress> = None;
        if scope == FocusedScope::Sender {
            if let Some(from) = summaries.iter().find_map(|summary| summary.from.first()).cloned() {
                match self.client.create_override(classification, &from).await {
                    Ok(created) => {
                        let override_id = created.as_ref().map(|entry| entry.id.as_str());
                        if let Some(Err(e)) = cache_op(&self.cache, {
                            let address = from.address.clone();
                            let override_id = override_id.map(str::to_string);
                            move |cache| cache.set_inference_override(&address, classification, override_id.as_deref(), Utc::now())
                        })
                        .await
                        {
                            tracing::warn!("failed to cache a Focused/Other override: {e}");
                        }
                        sender = Some(from);
                    }
                    Err(e) => tracing::warn!("graph mail: couldn't create a Focused/Other override: {e}"),
                }
            }
        }
        let message_ids: Vec<String> = summaries.iter().filter_map(|summary| summary.message_id.clone()).collect();
        let _ = self
            .events
            .send(AccountEvent::FocusedSet {
                message_ids,
                classification,
                sender,
            })
            .await;
        self.emit_mailbox(mailbox, false).await;
    }

    /// Tags become Graph categories. The command carries category *keys*; the
    /// display names come from what Graph has already shown this session (a
    /// brand-new local key falls back to itself until the app's master-list
    /// push lands it).
    async fn store_keywords(&mut self, mailbox: &MailboxId, uids: &[Uid], add: &[String], remove: &[String]) {
        if uids.is_empty() {
            return;
        }
        let add_keys = tag_keys(add);
        let remove_keys = tag_keys(remove);
        if add_keys.is_empty() && remove_keys.is_empty() {
            return;
        }
        let summaries = self.summaries(mailbox, uids).await;
        let ids = self.graph_ids(uids).await;
        let mut updates = Vec::new();
        for summary in &summaries {
            let Some(graph_id) = ids.get(&summary.uid) else { continue };
            let mut keys: std::collections::BTreeSet<&str> = summary.keywords.iter().filter_map(|keyword| lookout_core::tag_key_from_keyword(keyword)).collect();
            for key in &add_keys {
                keys.insert(key.as_str());
            }
            for key in &remove_keys {
                keys.remove(key.as_str());
            }
            let names: Vec<String> = keys.iter().map(|key| self.category_name(key)).collect();
            if let Err(e) = self.client.set_categories(graph_id, &names).await {
                let _ = self.events.send(AccountEvent::Error(format!("Couldn't update message tags: {e}"))).await;
                return;
            }
            if let Some(message_id) = summary.message_id.clone() {
                updates.push((message_id, keys.into_iter().map(str::to_string).collect()));
            }
        }
        // Keep the overlay in step with the native write; the next delta
        // reconciles it again with whatever Graph reports.
        if let Some(Err(e)) = cache_op(&self.cache, {
            let summaries = summaries.clone();
            let add = add_keys.clone();
            let remove = remove_keys.clone();
            move |cache| cache.update_local_categories(&summaries, &add, &remove, Utc::now())
        })
        .await
        {
            tracing::warn!("failed to update cached categories: {e}");
        }
        if !updates.is_empty() {
            let _ = self.events.send(AccountEvent::CategoriesChanged { updates }).await;
        }
        self.emit_mailbox(mailbox, false).await;
    }

    /// Graph has no `$Junk` keyword. The caller moves the message to Junk; the
    /// keyword is a local hint only (the 5.7.7 reconciliation marks this
    /// degraded).
    async fn set_junk(&mut self, mailbox: &MailboxId, uids: &[Uid], junk: bool) {
        if uids.is_empty() {
            return;
        }
        let (add, remove) = lookout_core::junk_keyword_change(junk);
        if let Some(Err(e)) = cache_op(&self.cache, {
            let mailbox = mailbox.clone();
            let uids = uids.to_vec();
            let add = vec![add.to_string()];
            let remove = vec![remove.to_string()];
            move |cache| cache.update_keywords_many(&mailbox, &uids, &add, &remove)
        })
        .await
        {
            tracing::warn!("failed to cache the junk state: {e}");
        }
        self.emit_mailbox(mailbox, false).await;
    }

    // --- Moves, copies, snooze ---------------------------------------------------

    async fn move_to_role(&mut self, mailbox: &MailboxId, uids: &[Uid], role: MailboxRole) {
        let Some(target) = self.folder_id(role) else {
            let _ = self
                .events
                .send(AccountEvent::MoveFailed {
                    mailbox: mailbox.clone(),
                    uids: uids.to_vec(),
                    role,
                    message: format!("There's no {} folder on this account", role_label(role)),
                })
                .await;
            return;
        };
        self.move_to_mailbox(mailbox, uids, &target, role).await;
    }

    async fn move_to_mailbox(&mut self, mailbox: &MailboxId, uids: &[Uid], target: &MailboxId, role: MailboxRole) {
        if uids.is_empty() || mailbox == target {
            return;
        }
        let Some(destination) = graph_folder_id(&self.account_id, target).map(str::to_string) else {
            let _ = self.events.send(AccountEvent::Error("Couldn't move the message: unknown target folder".to_string())).await;
            return;
        };
        let ids = self.graph_ids(uids).await;
        let mut failed: Vec<Uid> = Vec::new();
        let mut moved: Vec<(Uid, super::GraphMessage)> = Vec::new();
        for uid in uids {
            let Some(graph_id) = ids.get(uid) else {
                failed.push(*uid);
                continue;
            };
            match self.client.move_message(graph_id, &destination).await {
                Ok(Some(message)) => moved.push((*uid, message)),
                Ok(None) => failed.push(*uid),
                Err(e) => {
                    tracing::warn!(%mailbox, "graph mail: move failed: {e}");
                    failed.push(*uid);
                }
            }
        }
        if !moved.is_empty() {
            self.store_moved(mailbox, target, &moved).await;
            self.emit_mailbox(mailbox, true).await;
            if self.current.as_ref() == Some(target) {
                self.emit_mailbox(target, false).await;
            }
            self.adjust_move_counts(mailbox, target, moved.len() as i64).await;
            let _ = self.events.send(AccountEvent::MessageMoved { role }).await;
        }
        if !failed.is_empty() {
            let _ = self
                .events
                .send(AccountEvent::MoveFailed {
                    mailbox: mailbox.clone(),
                    uids: failed,
                    role,
                    message: "Couldn't move the message".to_string(),
                })
                .await;
        }
    }

    /// Applies a move's cache consequences: a removal in the source folder and
    /// an upsert of the same Uid in the destination (item 5.7.4 rule 1). The
    /// row already in the source cache is reused, so flags, thread key and the
    /// 5.4 overlays travel with it; only a message this cache hadn't seen falls
    /// back to mapping Graph's move response.
    async fn store_moved(&mut self, source: &MailboxId, target: &MailboxId, moved: &[(Uid, super::GraphMessage)]) {
        let source = source.clone();
        let target = target.clone();
        let moved: Vec<(Uid, super::GraphMessage)> = moved.to_vec();
        if let Some(Err(e)) = cache_op(&self.cache, move |cache| -> lookout_mail::Result<()> {
            let validity = cache.graph_uid_validity()?;
            let source_uids: Vec<Uid> = moved.iter().map(|(uid, _)| *uid).collect();
            let cached = cache.load_summaries(&source, &source_uids)?;
            let summaries: Vec<EmailSummary> = moved
                .iter()
                .map(|(uid, message)| match cached.get(uid) {
                    Some(existing) => {
                        let mut summary = existing.clone();
                        summary.mailbox = target.clone();
                        summary
                    }
                    None => email_summary_from_graph(message, target.clone(), *uid),
                })
                .collect();
            cache.upsert_messages(&target, validity, &summaries)?;
            cache.delete_messages(&source, &source_uids)?;
            Ok(())
        })
        .await
        {
            tracing::warn!("failed to record a Graph move in the cache: {e}");
        }
    }

    async fn copy_messages(&mut self, mailbox: &MailboxId, uids: &[Uid], target: &MailboxId) {
        if uids.is_empty() || mailbox == target {
            return;
        }
        let Some(destination) = graph_folder_id(&self.account_id, target).map(str::to_string) else {
            let _ = self.events.send(AccountEvent::Error("Couldn't copy messages: unknown target folder".to_string())).await;
            return;
        };
        let ids = self.graph_ids(uids).await;
        let mut copied: Vec<(Uid, super::GraphMessage)> = Vec::new();
        for uid in uids {
            let Some(graph_id) = ids.get(uid) else { continue };
            match self.client.copy_message(graph_id, &destination).await {
                Ok(Some(copy)) => {
                    // The `copy` response carries only the new id; read the row
                    // so the target's cache gets a complete message instead of
                    // a blank one until its next delta.
                    let message = match self.client.get_summary(&copy.graph_id, self.zone).await {
                        Ok(Some(full)) => full,
                        _ => copy,
                    };
                    copied.push((*uid, message));
                }
                Ok(None) => {}
                Err(e) => {
                    let _ = self.events.send(AccountEvent::Error(format!("Couldn't copy messages: {e}"))).await;
                    return;
                }
            }
        }
        if copied.is_empty() {
            return;
        }
        // A copy is a new Graph id, so it gets a new account-unique Uid.
        let graph_ids: Vec<String> = copied.iter().map(|(_, message)| message.graph_id.clone()).collect();
        let new_uids = cache_op(&self.cache, move |cache| {
            let ids: Vec<&str> = graph_ids.iter().map(String::as_str).collect();
            cache.graph_uids(&ids)
        })
        .await
        .and_then(|r| r.ok())
        .unwrap_or_default();
        if new_uids.len() != copied.len() {
            let _ = self.events.send(AccountEvent::Error("Couldn't copy messages: no local cache".to_string())).await;
            return;
        }
        let target_owned = target.clone();
        let copied_messages: Vec<super::GraphMessage> = copied.iter().map(|(_, message)| message.clone()).collect();
        let stored_uids = new_uids.clone();
        if let Some(Err(e)) = cache_op(&self.cache, move |cache| -> lookout_mail::Result<()> {
            let validity = cache.graph_uid_validity()?;
            let summaries: Vec<EmailSummary> = copied_messages
                .iter()
                .zip(stored_uids.iter())
                .map(|(message, uid)| email_summary_from_graph(message, target_owned.clone(), *uid))
                .collect();
            cache.upsert_messages(&target_owned, validity, &summaries)
        })
        .await
        {
            tracing::warn!("failed to cache Graph copies: {e}");
        }
        for ((original, _), new_uid) in copied.iter().zip(new_uids.iter()) {
            self.copy_ledger.entry((mailbox.clone(), target.clone(), *original)).or_default().push(*new_uid);
        }
        self.bump_counts(target, Some(copied.len() as i64), Some(copied.len() as i64)).await;
        if self.current.as_ref() == Some(target) {
            self.emit_mailbox(target, false).await;
        }
    }

    async fn revert_copy(&mut self, copied_from: &MailboxId, copied_to: &MailboxId, uids: &[Uid]) {
        let mut doomed: Vec<Uid> = Vec::new();
        for uid in uids {
            if let Some(copies) = self.copy_ledger.remove(&(copied_from.clone(), copied_to.clone(), *uid)) {
                doomed.extend(copies);
            }
        }
        if doomed.is_empty() {
            let _ = self.events.send(AccountEvent::Error("Couldn't undo: the copies are no longer there".to_string())).await;
            return;
        }
        let ids = self.graph_ids(&doomed).await;
        for uid in &doomed {
            if let Some(graph_id) = ids.get(uid) {
                if let Err(e) = self.client.delete_message(graph_id).await {
                    tracing::warn!("graph mail: deleting a copy failed: {e}");
                }
            }
        }
        if let Some(Err(e)) = cache_op(&self.cache, {
            let copied_to = copied_to.clone();
            let doomed = doomed.clone();
            move |cache| cache.delete_messages(&copied_to, &doomed)
        })
        .await
        {
            tracing::warn!("failed to drop the un-copied messages from cache: {e}");
        }
        let count = doomed.len() as i64;
        self.bump_counts(copied_to, Some(-count), Some(-count)).await;
        self.emit_mailbox(copied_to, true).await;
    }

    async fn snooze(&mut self, mailbox: &MailboxId, uids: &[Uid], until: DateTime<Utc>) {
        if uids.is_empty() {
            return;
        }
        if let Some(Err(e)) = cache_op(&self.cache, {
            let mailbox = mailbox.clone();
            let uids = uids.to_vec();
            move |cache| cache.snooze_messages(&mailbox, &uids, until)
        })
        .await
        {
            let _ = self.events.send(AccountEvent::Error(format!("Couldn't snooze: {e}"))).await;
            return;
        }
        self.next_snooze_wake = cache_op(&self.cache, |cache| cache.next_snooze_wake()).await.and_then(|r| r.ok()).flatten();
        self.emit_mailbox(mailbox, false).await;
        let _ = self.events.send(AccountEvent::MessageSnoozed).await;
    }

    async fn unsnooze(&mut self, mailbox: &MailboxId, uids: &[Uid]) {
        if uids.is_empty() {
            return;
        }
        if let Some(Err(e)) = cache_op(&self.cache, {
            let mailbox = mailbox.clone();
            let uids = uids.to_vec();
            move |cache| cache.unsnooze_messages(&mailbox, &uids)
        })
        .await
        {
            let _ = self.events.send(AccountEvent::Error(format!("Couldn't unsnooze: {e}"))).await;
            return;
        }
        self.next_snooze_wake = cache_op(&self.cache, |cache| cache.next_snooze_wake()).await.and_then(|r| r.ok()).flatten();
        self.emit_mailbox(mailbox, false).await;
    }

    // --- Whole-folder operations -------------------------------------------------

    async fn empty_mailbox(&mut self, mailbox: &MailboxId) {
        let role = self.role_of(mailbox);
        let messages = self.all_messages(mailbox).await;
        let ids = self.graph_ids(&messages.iter().map(|message| message.uid).collect::<Vec<_>>()).await;
        for graph_id in ids.values() {
            if let Err(e) = self.client.delete_message(graph_id).await {
                tracing::warn!(%mailbox, "graph mail: emptying the folder failed: {e}");
            }
        }
        let uids: Vec<Uid> = messages.iter().map(|message| message.uid).collect();
        if let Some(Err(e)) = cache_op(&self.cache, {
            let mailbox = mailbox.clone();
            move |cache| cache.delete_messages(&mailbox, &uids)
        })
        .await
        {
            tracing::warn!("failed to clear the emptied folder's cache: {e}");
        }
        self.emit_mailbox(mailbox, true).await;
        self.set_unread(mailbox, 0).await;
        let _ = self.events.send(AccountEvent::MailboxExpunged { role }).await;
    }

    async fn mark_mailbox_read(&mut self, mailbox: &MailboxId) {
        let messages = self.all_messages(mailbox).await;
        let unread: Vec<&EmailSummary> = messages.iter().filter(|message| message.is_unread()).collect();
        let ids = self.graph_ids(&unread.iter().map(|message| message.uid).collect::<Vec<_>>()).await;
        let mut patched: Vec<Uid> = Vec::new();
        for uid in &unread {
            let Some(graph_id) = ids.get(&uid.uid) else { continue };
            let patch = MessagePatch {
                is_read: Some(true),
                ..Default::default()
            };
            match self.client.update_message(graph_id, &patch, self.zone).await {
                Ok(()) => patched.push(uid.uid),
                Err(e) => {
                    tracing::warn!(%mailbox, "graph mail: mark-all-read failed: {e}");
                    break;
                }
            }
        }
        let patched_count = patched.len();
        if let Some(Err(e)) = cache_op(&self.cache, {
            let mailbox = mailbox.clone();
            let patched = patched.clone();
            move |cache| cache.update_flags_many(&mailbox, &patched, &[SystemFlagBit::Seen], &[])
        })
        .await
        {
            tracing::warn!("failed to cache mark-all-read: {e}");
        }
        self.emit_mailbox(mailbox, false).await;
        let remaining = unread.len().saturating_sub(patched_count) as u32;
        self.set_unread(mailbox, remaining).await;
    }

    /// Every message in a folder: its cached rows when it has a full snapshot,
    /// else one full delta read (which also fills the cache).
    async fn all_messages(&mut self, mailbox: &MailboxId) -> Vec<EmailSummary> {
        let cached = cache_op(&self.cache, {
            let mailbox = mailbox.clone();
            move |cache| cache.load_messages(&mailbox)
        })
        .await
        .and_then(|r| r.ok())
        .unwrap_or_default();
        if !cached.is_empty() {
            return cached;
        }
        let Some(folder_id) = graph_folder_id(&self.account_id, mailbox).map(str::to_string) else {
            return Vec::new();
        };
        match self.client.folder_messages_delta(&folder_id, None, self.zone).await {
            Ok(delta) => {
                self.store_delta(mailbox, delta, Utc::now(), true).await;
                cache_op(&self.cache, {
                    let mailbox = mailbox.clone();
                    move |cache| cache.load_messages(&mailbox)
                })
                .await
                .and_then(|r| r.ok())
                .unwrap_or_default()
            }
            Err(e) => {
                tracing::warn!(%mailbox, "graph mail: reading the whole folder failed: {e}");
                Vec::new()
            }
        }
    }

    // --- Folder operations -------------------------------------------------------

    async fn create_mailbox(&mut self, parent: Option<&MailboxId>, name: &str) {
        let parent_id = parent.and_then(|parent| graph_folder_id(&self.account_id, parent));
        match self.client.create_folder(parent_id, name).await {
            Ok(Some(folder)) => {
                let id = graph_mailbox_id(&self.account_id, &folder.id);
                if let Err(e) = self.sync_folders(false).await {
                    tracing::warn!("created a folder but couldn't re-list: {e}");
                }
                let _ = self.events.send(AccountEvent::MailboxCreated { mailbox: id }).await;
            }
            Ok(None) => {
                let _ = self.events.send(AccountEvent::Error(format!("Couldn't create the folder \"{name}\""))).await;
            }
            Err(e) => {
                let _ = self.events.send(AccountEvent::Error(format!("Couldn't create the folder \"{name}\": {e}"))).await;
            }
        }
    }

    async fn rename_mailbox(&mut self, mailbox: &MailboxId, new_name: &str) {
        let Some(folder_id) = graph_folder_id(&self.account_id, mailbox).map(str::to_string) else {
            return;
        };
        match self.client.rename_folder(&folder_id, new_name).await {
            Ok(()) => {
                if let Err(e) = self.sync_folders(false).await {
                    tracing::warn!("renamed a folder but couldn't re-list: {e}");
                }
                // Graph folder ids are stable across a rename, so nothing keyed
                // by id needs re-keying (item 5.7.4 rule 4); this event still
                // refreshes the pane.
                let _ = self
                    .events
                    .send(AccountEvent::MailboxRenamed {
                        from: mailbox.clone(),
                        to: mailbox.clone(),
                        delimiter: '/',
                        to_trash: false,
                    })
                    .await;
            }
            Err(e) => {
                let _ = self.events.send(AccountEvent::Error(format!("Couldn't rename the folder: {e}"))).await;
            }
        }
    }

    /// Deletes a folder. Outside Trash, its messages are first moved to Trash
    /// so nothing is permanently lost, then the (empty) folder is deleted -
    /// Graph has no folder move that could keep it recoverable.
    async fn delete_mailbox(&mut self, mailbox: &MailboxId) {
        if self.role_of(mailbox) != MailboxRole::Trash {
            if let Some(trash) = self.folder_id(MailboxRole::Trash) {
                if trash != *mailbox {
                    let messages = self.all_messages(mailbox).await;
                    let uids: Vec<Uid> = messages.iter().map(|message| message.uid).collect();
                    self.move_to_mailbox(mailbox, &uids, &trash, MailboxRole::Trash).await;
                }
            }
        }
        let Some(folder_id) = graph_folder_id(&self.account_id, mailbox).map(str::to_string) else {
            return;
        };
        match self.client.delete_folder(&folder_id).await {
            Ok(()) => {
                if let Some(index) = self.folders.iter().position(|folder| folder.id == *mailbox) {
                    self.folders.remove(index);
                }
                self.forget_mailbox(mailbox).await;
                if self.current.as_ref() == Some(mailbox) {
                    self.current = self.folders.iter().find(|folder| folder.role == MailboxRole::Inbox).map(|folder| folder.id.clone());
                }
                self.publish_folders().await;
                let _ = self
                    .events
                    .send(AccountEvent::MailboxDeleted {
                        mailbox: mailbox.clone(),
                        delimiter: '/',
                    })
                    .await;
            }
            Err(e) => {
                let _ = self.events.send(AccountEvent::Error(format!("Couldn't delete the folder: {e}"))).await;
            }
        }
    }

    // --- Drafts ------------------------------------------------------------------

    /// Saves a draft as raw MIME (`POST /me/messages` with `text/rfc822`), the
    /// only form that preserves the compose session's stable `Message-ID` and
    /// its Bcc header - both of which make repeated autosaves replace rather
    /// than duplicate, exactly as the IMAP `APPEND` path does.
    async fn save_draft(&mut self, message: ComposedMessage, replace: DraftReplace) {
        let existing = match &replace {
            DraftReplace::None => None,
            DraftReplace::ByUid { mailbox: _, uid } => self.graph_id(*uid).await,
            DraftReplace::ByMessageId => match message.message_id.as_deref() {
                Some(message_id) => self.find_draft(message_id).await,
                None => None,
            },
        };
        if let Some(graph_id) = existing {
            if let Err(e) = self.client.delete_message(&graph_id).await {
                tracing::warn!("couldn't replace the previous draft copy: {e}");
            }
        }
        let (raw, message_id, _recipients) = build_draft_raw_message(&message);
        match self.client.create_draft_mime(&raw).await {
            Ok(Some(draft)) => {
                // Put it in the Drafts folder's cache now, so the list shows it
                // without waiting for the next delta.
                if let Some(drafts) = self.folder_id(MailboxRole::Drafts) {
                    let id = draft.graph_id.clone();
                    let uids = cache_op(&self.cache, move |cache| cache.graph_uids(&[id.as_str()]))
                        .await
                        .and_then(|r| r.ok())
                        .unwrap_or_default();
                    if let Some(uid) = uids.first().copied() {
                        let summary = email_summary_from_graph(&draft, drafts.clone(), uid);
                        let validity = self.validity().await;
                        let target = drafts.clone();
                        if let Some(Err(e)) = cache_op(&self.cache, move |cache| cache.upsert_messages(&target, validity, &[summary])).await {
                            tracing::warn!("failed to cache a saved draft: {e}");
                        }
                    }
                    if self.current.as_ref() == Some(&drafts) {
                        self.emit_mailbox(&drafts, false).await;
                    }
                }
                let _ = self.events.send(AccountEvent::DraftSaved { message_id }).await;
            }
            Ok(None) => tracing::warn!("save-draft returned no message"),
            Err(e) => tracing::warn!("couldn't save the draft: {e}"),
        }
    }

    /// Finds a copy in Drafts by its RFC 822 Message-ID, so deleting a draft
    /// never risks matching the sent copy in Sent Items.
    async fn find_draft(&mut self, message_id: &str) -> Option<String> {
        let drafts = self.folder_id(MailboxRole::Drafts)?;
        let folder_id = graph_folder_id(&self.account_id, &drafts)?.to_string();
        match self.client.resolve_message_id_in(&folder_id, message_id).await {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!("couldn't look up the draft: {e}");
                None
            }
        }
    }

    async fn delete_draft(&mut self, message_id: &str) -> Result<(), GraphError> {
        if let Some(graph_id) = self.find_draft(message_id).await {
            self.client.delete_message(&graph_id).await?;
        }
        Ok(())
    }

    // --- Search ------------------------------------------------------------------

    /// The live-search fallback. Lookout's FTS5 cache is still the primary
    /// search on this transport (R2); this answers the same command with
    /// Graph's `$search` for mail the cache hasn't seen.
    async fn search_mailbox(&mut self, mailbox: &MailboxId, query: &str) {
        let folder_id = graph_folder_id(&self.account_id, mailbox).map(str::to_string);
        match self.client.search_messages(folder_id.as_deref(), query, MAX_SEARCH_RESULTS, self.zone).await {
            Ok(found) => {
                let mut messages = found;
                allocation_order(&mut messages);
                let ids: Vec<String> = messages.iter().map(|message| message.graph_id.clone()).collect();
                let uids = cache_op(&self.cache, move |cache| {
                    let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
                    cache.graph_uids(&ids)
                })
                .await
                .and_then(|r| r.ok())
                .unwrap_or_default();
                let summaries: Vec<EmailSummary> = messages
                    .iter()
                    .zip(uids.iter())
                    .map(|(message, uid)| email_summary_from_graph(message, mailbox.clone(), *uid))
                    .collect();
                let validity = self.validity().await;
                if let Some(Err(e)) = cache_op(&self.cache, {
                    let mailbox = mailbox.clone();
                    let summaries = summaries.clone();
                    move |cache| cache.upsert_messages(&mailbox, validity, &summaries)
                })
                .await
                {
                    tracing::warn!("failed to cache Graph search results: {e}");
                }
                let _ = self
                    .events
                    .send(AccountEvent::SearchResults {
                        mailbox: mailbox.clone(),
                        query: query.to_string(),
                        messages: summaries,
                    })
                    .await;
            }
            Err(e) => {
                tracing::warn!(%mailbox, "graph mail search failed: {e}");
                let _ = self.events.send(AccountEvent::Error(format!("Couldn't search the mailbox: {e}"))).await;
                let _ = self
                    .events
                    .send(AccountEvent::SearchResults {
                        mailbox: mailbox.clone(),
                        query: query.to_string(),
                        messages: Vec::new(),
                    })
                    .await;
            }
        }
    }

    // --- Outbox commands ---------------------------------------------------------

    async fn queue_send(&mut self, message: ComposedMessage, not_before: Option<DateTime<Utc>>, client_token: Option<u64>) {
        let not_before = not_before.unwrap_or_else(Utc::now);
        let payload = match serde_json::to_vec(&OutboxMessage::from_composed(&message)) {
            Ok(payload) => payload,
            Err(e) => {
                let _ = self
                    .events
                    .send(AccountEvent::OutboxFailed {
                        id: 0,
                        client_token,
                        message: format!("Couldn't queue the message: {e}"),
                    })
                    .await;
                return;
            }
        };
        let account_id = self.account_id.clone();
        let queued = cache_op(&self.cache, move |cache| cache.enqueue_outbox(&account_id, not_before, &payload))
            .await
            .and_then(|r| r.ok());
        match queued {
            Some(id) => {
                let _ = self.events.send(AccountEvent::OutboxQueued { id, client_token, not_before }).await;
                self.next_outbox_due = cache_op(&self.cache, |cache| cache.next_outbox_not_before()).await.and_then(|r| r.ok()).flatten();
                self.emit_outbox().await;
            }
            None => {
                let _ = self
                    .events
                    .send(AccountEvent::OutboxFailed {
                        id: 0,
                        client_token,
                        message: "Couldn't queue the message: the local outbox is unavailable".to_string(),
                    })
                    .await;
            }
        }
    }

    async fn cancel_outbox(&mut self, id: i64) {
        let entry = cache_op(&self.cache, move |cache| cache.cancel_outbox(id)).await.and_then(|r| r.ok()).flatten();
        match entry {
            Some(entry) => {
                let message = serde_json::from_slice::<OutboxMessage>(&entry.payload).ok().map(Box::new);
                let _ = self.events.send(AccountEvent::OutboxCancelled { id, message }).await;
            }
            None => {
                let _ = self.events.send(AccountEvent::OutboxCancelFailed { id }).await;
            }
        }
        self.next_outbox_due = cache_op(&self.cache, |cache| cache.next_outbox_not_before()).await.and_then(|r| r.ok()).flatten();
        self.emit_outbox().await;
    }

    async fn reschedule_outbox(&mut self, id: i64, not_before: DateTime<Utc>) {
        cache_op(&self.cache, move |cache| cache.reschedule_outbox(id, not_before)).await;
        self.next_outbox_due = cache_op(&self.cache, |cache| cache.next_outbox_not_before()).await.and_then(|r| r.ok()).flatten();
        self.emit_outbox().await;
    }

    // --- Small shared helpers ----------------------------------------------------

    async fn validity(&self) -> UidValidity {
        cache_op(&self.cache, |cache| cache.graph_uid_validity())
            .await
            .and_then(|r| r.ok())
            .unwrap_or(UidValidity(0))
    }

    async fn graph_id(&self, uid: Uid) -> Option<String> {
        cache_op(&self.cache, move |cache| cache.graph_ids_for(&[uid]))
            .await
            .and_then(|r| r.ok())
            .and_then(|mut ids| ids.remove(&uid))
    }

    /// The Graph ids behind `uids`, keyed by Uid. Uids with no mapping are
    /// absent.
    async fn graph_ids(&self, uids: &[Uid]) -> HashMap<Uid, String> {
        let uids = uids.to_vec();
        cache_op(&self.cache, move |cache| cache.graph_ids_for(&uids))
            .await
            .and_then(|r| r.ok())
            .unwrap_or_default()
    }

    /// The cached summaries for `uids` (with overlays applied).
    async fn summaries(&self, mailbox: &MailboxId, uids: &[Uid]) -> Vec<EmailSummary> {
        cache_op(&self.cache, {
            let mailbox = mailbox.clone();
            let uids = uids.to_vec();
            move |cache| cache.load_summaries(&mailbox, &uids)
        })
        .await
        .and_then(|r| r.ok())
        .map(|found| found.into_values().collect())
        .unwrap_or_default()
    }

    fn category_name(&self, key: &str) -> String {
        self.category_names.get(key).cloned().unwrap_or_else(|| key.to_string())
    }

    async fn adjust_unread(&mut self, mailbox: &MailboxId, delta: i64) {
        self.bump_counts(mailbox, None, Some(delta)).await;
    }

    async fn adjust_move_counts(&mut self, from: &MailboxId, to: &MailboxId, count: i64) {
        self.bump_counts(from, Some(-count), None).await;
        self.bump_counts(to, Some(count), None).await;
    }

    async fn set_unread(&mut self, mailbox: &MailboxId, unread: u32) {
        let changed = match self.folders.iter_mut().find(|folder| folder.id == *mailbox) {
            Some(folder) => {
                folder.unread = unread;
                true
            }
            None => false,
        };
        if changed {
            self.publish_folders().await;
        }
    }

    async fn bump_counts(&mut self, mailbox: &MailboxId, total: Option<i64>, unread: Option<i64>) {
        let changed = match self.folders.iter_mut().find(|folder| folder.id == *mailbox) {
            Some(folder) => {
                if let Some(delta) = total {
                    folder.total = (folder.total as i64 + delta).max(0) as u32;
                }
                if let Some(delta) = unread {
                    folder.unread = (folder.unread as i64 + delta).max(0) as u32;
                }
                true
            }
            None => false,
        };
        if changed {
            self.publish_folders().await;
        }
    }
}

/// `\Seen` maps to Graph's `isRead`; every other system flag has no Graph
/// property (R6). A `None` means the command only touches unsupported flags.
fn read_state(add: &[SystemFlagBit], remove: &[SystemFlagBit]) -> Option<bool> {
    if add.contains(&SystemFlagBit::Seen) {
        Some(true)
    } else if remove.contains(&SystemFlagBit::Seen) {
        Some(false)
    } else {
        None
    }
}

/// The Lookout category keys among `$Lookout-tag-<key>` atoms.
fn tag_keys(atoms: &[String]) -> Vec<String> {
    atoms.iter().filter_map(|atom| lookout_core::tag_key_from_keyword(atom).map(str::to_string)).collect()
}

/// Graph property records for the 5.4 overlay tables. A message without a
/// Message-ID (some drafts) can't carry them (item 5.7.4 rule 2); its categories
/// and flag still reach the list through the summary itself on this transport.
fn remote_properties(messages: &[super::GraphMessage]) -> Vec<RemoteMailProperties> {
    messages
        .iter()
        .filter_map(|message| {
            let message_id = message.message_id.clone()?;
            Some(RemoteMailProperties {
                message_id,
                graph_id: message.graph_id.clone(),
                categories: message.categories.iter().map(|name| lookout_core::sanitize_tag_key(name)).collect(),
                importance: message.importance,
                classification: message.classification,
                flag: message.flag.clone(),
            })
        })
        .collect()
}

/// Adds a `Bcc:` header to raw outbox bytes for Graph's MIME `sendMail`, whose
/// recipient list comes from the headers. The outbox built them without Bcc
/// (SMTP takes the blind list on the envelope, and a header would leak it);
/// Exchange strips `Bcc` on delivery, so writing it here is how a Graph send
/// reaches a blind recipient. A payload that already carries one is untouched.
fn with_bcc_header(raw: Vec<u8>, bcc: &[String]) -> Vec<u8> {
    if bcc.is_empty() {
        return raw;
    }
    let header_end = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .or_else(|| raw.windows(2).position(|window| window == b"\n\n"));
    let Some(end) = header_end else { return raw };
    let headers = String::from_utf8_lossy(&raw[..end]);
    if headers.lines().any(|line| line.to_ascii_lowercase().starts_with("bcc:")) {
        return raw;
    }
    let mut out = Vec::with_capacity(raw.len() + bcc.len() * 32);
    out.extend_from_slice(&raw[..end]);
    out.extend_from_slice(b"\r\nBcc: ");
    out.extend_from_slice(bcc.join(", ").as_bytes());
    out.extend_from_slice(&raw[end..]);
    out
}

/// The outbox's retry backoff, copied from the IMAP session: 60 s doubling to
/// an hour.
fn outbox_retry_delay(attempts: u32) -> chrono::Duration {
    let shift = attempts.saturating_sub(1).min(6);
    chrono::Duration::seconds(60i64.saturating_mul(1i64 << shift).min(60 * 60))
}

fn role_label(role: MailboxRole) -> &'static str {
    match role {
        MailboxRole::Inbox => "Inbox",
        MailboxRole::Sent => "Sent",
        MailboxRole::Drafts => "Drafts",
        MailboxRole::Trash => "Trash",
        MailboxRole::Junk => "Junk",
        MailboxRole::Archive => "Archive",
        MailboxRole::Custom => "target",
    }
}

async fn cache_op<T, F>(cache: &CacheHandle, op: F) -> Option<T>
where
    T: Send + 'static,
    F: FnOnce(&Cache) -> T + Send + 'static,
{
    let cache = cache.clone();
    tokio::task::spawn_blocking(move || (*cache).as_ref().map(op)).await.ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lookout_core::FlagStatus;

    #[test]
    fn bcc_is_added_to_raw_outbox_bytes_once() {
        let raw = b"From: me@example.com\r\nTo: you@example.com\r\nSubject: hi\r\n\r\nbody\r\n";
        let with = with_bcc_header(raw.to_vec(), &["blind@example.com".to_string()]);
        let text = String::from_utf8(with).unwrap();
        assert!(text.contains("\r\nBcc: blind@example.com\r\n\r\nbody"), "{text}");
        // A payload that already carries a Bcc header is left alone.
        let already = b"To: you@example.com\r\nBcc: blind@example.com\r\n\r\nbody".to_vec();
        assert_eq!(with_bcc_header(already.clone(), &["other@example.com".to_string()]), already);
        // Nothing to add, and a malformed message, are no-ops.
        assert_eq!(with_bcc_header(raw.to_vec(), &[]), raw.to_vec());
        assert_eq!(with_bcc_header(b"no separator".to_vec(), &["a@b".to_string()]), b"no separator".to_vec());
    }

    #[test]
    fn only_seen_maps_to_a_graph_read_state() {
        assert_eq!(read_state(&[SystemFlagBit::Seen], &[]), Some(true));
        assert_eq!(read_state(&[], &[SystemFlagBit::Seen]), Some(false));
        assert_eq!(read_state(&[SystemFlagBit::Flagged], &[]), None);
        assert_eq!(read_state(&[SystemFlagBit::Seen], &[SystemFlagBit::Flagged]), Some(true));
    }

    #[test]
    fn outbox_retries_back_off_and_cap() {
        assert_eq!(outbox_retry_delay(1).num_seconds(), 60);
        assert_eq!(outbox_retry_delay(2).num_seconds(), 120);
        assert_eq!(outbox_retry_delay(100).num_seconds(), 3600);
    }

    #[test]
    fn tag_keys_read_the_lookout_namespace_only() {
        assert_eq!(
            tag_keys(&["$Lookout-tag-work".to_string(), "\\Seen".to_string(), "$Junk".to_string()]),
            vec!["work".to_string()]
        );
    }

    #[test]
    fn flag_due_dates_survive_the_remote_property_round_trip() {
        let message = super::super::GraphMessage {
            graph_id: "g1".to_string(),
            change_key: None,
            folder_id: None,
            conversation_id: None,
            message_id: Some("<a@b>".to_string()),
            subject: None,
            preview: None,
            from: Vec::new(),
            to: Vec::new(),
            cc: Vec::new(),
            received: None,
            is_read: false,
            is_draft: false,
            has_attachments: false,
            is_meeting: false,
            flag: Some(lookout_core::FollowUpFlag {
                status: FlagStatus::Flagged,
                ..Default::default()
            }),
            categories: vec!["Project Xmas".to_string()],
            importance: Importance::High,
            classification: InferenceClassification::Other,
        };
        let properties = remote_properties(&[message]);
        assert_eq!(properties.len(), 1);
        assert_eq!(properties[0].message_id, "<a@b>");
        assert_eq!(properties[0].categories, vec!["project-xmas".to_string()]);
        assert_eq!(properties[0].importance, Importance::High);
        assert_eq!(properties[0].classification, InferenceClassification::Other);
        assert_eq!(properties[0].flag.as_ref().unwrap().status, FlagStatus::Flagged);
    }
}
