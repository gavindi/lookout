//! Session wiring: account, calendar, subscription and Google Tasks discovery, connection, event loops and teardown.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};
use lookout_core::calendar_protocol::{CalendarCommand, CalendarSessionEvent, ConnectionState as CalConnectionState};
use lookout_core::{AccountId, CalendarId, CalendarInfo, MailboxId, MailboxRole, SchedulingInfo, Uid};
use lookout_dav::subscription::SubscriptionSessionEvent;
use lookout_dav::CalendarAccountConfig;
use lookout_goa::{GoaCalendarAccount, GoaClient};
use lookout_mail::session::{AccountCommand, AccountEvent, ConnectionState};
use lookout_mail::{AccountConfig, EndpointConfig};

use crate::calendar_colors;
use crate::calendar_view::{self, CalendarMain};
use crate::contacts_view::sync_contacts_account;
use crate::goa_calendar_credentials::GoaCalendarCredentialProvider;
use crate::goa_credentials::GoaCredentialProvider;
use crate::google_tasks::{self, GoogleTasksEvent};
use crate::message_list::MessageListModel;
use crate::microsoft_oauth::MicrosoftCredentialProvider;
use crate::worker::Worker;

use super::*;

#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_account_discovery(
    worker: Rc<Worker>,
    state: Rc<RefCell<UiState>>,
    root_stack: gtk::Stack,
    toast_overlay: adw::ToastOverlay,
    window: adw::ApplicationWindow,
    app: adw::Application,
    folder_selection: gtk::SingleSelection,
    folder_scroller: gtk::ScrolledWindow,
    message_list: MessageListModel,
    message_list_stack: gtk::Stack,
    message_header: crate::message_header::MessageHeader,
    reading_stack: gtk::Stack,
    current_mail_page: Rc<Cell<&'static str>>,
    mail_view_button: gtk::ToggleButton,
    list_header: ListHeader,
    refresh_config: Rc<dyn Fn()>,
    current_lookout_page: Rc<Cell<&'static str>>,
    lookout_view_button: gtk::ToggleButton,
    dashboard_refresh: Rc<dyn Fn()>,
    mark_read_button: gtk::Button,
    pin_button: gtk::Button,
) {
    let (goa_tx, goa_rx) = async_channel::bounded(1);
    worker.spawn(async move {
        // Closing the window normally keeps Lookout running. Re-discover
        // accounts added in GNOME Settings without requiring a real quit.
        loop {
            let result = async {
                let client = GoaClient::connect().await?;
                let accounts = client.list_mail_accounts().await?;
                Ok::<_, lookout_goa::Error>((client, accounts))
            }
            .await;
            let failed = result.is_err();
            if goa_tx.send(result).await.is_err() || failed {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(15)).await;
        }
    });

    glib::spawn_future_local(async move {
        while let Ok(result) = goa_rx.recv().await {
            let show_page = |name: &'static str| {
                current_mail_page.set(name);
                if mail_view_button.is_active() {
                    root_stack.set_visible_child_name(name);
                }
            };
            // The Lookout tab flips between its status page (no accounts at all)
            // and the live dashboard once *any* account set exists. Mail
            // discovery owns the downgrade: only it knows whether nothing is
            // connected; the calendar discovery only ever upgrades.
            let show_lookout_page = |name: &'static str| {
                current_lookout_page.set(name);
                if lookout_view_button.is_active() {
                    root_stack.set_visible_child_name(name);
                }
            };
            match result {
                Ok((client, accounts)) => {
                    // Record every discovered account (disabled ones included -
                    // Config's account list and a later re-enable need them),
                    // then connect only the enabled ones. An account list of
                    // nothing-but-disabled accounts therefore falls through to
                    // the same empty/fallback handling as GOA reporting none at
                    // all - the disabled accounts stay visible in Config.
                    let mut enabled_accounts = Vec::new();
                    for account in accounts {
                        let id = AccountId(account.account_id.0.clone());
                        let enabled = {
                            let mut st = state.borrow_mut();
                            st.goa_client = Some(client.clone());
                            let entry = st.goa_accounts.entry(id.clone()).or_insert(DiscoveredGoaAccount {
                                display_name: String::new(),
                                email: String::new(),
                                provider_type: None,
                                mail: None,
                                calendar: None,
                                contacts: None,
                            });
                            entry.display_name = account.display_name.clone();
                            entry.email = account.email.clone();
                            entry.provider_type = account.provider_type.clone();
                            entry.mail = Some(account.clone());
                            st.account_enabled(&id)
                        };
                        // Keep existing sessions and caches intact on subsequent
                        // discovery passes, including sessions still connecting.
                        if enabled && !state.borrow().accounts.contains_key(&id) {
                            enabled_accounts.push(account);
                        }
                    }
                    if !enabled_accounts.is_empty() {
                        show_page("mail");
                        show_lookout_page("lookout");
                        // One AccountSession actor per connected account, all
                        // running concurrently on the worker thread. `GoaClient` is
                        // a cheap Arc-backed handle (see its doc comment), so
                        // cloning it per account reuses the one D-Bus connection
                        // rather than opening a redundant one each time.
                        for account in enabled_accounts {
                            connect_account(
                                worker.clone(),
                                state.clone(),
                                folder_selection.clone(),
                                folder_scroller.clone(),
                                message_list.clone(),
                                message_list_stack.clone(),
                                message_header.clone(),
                                reading_stack.clone(),
                                toast_overlay.clone(),
                                window.clone(),
                                app.clone(),
                                client.clone(),
                                list_header.clone(),
                                account,
                                dashboard_refresh.clone(),
                                mark_read_button.clone(),
                                pin_button.clone(),
                            );
                        }
                        refresh_config();
                    }
                    // GOA itself reported no (or zero usable) accounts - but manual
                    // "other" accounts are connected synchronously, before this async
                    // D-Bus round trip can land, so `state.accounts` may already be
                    // non-empty by the time this branch runs. Only show the empty
                    // state if nothing at all is connected.
                    else if state.borrow().accounts.is_empty() {
                        show_page("empty");
                        show_lookout_page("lookout-empty");
                    } else {
                        show_page("mail");
                        show_lookout_page("lookout");
                    }
                }
                Err(e) => {
                    if state.borrow().accounts.is_empty() {
                        show_page("empty");
                        show_lookout_page("lookout-empty");
                    } else {
                        show_page("mail");
                        show_lookout_page("lookout");
                    }
                    if e.is_service_unavailable() {
                        // Expected on a desktop with no GOA daemon (e.g. KDE) -
                        // silently note it so Config can hide the GOA section
                        // instead of nagging with a toast every launch.
                        state.borrow_mut().goa_unavailable = true;
                        refresh_config();
                    } else {
                        let title = glib::markup_escape_text(&format!("Couldn't reach GNOME Online Accounts: {e}"));
                        toast_overlay.add_toast(adw::Toast::new(&title));
                    }
                }
            }
        }
    });
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn connect_account(
    worker: Rc<Worker>,
    state: Rc<RefCell<UiState>>,
    folder_selection: gtk::SingleSelection,
    folder_scroller: gtk::ScrolledWindow,
    message_list: MessageListModel,
    message_list_stack: gtk::Stack,
    message_header: crate::message_header::MessageHeader,
    reading_stack: gtk::Stack,
    toast_overlay: adw::ToastOverlay,
    window: adw::ApplicationWindow,
    app: adw::Application,
    goa_client: GoaClient,
    list_header: ListHeader,
    account: lookout_goa::GoaMailAccount,
    dashboard_refresh: Rc<dyn Fn()>,
    mark_read_button: gtk::Button,
    pin_button: gtk::Button,
) {
    let account_id = AccountId(account.account_id.0.clone());
    let display_name = account.display_name.clone();
    let (mention_addresses, mention_names) = mention_identity_lists(&state.borrow().app_config.borrow().identities_for_account(&account_id, &display_name, &account.email));
    let config = AccountConfig {
        account_id: account_id.clone(),
        display_name: account.display_name.clone(),
        email: account.email.clone(),
        mention_addresses,
        mention_names,
        imap: EndpointConfig {
            host: account.imap.host.clone(),
            port: account.imap.port.unwrap_or(993),
            use_tls: account.imap.use_tls,
            username: account.imap.username.clone(),
        },
        smtp: EndpointConfig {
            host: account.smtp.host.clone(),
            port: account.smtp.port.unwrap_or(587),
            use_tls: account.smtp.use_tls,
            username: account.smtp.username.clone(),
        },
    };
    let is_microsoft_365 = account.is_microsoft_365();
    let credentials: Rc<dyn lookout_mail::session::CredentialProvider> = if is_microsoft_365 {
        // GOA's Microsoft 365 token carries only Microsoft Graph scopes and
        // can't authenticate to Exchange Online IMAP/SMTP (verified live), so
        // Microsoft accounts use Lookout's own OAuth2 flow instead - see
        // `microsoft_oauth.rs`. The first connect opens a browser; afterwards
        // the stored refresh token keeps it silent.
        Rc::new(MicrosoftCredentialProvider::new(account_id.clone()))
    } else {
        Rc::new(GoaCredentialProvider::new(goa_client, account))
    };
    // `run_account_session` requires `Arc<dyn CredentialProvider>` (it may
    // run reconnect attempts on the worker thread's own async tasks), so
    // wrap in a thread-safe handle even though only ever driven from one
    // worker task at a time.
    struct SendWrapper(Rc<dyn lookout_mail::session::CredentialProvider>);
    // SAFETY: the wrapped provider only ever runs on the single worker
    // thread's tokio tasks for this account; it is never touched from the
    // UI thread after being handed off here.
    unsafe impl Send for SendWrapper {}
    unsafe impl Sync for SendWrapper {}
    #[async_trait::async_trait]
    impl lookout_mail::session::CredentialProvider for SendWrapper {
        async fn imap_credential(&self) -> Result<lookout_mail::Credential, String> {
            self.0.imap_credential().await
        }

        async fn smtp_credential(&self) -> Result<lookout_mail::Credential, String> {
            self.0.smtp_credential().await
        }
    }
    let credentials: std::sync::Arc<dyn lookout_mail::session::CredentialProvider> = std::sync::Arc::new(SendWrapper(credentials));

    // Commands stay unbounded (sent via `send_blocking` from the main thread);
    // events are bounded so the drain coalesces a batch and stalls the session
    // under a flood instead of queueing unboundedly - see `EVENT_CHANNEL_CAPACITY`.
    // Two command channels: user-facing fetches (`FetchBody`/`FetchAttachment`/
    // `FetchRawMessage`) ride the interactive one, which the session drains
    // ahead of the background one - see `AccountHandle::interactive_cmd_tx`.
    let (cmd_tx, cmd_rx) = async_channel::unbounded();
    let (interactive_cmd_tx, interactive_cmd_rx) = async_channel::unbounded();
    let (evt_tx, evt_rx) = async_channel::bounded(EVENT_CHANNEL_CAPACITY);
    state.borrow_mut().accounts.insert(
        account_id.clone(),
        AccountHandle {
            cmd_tx,
            interactive_cmd_tx,
            email: config.email.clone(),
            display_name,
            imap_host: config.imap.host.clone(),
            imap_port: config.imap.port,
            smtp_host: config.smtp.host.clone(),
            smtp_port: config.smtp.port,
            folders: Vec::new(),
            // Opened off the GTK thread below and patched in once ready - see
            // `spawn_cache_open`. The session opens (and creates) the same
            // file; whichever gets there first wins, and until either lands
            // the field just means no composer suggestions yet.
            address_cache: None,
            // Independent of the `credentials` provider constructed above -
            // see the field's own doc comment on `AccountHandle`.
            graph_pin: is_microsoft_365.then(|| Rc::new(crate::graph_pin::GraphPinClient::new(account_id.clone()))),
            graph_mail: is_microsoft_365.then(|| Rc::new(crate::graph_mail::GraphMailClient::new(account_id.clone()))),
            graph_categories: is_microsoft_365.then(|| Rc::new(crate::graph_categories::GraphCategoriesClient::new(account_id.clone()))),
            graph_rules: is_microsoft_365.then(|| Rc::new(crate::graph_rules::GraphRulesClient::new(account_id.clone()))),
            graph_automatic_replies: is_microsoft_365.then(|| Rc::new(crate::graph_automatic_replies::GraphAutomaticRepliesClient::new(account_id.clone()))),
            sieve_advertised: false,
        },
    );
    rebuild_folder_tree(&state, &folder_selection, &folder_scroller);
    message_list.set_viewer_addresses(viewer_addresses(&state.borrow()));
    if is_microsoft_365 {
        // Read Outlook's flags once the session has had a moment to open its
        // cache (item 1.16); `GRAPH_FLAG_RESYNC` and F5 keep them fresh.
        let flags_state = state.clone();
        let flags_account = account_id.clone();
        glib::timeout_add_seconds_local_once(10, move || sync_follow_ups_from_graph(&flags_state, &flags_account));
        // Pull Outlook's category master list into the shared list (item 2.1),
        // a little after the flags, so the message list has rendered first.
        let categories_state = state.clone();
        let categories_message_list = message_list.clone();
        let categories_account = account_id.clone();
        glib::timeout_add_seconds_local_once(11, move || sync_categories_from_graph(&categories_state, &categories_message_list, &categories_account));
        // Pull (and push) Outlook's inbox rules (item 2.5), last of the three
        // so the category list it may reference is already in place.
        let rules_state = state.clone();
        let rules_account = account_id.clone();
        glib::timeout_add_seconds_local_once(12, move || sync_and_push_rules_from_graph(&rules_state, &rules_account));
        // Pull the mailbox's automatic replies (item 2.11 / M-24), after the
        // rules, so the Mail banner can reveal on startup.
        let replies_state = state.clone();
        let replies_account = account_id.clone();
        glib::timeout_add_seconds_local_once(13, move || sync_automatic_replies_from_graph(&replies_state, &replies_account));
    }
    {
        let reply_rx = spawn_cache_open(&worker, account_id.clone());
        let state = state.clone();
        let account_id = account_id.clone();
        glib::spawn_future_local(async move {
            let Ok(cache) = reply_rx.recv().await else { return };
            if let Some(handle) = state.borrow_mut().accounts.get_mut(&account_id) {
                handle.address_cache = cache;
            }
        });
    }

    worker.spawn(lookout_mail::session::run_account_session(config, credentials, cmd_rx, interactive_cmd_rx, evt_tx));

    spawn_account_event_loop(
        evt_rx,
        state,
        account_id,
        folder_selection,
        folder_scroller,
        message_list,
        message_list_stack,
        message_header,
        reading_stack,
        toast_overlay,
        window,
        app,
        list_header,
        dashboard_refresh,
        mark_read_button,
        pin_button,
        worker,
    );
}

/// The identity of a whole-snapshot session event: the one value a later copy
/// of the same event supersedes. An earlier snapshot of a key is dropped by
/// `collapse_last_wins` - replaying both would repaint the same surface twice
/// with the later, strictly-newer data.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) enum SnapshotKey {
    /// `AccountEvent::FoldersUpdated` - the whole folder list.
    Folders,
    /// `AccountEvent::MessagesUpdated` - one mailbox's full message set.
    Mailbox(MailboxId),
    /// `CalendarSessionEvent::CalendarsUpdated` - the whole calendar list.
    Calendars,
    /// `CalendarSessionEvent::OccurrencesUpdated` - one month's occurrences.
    Occurrences(chrono::NaiveDate),
    /// `TasksUpdated` from a calendar account or Google Tasks.
    Tasks,
    /// `AccountEvent::FlaggedMessages` - the account's whole flagged-mail set.
    FlaggedMail,
    /// `GoogleTasksEvent::ListsUpdated`.
    TaskLists,
    /// `SubscriptionSessionEvent::SubscriptionsUpdated` - one month's feeds.
    Subscriptions(chrono::NaiveDate),
}

/// Keeps the events of one drained batch in order, minus the superseded
/// copies: each event for which `supersedable` returns a key survives only in
/// its last occurrence, everything else is untouched in its original position.
/// The startup burst queues the same snapshot several times in a row (cache
/// replay, live sync, previews), and without this the UI would repopulate the
/// same surface once per queued copy.
pub(crate) fn collapse_last_wins<T>(events: Vec<T>, supersedable: impl Fn(&T) -> Option<SnapshotKey>) -> Vec<T> {
    let mut last: HashMap<SnapshotKey, usize> = HashMap::new();
    for (i, event) in events.iter().enumerate() {
        if let Some(key) = supersedable(event) {
            last.insert(key, i);
        }
    }
    let mut collapsed = Vec::with_capacity(events.len());
    for (i, event) in events.into_iter().enumerate() {
        match supersedable(&event) {
            Some(key) if last.get(&key) == Some(&i) => collapsed.push(event),
            Some(_) => {}
            None => collapsed.push(event),
        }
    }
    collapsed
}

pub(crate) fn collapse_account_events(events: Vec<AccountEvent>) -> Vec<AccountEvent> {
    collapse_last_wins(events, |event| match event {
        AccountEvent::FoldersUpdated(_) => Some(SnapshotKey::Folders),
        AccountEvent::MessagesUpdated { mailbox, .. } => Some(SnapshotKey::Mailbox(mailbox.clone())),
        AccountEvent::FlaggedMessages { .. } => Some(SnapshotKey::FlaggedMail),
        _ => None,
    })
}

pub(crate) fn collapse_calendar_events(events: Vec<CalendarSessionEvent>) -> Vec<CalendarSessionEvent> {
    collapse_last_wins(events, |event| match event {
        CalendarSessionEvent::CalendarsUpdated(_) => Some(SnapshotKey::Calendars),
        CalendarSessionEvent::OccurrencesUpdated { month, .. } => Some(SnapshotKey::Occurrences(*month)),
        CalendarSessionEvent::TasksUpdated(_) => Some(SnapshotKey::Tasks),
        _ => None,
    })
}

pub(crate) fn collapse_google_tasks_events(events: Vec<GoogleTasksEvent>) -> Vec<GoogleTasksEvent> {
    collapse_last_wins(events, |event| match event {
        GoogleTasksEvent::ListsUpdated(_) => Some(SnapshotKey::TaskLists),
        GoogleTasksEvent::TasksUpdated(_) => Some(SnapshotKey::Tasks),
        GoogleTasksEvent::Error(_) => None,
    })
}

pub(crate) fn collapse_subscription_events(events: Vec<SubscriptionSessionEvent>) -> Vec<SubscriptionSessionEvent> {
    collapse_last_wins(events, |event| match event {
        SubscriptionSessionEvent::SubscriptionsUpdated { month, .. } => Some(SnapshotKey::Subscriptions(*month)),
    })
}

/// Drives one connected account's `AccountEvent` stream for the rest of the
/// session, repainting whichever UI surface each event affects. Shared
/// between GOA accounts (`connect_account`) and manually-added accounts
/// (`connect_other_account`) - the event vocabulary and every reaction to it
/// is identical regardless of where the account's credentials come from.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_account_event_loop(
    evt_rx: async_channel::Receiver<AccountEvent>,
    state: Rc<RefCell<UiState>>,
    account_id: AccountId,
    folder_selection: gtk::SingleSelection,
    folder_scroller: gtk::ScrolledWindow,
    message_list: MessageListModel,
    message_list_stack: gtk::Stack,
    message_header: crate::message_header::MessageHeader,
    reading_stack: gtk::Stack,
    toast_overlay: adw::ToastOverlay,
    window: adw::ApplicationWindow,
    app: adw::Application,
    list_header: ListHeader,
    dashboard_refresh: Rc<dyn Fn()>,
    mark_read_button: gtk::Button,
    pin_button: gtk::Button,
    worker: Rc<Worker>,
) {
    glib::spawn_future_local(async move {
        while let Ok(event) = evt_rx.recv().await {
            // Drain the whole queued batch before handling, collapsing
            // whole-snapshot events (folders, message lists) into the last
            // copy of each - the startup burst queues the same envelope set
            // several times in a row (cache replay, live sync, previews), and
            // the UI must repaint once per batch, not once per queued copy.
            let mut batch = vec![event];
            while let Ok(next) = evt_rx.try_recv() {
                batch.push(next);
            }
            for event in collapse_account_events(batch) {
                match event {
                    AccountEvent::ConnectionStateChanged(ConnectionState::Error { message, retryable }) => {
                        // Retryable failures are warnings: the session reconnects
                        // itself with backoff, so they must not pop a toast on
                        // every attempt. Only non-retryable (fatal) errors surface.
                        if !retryable {
                            let title = glib::markup_escape_text(&format!("{}: {message}", account_label(&state, &account_id)));
                            toast_overlay.add_toast(adw::Toast::new(&title));
                        }
                        // A dropped connection while an uncached body was still
                        // loading is why that message never arrived: surface the
                        // unavailable page now instead of leaving the pane
                        // spinning until the fetch timeout fires.
                        let pending = state.borrow().pending_body_request.clone();
                        if let Some((mailbox, uid)) = pending.filter(|(m, _)| mailbox_account_id(m).as_ref() == Some(&account_id)) {
                            show_body_fetch_error(&state, &reading_stack, mailbox, uid);
                        }
                    }
                    AccountEvent::ConnectionStateChanged(_) => {}
                    AccountEvent::FoldersUpdated(folders) => {
                        {
                            let mut st = state.borrow_mut();
                            if let Some(handle) = st.accounts.get_mut(&account_id) {
                                handle.folders = folders;
                            }
                        }
                        // Fresh folder list means the account just (re)connected;
                        // any sync requests it left unanswered are dead - clear
                        // them so a later request for the same mailbox isn't
                        // wrongly suppressed by an entry that'll never resolve.
                        clear_account_syncing(&state, &account_id);
                        // This is also `request_account_refresh`'s own answer
                        // (a `Refresh` command relists before resyncing), and
                        // - like `syncing` above - a reconnect kills any
                        // `Refresh` this account had outstanding, so either
                        // way there's nothing left to dedupe against.
                        state.borrow_mut().refreshing.remove(&account_id);
                        // Fresh folder list also means a brand-new session: re-send
                        // the stored prefetch policy (Config → Advanced →
                        // "Aggressive prefetch") so a reconnect never leaves the
                        // session on the cooperative default when the user enabled
                        // aggressive prefetch.
                        {
                            let policy = state.borrow().settings.prefetch_policy();
                            if let Some(handle) = state.borrow().accounts.get(&account_id) {
                                let _ = handle.cmd_tx.send_blocking(lookout_mail::session::AccountCommand::SetPrefetchPolicy(policy));
                            }
                        }
                        rebuild_folder_tree(&state, &folder_selection, &folder_scroller);
                        // Folder names and account labels only exist once this
                        // event lands, so a view restored before it (or adopted by
                        // the race below) gets its header filled in here.
                        refresh_list_header(&state, &list_header);
                        // The dashboard's mail stats follow the folder syncs.
                        dashboard_refresh();
                        // A reconnect can clear the open mailbox's `syncing` entry
                        // (its old request is dead) without ever answering it -
                        // don't leave the spinner running for a request that will
                        // never land.
                        refresh_message_loading_state(&state, &message_list, &message_list_stack);
                        // A reconnect also means a body fetch that failed while
                        // the connection was down can be retried: re-arm it
                        // automatically so the message loads once connectivity
                        // is back, without the user having to press Retry.
                        let failed = state.borrow().body_fetch_error.clone();
                        if let Some((mailbox, uid)) = failed.filter(|(m, _)| mailbox_account_id(m).as_ref() == Some(&account_id)) {
                            request_body_fetch(&state, &worker, &reading_stack, &message_header, mailbox, uid);
                        }
                    }
                    AccountEvent::MessagesUpdated { mailbox, mut messages } => {
                        // This is the authoritative live-sync landing. Bump the
                        // mailbox's epoch first so any off-thread cache read
                        // still in flight for it (dispatched before this sync
                        // returned, so it holds a stale window) is discarded
                        // rather than repainting the list over these messages.
                        {
                            let mut st = state.borrow_mut();
                            *st.mailbox_sync_epoch.entry(mailbox.clone()).or_insert(0) += 1;
                        }
                        // A sync can land for reasons unrelated to a pending
                        // optimistic removal/flag-toggle (see the IDLE-vs-
                        // command race in session.rs) and carry stale state
                        // for a uid this client already acted on locally -
                        // reconcile against the stash instead of trusting
                        // every sync as automatically authoritative.
                        reconcile_optimistic_removals(&state, &mailbox, &mut messages);
                        reconcile_optimistic_flag_changes(&state, &message_list, &mailbox, &mut messages);
                        reconcile_optimistic_pinned_changes(&state, &message_list, &mailbox, &mut messages);
                        // The sync this mailbox was asked for (if any) has landed.
                        set_mailbox_syncing(&state, &mailbox, false);
                        // A sync means the envelope cache this dashboard reads
                        // changed - repaint its mail sections. (The calendar
                        // sections repaint via their own event loops.)
                        dashboard_refresh();
                        // While a search is active the results list owns the
                        // pane: a background sync repopulating it would clobber
                        // the results with the folder's full set. Still fold
                        // inbox syncs into the unified snapshot so exiting search
                        // restores fresh data.
                        if state.borrow().search_active {
                            let mut st = state.borrow_mut();
                            let in_unified_set = st
                                .accounts
                                .values()
                                .any(|h| h.folders.iter().any(|m| m.id == mailbox && matches!(m.role, MailboxRole::Inbox)));
                            if in_unified_set {
                                st.unified_snapshots.insert(mailbox, messages);
                            }
                            continue;
                        }
                        // Decide whether this mailbox belongs to the view on
                        // screen, folding its payload into the unified snapshot
                        // when in "All Inboxes" mode. On fresh startup (nothing
                        // selected yet) the first inbox sync is still adopted as
                        // the default single-mailbox view, matching the old
                        // race-first behavior.
                        let (display, single_messages, adopted, unified_slice) = {
                            let mut st = state.borrow_mut();
                            if matches!(st.mail_view, MailView::UnifiedInbox) {
                                // Only accept mailboxes that are actually part
                                // of the unified set (each account's Inbox) - a
                                // stale resync from a mailbox the user last had
                                // open in single-view must not leak into the
                                // merged list.
                                let in_unified_set = st
                                    .accounts
                                    .values()
                                    .any(|h| h.folders.iter().any(|m| m.id == mailbox && matches!(m.role, MailboxRole::Inbox)));
                                // `unified_snapshots` and `message_list`'s own
                                // merged `truth` are independently maintained
                                // (the former is what the 3 full-rebuild call
                                // sites - disconnect, exit_search,
                                // enter_unified_inbox - read from), so both need
                                // this account's new snapshot; the clones here
                                // are O(this account's messages), not O(every
                                // account's).
                                let unified_slice = if in_unified_set {
                                    st.unified_snapshots.insert(mailbox.clone(), messages.clone());
                                    Some(messages)
                                } else {
                                    None
                                };
                                (in_unified_set, None, false, unified_slice)
                            } else {
                                // Nothing selected yet: adopt whichever account's
                                // initial inbox sync lands first as the default
                                // view, rather than leaving the message list empty
                                // until the user clicks a folder. Unless a
                                // remembered view is still pending restore - that
                                // takes priority over the race, so a slow account
                                // can't steal the pane away from the user's last
                                // folder. Whichever connected account happens to
                                // finish its first sync first wins this race - an
                                // acceptable, benign nondeterminism for Phase 1.
                                let adopted = st.current_mailbox.is_none() && !st.restore_pending;
                                if adopted {
                                    st.current_account = Some(account_id.clone());
                                    st.current_mailbox = Some(mailbox.clone());
                                }
                                let is_current = st.current_mailbox.as_ref() == Some(&mailbox);
                                (is_current, is_current.then_some(messages), adopted, None)
                            }
                        };
                        // The adopt-first path picked a default view; name it in
                        // the list header.
                        if adopted {
                            refresh_list_header(&state, &list_header);
                        }
                        if display {
                            let (key, descending) = current_sort(&state);
                            match single_messages {
                                Some(messages) => message_list.repopulate(messages, key, descending),
                                None => {
                                    // `display` in the unified branch is exactly
                                    // `in_unified_set`, which is also what gates
                                    // `unified_slice` being `Some` above - so
                                    // this arm (single-view's `single_messages`
                                    // is always `Some`) only runs when it's set.
                                    let slice = unified_slice.expect("display && single_messages.is_none() implies unified branch set unified_slice");
                                    message_list.repopulate_unified_slice(&mailbox, slice, key, descending);
                                }
                            }
                        }
                        // Re-check now that `repopulate`/`repopulate_unified_slice`
                        // (if this mailbox was the one on screen) has actually
                        // landed - checking right after `set_mailbox_syncing`
                        // above would still see the pre-sync (often empty) list
                        // and briefly flash the "empty" state before the real
                        // content arrives.
                        refresh_message_loading_state(&state, &message_list, &message_list_stack);
                    }
                    // The shared `sync_mailbox` fires this at the start of
                    // every full envelope re-sync it does, for any trigger -
                    // an explicit request is only one of them (IDLE pushes,
                    // moves, flag/keyword changes, snoozes, drafts, sends,
                    // and `Refresh` all resync the open mailbox too). Folded
                    // straight into `syncing` rather than a separate set
                    // (unlike `prefetching` below): this is exactly the same
                    // kind of activity `request_mailbox_sync` already tracks
                    // there, just from more triggers, so deduping a fresh
                    // explicit request against one already in flight for any
                    // reason is correct - the in-flight round trip answers
                    // both. The matching "finished" signal is the mailbox's
                    // next `MessagesUpdated`, handled above.
                    AccountEvent::MailboxSyncStarted { mailbox } => {
                        set_mailbox_syncing(&state, &mailbox, true);
                    }
                    // The background body prefetch is otherwise silent (it
                    // only warms the body cache, never touches the message
                    // list), so `syncing`'s folder-pane spinner is its only
                    // UI surface - `prefetching` is a separate set from
                    // `syncing` rather than folding into it, so a mailbox
                    // mid-prefetch can never suppress a real
                    // `request_mailbox_sync` dedup check for the same
                    // mailbox (see `set_mailbox_prefetching`).
                    AccountEvent::PrefetchStarted { mailbox } => {
                        set_mailbox_prefetching(&state, &mailbox, true);
                    }
                    AccountEvent::PrefetchFinished { mailbox } => {
                        set_mailbox_prefetching(&state, &mailbox, false);
                    }
                    AccountEvent::PreviewsFetched { mailbox, previews, mentions } => {
                        // Keep the unified-view snapshot current regardless of
                        // what's on screen - it's what the 3 full-rebuild call
                        // sites (disconnect, exit_search, enter_unified_inbox)
                        // read from.
                        if let Some(snapshot) = state.borrow_mut().unified_snapshots.get_mut(&mailbox) {
                            for m in snapshot.iter_mut() {
                                if let Some(p) = previews.get(&m.uid) {
                                    m.preview = Some(p.clone());
                                }
                                if let Some(mentions) = mentions.get(&m.uid) {
                                    m.mentions = *mentions;
                                }
                            }
                        }
                        // Mirrors MessagesUpdated's search-active early-out:
                        // the results list owns the pane during a search.
                        if state.borrow().search_active {
                            continue;
                        }
                        let display = {
                            let st = state.borrow();
                            match st.mail_view {
                                MailView::UnifiedInbox => st
                                    .accounts
                                    .values()
                                    .any(|h| h.folders.iter().any(|m| m.id == mailbox && matches!(m.role, MailboxRole::Inbox))),
                                _ => st.current_mailbox.as_ref() == Some(&mailbox),
                            }
                        };
                        if display {
                            patch_previews(&state, &message_list, &mailbox, &previews, &mentions);
                        }
                    }
                    AccountEvent::NewMessages { mailbox, messages } => {
                        // Desktop notification for genuinely-new unread mail
                        // (`session.rs` already excludes a mailbox's first-ever
                        // sync). Gated by the settings toggle and the mailbox's
                        // role (only Inbox/Custom are worth notifying about - see
                        // `should_notify_role`). Fires even if the window is
                        // already focused on this exact mailbox - the message
                        // just arrived and the list may not have repainted yet,
                        // so the notification is still useful.
                        if state.borrow().settings.get_bool(crate::settings::MAIL_NOTIFICATIONS_ENABLED) {
                            let mailbox_info = state.borrow().accounts.get(&account_id).and_then(|h| h.folders.iter().find(|f| f.id == mailbox).cloned());
                            if let Some(mailbox_info) = mailbox_info {
                                if crate::mail_notifications::should_notify_role(mailbox_info.role) {
                                    crate::mail_notifications::show_new_mail_notification(&app, &mailbox_info.name, &mailbox, &messages);
                                }
                            }
                        }
                    }
                    AccountEvent::NewMailArrived { mailbox, messages } => {
                        // Client rules (item 2.5 / M-14) run on genuinely-new
                        // arriving Inbox mail. See `rules::run_client_rules`.
                        crate::rules::run_client_rules(&state, &account_id, &mailbox, &messages);
                        // Blocked senders (item 2.9 / M-17) always apply, even
                        // when the rules master switch is off.
                        crate::rules::enforce_blocked_senders(&state, &account_id, &mailbox, &messages);
                    }
                    AccountEvent::SieveAdvertised(advertised) => {
                        // Remember whether this account's server offers
                        // ManageSieve, so the rules UI can offer the Sieve
                        // backend (item 2.5).
                        if let Some(handle) = state.borrow_mut().accounts.get_mut(&account_id) {
                            handle.sieve_advertised = advertised;
                        }
                        if advertised {
                            // Push any Sieve rules now that the server is
                            // reachable, and ask which vacation extensions the
                            // automatic-replies settings can rely on (item 2.11).
                            push_sieve_script(&state);
                            if let Some(handle) = state.borrow().accounts.get(&account_id) {
                                let _ = handle.cmd_tx.send_blocking(AccountCommand::FetchSieveCapabilities);
                            }
                            // A Sieve account's automatic replies read from
                            // `settings.json`, so the banner can settle now.
                            refresh_automatic_replies_ui(&state);
                        }
                    }
                    AccountEvent::SieveScriptUpdated => {
                        toast_overlay.add_toast(adw::Toast::new("Server script updated"));
                    }
                    AccountEvent::SieveFailed { message } => {
                        toast_overlay.add_toast(adw::Toast::new(&glib::markup_escape_text(&format!("Couldn't update the server script: {message}"))));
                    }
                    AccountEvent::SieveCapabilities {
                        reachable,
                        vacation,
                        date,
                        relational,
                    } => {
                        // What the account's ManageSieve server can host for
                        // automatic replies (item 2.11); the settings dialog
                        // reads this to decide what to offer.
                        state.borrow_mut().sieve_support.insert(
                            account_id.clone(),
                            SieveVacationSupport {
                                reachable,
                                vacation,
                                date,
                                relational,
                            },
                        );
                        // An open Automatic replies dialog is waiting on the
                        // answer: repaint it in place.
                        let hook = state.borrow().automatic_replies_dialog_refresh.clone();
                        if let Some(hook) = hook {
                            hook();
                        }
                    }
                    AccountEvent::BodyFetched { mailbox, uid, body } => {
                        // A rules-engine evaluation (item 2.5 / M-14, A5b)
                        // waiting on this body now has what it needs.
                        crate::rules::evaluate_deferred_rules(&state, &account_id, &mailbox, uid, &body);
                        let should_render = {
                            let mut st = state.borrow_mut();
                            // Render if this is the request still in flight, or
                            // if it's the message the error page is currently
                            // showing for - a slow fetch that lands after the
                            // timeout has already raised the error page should
                            // still replace it with the real message.
                            let is_current =
                                body_request_matches(&mailbox, &uid, st.pending_body_request.as_ref()) || st.body_fetch_error.as_ref() == Some(&(mailbox.clone(), uid));
                            tracing::debug!(
                                ?mailbox,
                                uid = uid.0,
                                is_current,
                                pending = ?st.pending_body_request,
                                "FetchBody: BodyFetched event received"
                            );
                            // Cache the body regardless of whether the user is
                            // still looking at this message - a fetch that
                            // completed after they moved on is still worth
                            // keeping for when they come back.
                            st.body_cache.insert(mailbox.clone(), uid, body.clone());
                            is_current
                        };
                        // A message-menu action waiting for this body (item
                        // 1.17) runs once the borrow above is dropped.
                        let waiter = {
                            let mut st = state.borrow_mut();
                            match &st.body_waiter {
                                Some(waiter) if waiter.mailbox == mailbox && waiter.uid == uid => st.body_waiter.take(),
                                _ => None,
                            }
                        };
                        let waiting_body = waiter.as_ref().map(|_| body.clone());
                        // A conversation card waiting on this body (item 1.7).
                        conversation_body_arrived(&state, &mailbox, uid, body.clone());
                        if should_render {
                            tracing::debug!(?mailbox, uid = uid.0, "FetchBody: body arrived on UI thread");
                            render_body(&state, &reading_stack, &message_header, mailbox, uid, body);
                        }
                        if let (Some(waiter), Some(body)) = (waiter, waiting_body) {
                            (waiter.run)(body);
                        }
                    }
                    AccountEvent::PartFetched { mailbox, uid, part, bytes } => {
                        // An inline `cid:` image request for this part (separate
                        // from the strip's one-at-a-time row actions): finish the
                        // WebKit request with the bytes. Keyed by part number and
                        // dropped on every message change, so a hit is
                        // authoritative - a stale part for the same number can't
                        // linger because the map only ever holds the current
                        // message's requests. Runs first so the strip closures
                        // below can move `part`/`bytes` into their futures.
                        let cid_requests = state.borrow_mut().pending_cid.remove(&(mailbox.clone(), uid, part.part_number.clone()));
                        for cid_request in cid_requests.into_iter().flatten() {
                            tracing::debug!(?mailbox, uid = uid.0, part = %part.part_number, bytes = bytes.len(), "cid: image bytes arrived; finishing WebKit request");
                            let stream = gio::MemoryInputStream::from_bytes(&glib::Bytes::from(&bytes));
                            cid_request.finish(&stream, bytes.len() as i64, Some(&part.content_type));
                        }
                        // An attachment part's bytes arrived. Only act if they
                        // belong to the message currently on the reading pane and
                        // to the outstanding row action - a response that lands
                        // after the user moved on (or for a different part of the
                        // same message) is stale and dropped with its in-flight
                        // bookkeeping.
                        // A "Save all" / "Download as zip" run (item 1.23) may
                        // be collecting this part instead of a single-row
                        // action; it takes precedence and consumes the bytes
                        // when it does.
                        if !advance_attachment_batch(&state, &worker, &reading_stack, &mailbox, uid, &part, &bytes) {
                            let pending = {
                                let mut st = state.borrow_mut();
                                let on_screen = message_is_visible(&st, &mailbox, uid);
                                if !on_screen {
                                    st.pending_attachment = None;
                                    None
                                } else {
                                    st.pending_attachment.take()
                                }
                            };
                            if let Some(p) = pending {
                                if p.mailbox == mailbox && p.uid == uid && p.part_number == part.part_number {
                                    p.button.set_sensitive(true);
                                    match p.action {
                                        PendingAttachmentAction::Open => {
                                            let window_for_open = window.clone();
                                            let state_for_open = state.clone();
                                            let toast_for_open = toast_overlay.clone();
                                            glib::spawn_future_local(async move {
                                                open_attachment_temp(&window_for_open, &state_for_open, toast_for_open, &part, &bytes).await;
                                            });
                                        }
                                        PendingAttachmentAction::OpenWith => {
                                            let state_for_open_with = state.clone();
                                            let toast_for_open_with = toast_overlay.clone();
                                            glib::spawn_future_local(async move {
                                                open_attachment_with(&state_for_open_with, toast_for_open_with, &part, &bytes).await;
                                            });
                                        }
                                        PendingAttachmentAction::Save => {
                                            let window_for_save = window.clone();
                                            let toast_for_save = toast_overlay.clone();
                                            glib::spawn_future_local(async move {
                                                save_attachment_to_disk(&window_for_save, toast_for_save, &part, &bytes).await;
                                            });
                                        }
                                    }
                                } else {
                                    // Mismatched request (shouldn't happen with the
                                    // one-at-a-time guard, but be safe).
                                    state.borrow_mut().pending_attachment = Some(p);
                                }
                            }
                        }
                    }
                    AccountEvent::PartFetchFailed {
                        mailbox,
                        uid,
                        part_number,
                        message,
                    } => {
                        // The session couldn't produce this attachment's bytes.
                        // Restore the row's button if this is still the outstanding
                        // request and tell the user what went wrong - never leave
                        // it stuck.
                        let pending = {
                            let mut st = state.borrow_mut();
                            let on_screen = message_is_visible(&st, &mailbox, uid);
                            if !on_screen {
                                st.pending_attachment = None;
                                None
                            } else {
                                st.pending_attachment.take()
                            }
                        };
                        if let Some(p) = pending {
                            if p.mailbox == mailbox && p.uid == uid && p.part_number == part_number {
                                p.button.set_sensitive(true);
                                toast_overlay.add_toast(adw::Toast::new(&glib::markup_escape_text(&message)));
                            } else {
                                state.borrow_mut().pending_attachment = Some(p);
                            }
                        }
                        // A "Save all" / "Download as zip" run (item 1.23)
                        // waiting on this part gives up as a whole; a forward
                        // (item 1.11 / C-13) still opens its composer with the
                        // parts that did arrive, minus this failed one.
                        let failed_batch = {
                            let mut st = state.borrow_mut();
                            let failed = st
                                .pending_attachment_batch
                                .as_ref()
                                .is_some_and(|b| b.mailbox == mailbox && b.uid == uid && b.current == part_number);
                            if failed {
                                st.pending_attachment_batch.take()
                            } else {
                                None
                            }
                        };
                        if let Some(batch) = failed_batch {
                            let AttachmentBatch { action, collected, .. } = batch;
                            match action {
                                AttachmentBatchAction::Forward(fb) => {
                                    toast_overlay.add_toast(adw::Toast::new(&glib::markup_escape_text(&format!(
                                        "Couldn't attach one of the original's files: {message}"
                                    ))));
                                    open_forward_composer(&state, &worker, &reading_stack, *fb, collected);
                                }
                                _ => {
                                    toast_overlay.add_toast(adw::Toast::new(&glib::markup_escape_text(&format!("Couldn't save attachments: {message}"))));
                                }
                            }
                        }
                        // The same failure for an inline `cid:` image request:
                        // finish the WebKit request with an error so the browser
                        // draws a broken image rather than waiting forever.
                        let cid_requests = state.borrow_mut().pending_cid.remove(&(mailbox.clone(), uid, part_number.clone()));
                        for cid_request in cid_requests.into_iter().flatten() {
                            tracing::warn!(?mailbox, uid = uid.0, part = %part_number, "cid: inline image fetch failed: {message}");
                            finish_cid_request_error(&cid_request, "the inline image could not be fetched");
                        }
                    }
                    AccountEvent::RawMessageFetched { mailbox, uid, bytes } => {
                        // A "message in new window" pop-out (item 1.9) waiting
                        // on these bytes renders them; independent of the
                        // export below, since both can be in flight for the
                        // same message at once.
                        crate::message_window::deliver_raw(&state, &mailbox, uid, &bytes);
                        // The "Message details" dialog (item 1.10), if one is
                        // waiting for this exact message's source.
                        deliver_raw_to_details_dialog(&state, &mailbox, uid, &bytes);
                        // A draft "Edit" action waiting on these bytes (item
                        // 1.12) opens the composer prefilled from the parsed
                        // draft.
                        let pending_draft = {
                            let mut st = state.borrow_mut();
                            match st.pending_draft_open.take() {
                                Some(pending) if pending.mailbox == mailbox && pending.uid == uid => Some(pending),
                                other => {
                                    st.pending_draft_open = other;
                                    None
                                }
                            }
                        };
                        if let Some(pending) = pending_draft {
                            open_draft_composer_with_raw(&state, &worker, &reading_stack, &pending.summary, &bytes);
                        }
                        // A Report phishing run (item 6.7 / S-09) waiting on
                        // these bytes: forward the message as an attachment to
                        // the configured address, then move it to Junk. Done
                        // before the export branch below, which moves `bytes`
                        // into its async task.
                        let pending_report = {
                            let mut st = state.borrow_mut();
                            match st.pending_report_phishing.take() {
                                Some((m, u)) if m == mailbox && u == uid => true,
                                other => {
                                    st.pending_report_phishing = other;
                                    false
                                }
                            }
                        };
                        if pending_report {
                            let summary = message_list.all_messages().into_iter().find(|m| m.mailbox == mailbox && m.uid == uid);
                            if let Some(summary) = summary {
                                match report_phishing_message(&state, &summary, &bytes) {
                                    Some(message) => {
                                        let account_id = mailbox_account_id(&summary.mailbox);
                                        let sent = account_id
                                            .and_then(|id| state.borrow().accounts.get(&id).map(|handle| handle.cmd_tx.clone()))
                                            .map(|cmd_tx| {
                                                cmd_tx
                                                    .send_blocking(AccountCommand::SendMessage {
                                                        message: Box::new(message),
                                                        not_before: None,
                                                        client_token: None,
                                                    })
                                                    .is_ok()
                                            })
                                            .unwrap_or(false);
                                        if sent {
                                            set_junk_selection(&state, &message_list, std::slice::from_ref(&summary), true);
                                            toast_overlay.add_toast(adw::Toast::new("Message reported as phishing"));
                                        } else {
                                            toast_overlay.add_toast(adw::Toast::new("Couldn't report the message: the account isn't connected"));
                                        }
                                    }
                                    None => toast_overlay.add_toast(adw::Toast::new("Set a Report phishing address in Settings first")),
                                }
                            }
                        }
                        // A rule-initiated Forward/Redirect (item 2.5 / M-14)
                        // waiting on this message's raw bytes sends it on now.
                        crate::rules::deliver_rule_forwards(&state, &mailbox, uid, &bytes);
                        // A "Forward as attachment" run (item 1.11 / C-13)
                        // waiting on these bytes opens its composer with the
                        // whole message attached.
                        let pending_forward = {
                            let mut st = state.borrow_mut();
                            match st.pending_forward_raw.take() {
                                Some(p) if p.mailbox == mailbox && p.uid == uid => Some(p),
                                other => {
                                    st.pending_forward_raw = other;
                                    None
                                }
                            }
                        };
                        if let Some(pending) = pending_forward {
                            let prefill = crate::compose::build_forward_attachment_prefill(&pending.summary, &bytes);
                            let rich_text_default = state.borrow().rich_text_default;
                            show_composer_in_reading_pane(
                                &state,
                                &worker,
                                &reading_stack,
                                "Forward",
                                pending.cmd_tx,
                                prefill,
                                rich_text_default,
                                pending.account_id,
                                Some(crate::config_view::SignatureDefault::Reply),
                            );
                        }
                        // The .eml export's whole raw message arrived. Only act if
                        // it belongs to the outstanding export request; a response
                        // that lands after the user selected a different message
                        // (or triggered a newer export) is stale and dropped with
                        // its bookkeeping.
                        let pending = {
                            let mut st = state.borrow_mut();
                            match st.pending_raw_message.take() {
                                Some(p) if p.mailbox == mailbox && p.uid == uid => Some(p),
                                other => {
                                    st.pending_raw_message = other;
                                    None
                                }
                            }
                        };
                        if let Some(pending) = pending {
                            let window_for_save = window.clone();
                            let toast_for_save = toast_overlay.clone();
                            glib::spawn_future_local(async move {
                                save_raw_message_to_disk(&window_for_save, toast_for_save, &pending.initial_name, &bytes).await;
                            });
                        }
                    }
                    AccountEvent::RawMessageFetchFailed { mailbox, uid, message } => {
                        // Tell a pop-out window (item 1.9) or the "Message
                        // details" dialog (item 1.10) waiting on this fetch
                        // what went wrong, so neither is left on its spinner.
                        crate::message_window::deliver_error(&state, &mailbox, uid, &message);
                        deliver_raw_error_to_details_dialog(&state, &mailbox, uid, &message);
                        // A Report phishing run (item 6.7 / S-09) waiting on
                        // this fetch fails as a whole.
                        let report_failed = {
                            let mut st = state.borrow_mut();
                            match st.pending_report_phishing.take() {
                                Some((m, u)) if m == mailbox && u == uid => true,
                                other => {
                                    st.pending_report_phishing = other;
                                    false
                                }
                            }
                        };
                        if report_failed {
                            toast_overlay.add_toast(adw::Toast::new(&glib::markup_escape_text(&format!("Couldn't report the message: {message}"))));
                        }
                        // A rule-initiated Forward/Redirect waiting on the same
                        // fetch fails with it.
                        crate::rules::fail_rule_forwards(&state, &mailbox, uid, &message);
                        // A "Forward as attachment" run (item 1.11 / C-13)
                        // waiting on the same fetch fails as a whole.
                        let forward_failed = {
                            let mut st = state.borrow_mut();
                            match st.pending_forward_raw.take() {
                                Some(pending) if pending.mailbox == mailbox && pending.uid == uid => true,
                                other => {
                                    st.pending_forward_raw = other;
                                    false
                                }
                            }
                        };
                        if forward_failed {
                            toast_overlay.add_toast(adw::Toast::new(&glib::markup_escape_text(&format!("Couldn't forward the message: {message}"))));
                        }
                        // A draft "Edit" action (item 1.12) waiting on the
                        // same fetch: drop back to the empty pane and say why.
                        let draft_pending = {
                            let mut st = state.borrow_mut();
                            match st.pending_draft_open.take() {
                                Some(pending) if pending.mailbox == mailbox && pending.uid == uid => true,
                                other => {
                                    st.pending_draft_open = other;
                                    false
                                }
                            }
                        };
                        if draft_pending {
                            reading_stack.set_visible_child_name("empty");
                            toast_overlay.add_toast(adw::Toast::new(&glib::markup_escape_text(&format!("Couldn't open the draft: {message}"))));
                        }
                        // The session couldn't produce the raw message. Clear the
                        // pending export (so a later click can retry) and tell the
                        // user what went wrong - never leave it stuck.
                        let pending = {
                            let mut st = state.borrow_mut();
                            match st.pending_raw_message.take() {
                                Some(p) if p.mailbox == mailbox && p.uid == uid => Some(p),
                                other => {
                                    st.pending_raw_message = other;
                                    None
                                }
                            }
                        };
                        if pending.is_some() {
                            toast_overlay.add_toast(adw::Toast::new(&glib::markup_escape_text(&message)));
                        }
                    }
                    // The message was persisted to the local outbox (item 0.3).
                    // The "Sending: …" toast was already shown when Send was
                    // clicked; bind it to the outbox row so its Undo can
                    // cancel it and `OutboxSent`/`OutboxFailed` retract it. A
                    // scheduled send (item 1.14) will also use this to show a
                    // "Scheduled for …" bar.
                    AccountEvent::OutboxQueued { id, client_token, .. } => {
                        if let Some(client_token) = client_token {
                            bind_pending_send(&state, &account_id, client_token, id);
                        }
                    }
                    // Only a composer send gets "Message sent": read receipts
                    // and invitation replies announce themselves when sent.
                    AccountEvent::OutboxSent { id } => {
                        if take_pending_send(&state, &account_id, id, None).is_some() {
                            toast_overlay.add_toast(adw::Toast::new("Message sent"));
                        }
                    }
                    // Undo send (item 1.13) or schedule send's "Cancel send" /
                    // "Edit" (item 1.14) withdrew the message: reopen it -
                    // from the in-memory copy when this run started the send,
                    // else from the outbox payload the event carries.
                    AccountEvent::OutboxCancelled { id, message } => {
                        if let Some(pending) = take_pending_send(&state, &account_id, id, None) {
                            toast_overlay.add_toast(adw::Toast::new("Sending cancelled"));
                            reopen_undone_send(&state, &worker, &reading_stack, &account_id, &pending.message);
                        } else if let Some(prefill) = message.as_deref().and_then(crate::compose::build_outbox_prefill) {
                            toast_overlay.add_toast(adw::Toast::new("Sending cancelled"));
                            reopen_withdrawn_send(&state, &worker, &reading_stack, &account_id, prefill);
                        }
                    }
                    // Undo came too late - the message had already gone out
                    // (its `OutboxSent` arrived first and said so).
                    AccountEvent::OutboxCancelFailed { .. } => {
                        toast_overlay.add_toast(adw::Toast::new("Too late to undo: the message was already sent"));
                    }
                    AccountEvent::OutboxFailed { id, client_token, message } => {
                        take_pending_send(&state, &account_id, id, client_token);
                        toast_overlay.add_toast(adw::Toast::new(&glib::markup_escape_text(&message)));
                        // The toast above is enough while the window is focused;
                        // a desktop notification is for when the user isn't
                        // looking, so a failed send doesn't go unnoticed.
                        if !window.is_active() && state.borrow().settings.get_bool(crate::settings::MAIL_NOTIFICATIONS_ENABLED) {
                            crate::mail_notifications::show_send_failed_notification(&app, &account_id, &message);
                        }
                    }
                    // The outbox changed (item 1.14): repaint the scheduled-
                    // messages banner and dialog, and a scheduled draft's bar.
                    AccountEvent::OutboxContents { items } => {
                        state.borrow_mut().outbox.insert(account_id.clone(), items);
                        refresh_outbox_ui(&state);
                        let is_rendered_draft = state.borrow().rendered_draft.is_some();
                        refresh_draft_banner(&state, &reading_stack, is_rendered_draft);
                    }
                    AccountEvent::DraftSaved { message_id } => {
                        // Relay the confirmation to whichever composer is open;
                        // it decides whether the id is its own.
                        if let Some(tx) = state.borrow().draft_saved_tx.clone() {
                            let _ = tx.try_send(message_id);
                        }
                    }
                    // Every move path already showed its "… / Undo" toast
                    // the moment it was clicked (item 1.13); a second
                    // "Deleted"/"Moved" toast once the server confirms would
                    // only queue up behind it and bury the Undo.
                    AccountEvent::MessageMoved { .. } => {}
                    AccountEvent::MoveFailed { mailbox, uids, role, message } => {
                        // The move actually failed server-side, so put back
                        // exactly the rows `optimistic_remove_messages` hid for
                        // this attempt - the row must not stay gone for a move
                        // that never happened.
                        restore_optimistic_removals(&state, &message_list, &mailbox, &uids);
                        let verb = match role {
                            MailboxRole::Trash => "delete",
                            MailboxRole::Archive => "archive",
                            MailboxRole::Junk => "report as junk",
                            _ => "move",
                        };
                        let title = glib::markup_escape_text(&format!("Couldn't {verb}: {message}"));
                        toast_overlay.add_toast(adw::Toast::new(&title));
                    }
                    AccountEvent::StoreFlagsFailed { mailbox, uids, message } => {
                        // The flag change actually failed server-side, so put
                        // back exactly the summaries `optimistic_toggle_read`/
                        // `optimistic_toggle_pinned` patched for this attempt -
                        // each restore is a no-op if this failure wasn't from
                        // that particular toggle to begin with.
                        restore_optimistic_flag_changes(&state, &message_list, &mailbox, &uids);
                        restore_optimistic_pinned_changes(&state, &message_list, &mailbox, &uids);
                        refresh_mark_read_button(&mark_read_button, &message_list);
                        // Reverts the click handler's optimistic icon flip back
                        // to whatever the (now-restored, so correct again)
                        // selected summaries actually say.
                        refresh_pin_button(&pin_button, &message_list);
                        let title = glib::markup_escape_text(&format!("Couldn't update message flags: {message}"));
                        toast_overlay.add_toast(adw::Toast::new(&title));
                    }
                    AccountEvent::MailboxExpunged { role } => {
                        let label = match role {
                            MailboxRole::Trash => "Trash emptied",
                            MailboxRole::Junk => "Junk emptied",
                            _ => "Folder emptied",
                        };
                        toast_overlay.add_toast(adw::Toast::new(label));
                    }
                    // The snooze button's Undo toast already confirmed it.
                    AccountEvent::MessageSnoozed => {}
                    // The session re-emits the folder from its cache after a
                    // failed `SetFollowUp`, which undoes the optimistic flag.
                    AccountEvent::FollowUpFailed { message, .. } => {
                        toast_overlay.add_toast(adw::Toast::new(&glib::markup_escape_text(&message)));
                    }
                    // A flag change was stored: carry it to Outlook on
                    // Microsoft 365 (item 1.16), whether an action or its
                    // undo sent it.
                    AccountEvent::FollowUpSet { message_ids, flag } => {
                        mirror_follow_up_to_graph(&state, &account_id, message_ids, flag);
                    }
                    AccountEvent::FlaggedMessages { messages } => {
                        state.borrow_mut().flagged_mail.insert(account_id.clone(), messages);
                        refresh_flagged_mail(&state);
                        refresh_follow_up_banner(&state, &message_list, &reading_stack);
                    }
                    AccountEvent::MailboxCreated { mailbox } => {
                        // Open the new folder's parent, so it's visible once the
                        // re-list that follows lands in `FoldersUpdated`.
                        let delimiter = state
                            .borrow()
                            .accounts
                            .get(&account_id)
                            .and_then(|handle| handle.folders.first().map(|m| m.delimiter))
                            .unwrap_or('/');
                        if let Some(parent) = lookout_mail::mailbox_name::parent_mailbox_id(&account_id, &mailbox, delimiter) {
                            // Both halves: the rebuild's capture overrides the
                            // memory for a parent that's expandable-but-collapsed
                            // now, so open its live row too; a parent that had no
                            // subfolders yet only exists in the memory.
                            if let Some(model) = folder_selection.model().and_downcast::<gtk::TreeListModel>() {
                                if let Some(row) = find_mailbox_index(&model, &parent).and_then(|index| model.item(index)).and_downcast::<gtk::TreeListRow>() {
                                    row.set_expanded(true);
                                }
                            }
                            state.borrow_mut().expanded_folders.insert(parent);
                        }
                        let name = lookout_mail::mailbox_name::display_leaf(&account_id, &mailbox, delimiter);
                        toast_overlay.add_toast(adw::Toast::new(&format!("Created \u{201c}{name}\u{201d}")));
                    }
                    AccountEvent::MailboxRenamed { from, to, delimiter, to_trash } => {
                        rekey_renamed_folder(&state, &from, &to, delimiter);
                        if to_trash {
                            let name = lookout_mail::mailbox_name::display_leaf(&account_id, &from, delimiter);
                            toast_overlay.add_toast(adw::Toast::new(&format!("Moved \u{201c}{name}\u{201d} to Trash")));
                        }
                        // The header names the open folder, which may be the
                        // renamed one; the `FoldersUpdated` that follows
                        // redraws the tree under the new names.
                        refresh_list_header(&state, &list_header);
                    }
                    AccountEvent::MailboxDeleted { mailbox, delimiter } => {
                        forget_deleted_folder(&state, &mailbox, delimiter);
                        let name = lookout_mail::mailbox_name::display_leaf(&account_id, &mailbox, delimiter);
                        toast_overlay.add_toast(adw::Toast::new(&format!("Deleted \u{201c}{name}\u{201d}")));
                        refresh_list_header(&state, &list_header);
                    }
                    AccountEvent::SearchResults { mailbox, query, messages } => {
                        // An answer to the live IMAP pass. `mailbox` matches a
                        // `search_pending` entry (the session always answers, even
                        // with an empty match set); a result whose folder is no
                        // longer pending - or whose query isn't the one on screen,
                        // for the same-folder re-search race where an old answer
                        // lands after the folder is pending again - belongs to a
                        // stale search and is dropped.
                        let account_id = mailbox_account_id(&mailbox);
                        let pending_key = account_id.as_ref().map(|id| (id.clone(), mailbox.clone()));
                        let wanted = pending_key.as_ref().is_some_and(|key| {
                            let mut st = state.borrow_mut();
                            st.search_pending.remove(key)
                        });
                        if !wanted || state.borrow().search_query != query {
                            continue;
                        }
                        // Merge the answer into the accumulated results - a
                        // search can also have surfaced this message in the cache
                        // pass, so dedupe by `(mailbox, uid)`.
                        let mut seen: HashSet<(MailboxId, Uid)> = state.borrow().search_results.iter().map(|m| (m.mailbox.clone(), m.uid)).collect();
                        let mut st = state.borrow_mut();
                        for m in messages {
                            if seen.insert((m.mailbox.clone(), m.uid)) {
                                st.search_results.push(m);
                            }
                        }
                        drop(st);
                        repopulate_search_results(&state, &message_list);
                    }
                    AccountEvent::Error(message) => {
                        let title = glib::markup_escape_text(&format!("{}: {message}", account_label(&state, &account_id)));
                        toast_overlay.add_toast(adw::Toast::new(&title));
                    }
                }
            }
        }
    });
}

/// Connects one manually-added ("other") IMAP/SMTP account - the
/// non-GOA counterpart to `connect_account`. Builds the session's
/// `AccountConfig` directly from the persisted `OtherAccount` fields instead
/// of a `GoaMailAccount`, and its credentials come from the GNOME keyring
/// (`OtherCredentialProvider`) rather than GOA/Microsoft OAuth - everything
/// past that point (the `AccountHandle`, the session actor, the event loop)
/// is identical, so it's shared via `spawn_account_event_loop`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn connect_other_account(
    worker: Rc<Worker>,
    state: Rc<RefCell<UiState>>,
    folder_selection: gtk::SingleSelection,
    folder_scroller: gtk::ScrolledWindow,
    message_list: MessageListModel,
    message_list_stack: gtk::Stack,
    message_header: crate::message_header::MessageHeader,
    reading_stack: gtk::Stack,
    toast_overlay: adw::ToastOverlay,
    window: adw::ApplicationWindow,
    app: adw::Application,
    list_header: ListHeader,
    account: crate::app_config::OtherAccount,
    keyring: std::sync::Arc<dyn crate::other_accounts::KeyringStore>,
    dashboard_refresh: Rc<dyn Fn()>,
    mark_read_button: gtk::Button,
    pin_button: gtk::Button,
) {
    let account_id = AccountId(account.id.clone());
    let display_name = account.display_name.clone();
    let (mention_addresses, mention_names) = mention_identity_lists(&state.borrow().app_config.borrow().identities_for_account(&account_id, &display_name, &account.email));
    let config = AccountConfig {
        account_id: account_id.clone(),
        display_name: account.display_name.clone(),
        email: account.email.clone(),
        mention_addresses,
        mention_names,
        imap: EndpointConfig {
            host: account.imap_host.clone(),
            port: account.imap_port,
            use_tls: account.imap_use_tls,
            username: account.imap_username.clone(),
        },
        smtp: EndpointConfig {
            host: account.smtp_host.clone(),
            port: account.smtp_port,
            use_tls: account.smtp_use_tls,
            username: account.smtp_username.clone(),
        },
    };
    // Unlike the GOA/Microsoft providers, `OtherCredentialProvider` holds no
    // `Rc`s (only an `Arc<dyn KeyringStore>`), so it's already `Send + Sync`
    // and needs no `SendWrapper` shim to satisfy `run_account_session`'s
    // `Arc<dyn CredentialProvider>` bound.
    let credentials: std::sync::Arc<dyn lookout_mail::session::CredentialProvider> =
        std::sync::Arc::new(crate::other_accounts::OtherCredentialProvider::new(account_id.clone(), keyring));

    // Commands stay unbounded (sent via `send_blocking` from the main thread);
    // events are bounded so the drain coalesces a batch and stalls the session
    // under a flood instead of queueing unboundedly - see `EVENT_CHANNEL_CAPACITY`.
    // Two command channels: user-facing fetches (`FetchBody`/`FetchAttachment`/
    // `FetchRawMessage`) ride the interactive one, which the session drains
    // ahead of the background one - see `AccountHandle::interactive_cmd_tx`.
    let (cmd_tx, cmd_rx) = async_channel::unbounded();
    let (interactive_cmd_tx, interactive_cmd_rx) = async_channel::unbounded();
    let (evt_tx, evt_rx) = async_channel::bounded(EVENT_CHANNEL_CAPACITY);
    state.borrow_mut().accounts.insert(
        account_id.clone(),
        AccountHandle {
            cmd_tx,
            interactive_cmd_tx,
            email: config.email.clone(),
            display_name,
            imap_host: config.imap.host.clone(),
            imap_port: config.imap.port,
            smtp_host: config.smtp.host.clone(),
            smtp_port: config.smtp.port,
            folders: Vec::new(),
            // Opened off the GTK thread below and patched in once ready - see
            // `spawn_cache_open`. The session opens (and creates) the same
            // file; whichever gets there first wins, and until either lands
            // the field just means no composer suggestions yet.
            address_cache: None,
            // Manual ("other") accounts are never Microsoft 365 - Graph pin
            // mirroring only ever applies to GOA-discovered M365 accounts,
            // wired in `connect_account`.
            graph_pin: None,
            graph_mail: None,
            graph_categories: None,
            graph_rules: None,
            graph_automatic_replies: None,
            sieve_advertised: false,
        },
    );
    rebuild_folder_tree(&state, &folder_selection, &folder_scroller);
    message_list.set_viewer_addresses(viewer_addresses(&state.borrow()));
    {
        let reply_rx = spawn_cache_open(&worker, account_id.clone());
        let state = state.clone();
        let account_id = account_id.clone();
        glib::spawn_future_local(async move {
            let Ok(cache) = reply_rx.recv().await else { return };
            if let Some(handle) = state.borrow_mut().accounts.get_mut(&account_id) {
                handle.address_cache = cache;
            }
        });
    }

    worker.spawn(lookout_mail::session::run_account_session(config, credentials, cmd_rx, interactive_cmd_rx, evt_tx));

    spawn_account_event_loop(
        evt_rx,
        state,
        account_id,
        folder_selection,
        folder_scroller,
        message_list,
        message_list_stack,
        message_header,
        reading_stack,
        toast_overlay,
        window,
        app,
        list_header,
        dashboard_refresh,
        mark_read_button,
        pin_button,
        worker,
    );
}

/// Tears a GOA account down after the user disables it in Config → Accounts:
/// every running service (mail, calendar, Google Tasks, contacts) stops by
/// dropping its handle - which closes the session's command channel, the
/// same clean-shutdown convention the other-account Remove path relies on -
/// per-account state is pruned, and every view is repainted so nothing from
/// the account remains visible. The on-disk caches are deliberately kept: a
/// re-enable reconnects to them immediately (see `connect_goa_account`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn teardown_goa_account(
    state: &Rc<RefCell<UiState>>,
    calendar_state: &Rc<RefCell<CalendarUiState>>,
    account_id: &AccountId,
    email: Option<&str>,
    folder_selection: &gtk::SingleSelection,
    folder_scroller: &gtk::ScrolledWindow,
    message_list: &MessageListModel,
    message_list_stack: &gtk::Stack,
    list_header: &ListHeader,
    calendar_main: &Rc<CalendarMain>,
    calendar_list_box: &gtk::Box,
    reminders_engine: &Rc<RefCell<crate::reminders::ReminderEngine>>,
    tasks_view: &Rc<crate::tasks_view::TasksView>,
    refresh_contacts_ui: &Rc<dyn Fn(Option<i32>)>,
    dashboard_refresh: &Rc<dyn Fn()>,
) {
    // -- mail -- Prune every per-mailbox piece of state the account feeds so
    // no stale data survives, and note whether the open mailbox belonged to
    // it (the view must navigate away or the messages stay on screen).
    let open_mailbox_was_this_account = {
        let mut st = state.borrow_mut();
        st.accounts.remove(account_id);
        st.refreshing.remove(account_id);
        let belongs = |mailbox: &MailboxId| mailbox_account_id(mailbox).as_ref() == Some(account_id);
        st.unified_snapshots.retain(|mailbox, _| !belongs(mailbox));
        st.pending_optimistic_removals.retain(|mailbox, _| !belongs(mailbox));
        st.pending_optimistic_flag_changes.retain(|mailbox, _| !belongs(mailbox));
        st.pending_optimistic_pinned_changes.retain(|mailbox, _| !belongs(mailbox));
        st.search_results.retain(|summary| !belongs(&summary.mailbox));
        st.search_pending.retain(|(_, mailbox)| !belongs(mailbox));
        st.current_mailbox.as_ref().is_some_and(belongs)
    };
    clear_account_syncing(state, account_id);
    if open_mailbox_was_this_account {
        enter_unified_inbox(state, message_list, message_list_stack);
    }
    // A unified-view list drawn from the disabled account's inbox snapshots
    // must repopulate even when no mailbox was open.
    if matches!(state.borrow().mail_view, MailView::UnifiedInbox) {
        let all = merge_unified_snapshots(&state.borrow().unified_snapshots);
        let (key, descending) = current_sort(state);
        message_list.repopulate(all, key, descending);
        refresh_message_loading_state(state, message_list, message_list_stack);
    }
    rebuild_folder_tree(state, folder_selection, folder_scroller);
    refresh_list_header(state, list_header);
    dashboard_refresh();

    // -- calendar, Google Tasks, reminders --
    {
        let mut st = calendar_state.borrow_mut();
        st.accounts.remove(account_id);
        // Calendar ids embed their account's id as a prefix; drop every
        // checked entry belonging to the disabled account.
        let prefix = format!("{}:", account_id.0);
        st.checked_calendar_ids.retain(|id| !id.0.starts_with(&prefix));
        if let Some(email) = email {
            st.google_tasks.remove(email);
            st.google_account_emails.retain(|e| e != email);
        }
    }
    reminders_engine.borrow_mut().remove_account(account_id);
    refresh_calendar_checklist(calendar_state, calendar_list_box, calendar_main);
    refresh_displayed_calendar_view(calendar_state, calendar_main);
    refresh_tasks_view(calendar_state, tasks_view);

    // -- contacts --
    {
        let mut st = state.borrow_mut();
        st.contacts_by_account.remove(account_id);
        st.deleted_contacts.remove(account_id);
        st.contact_cmd_tx.remove(account_id);
    }
    refresh_contacts_ui(None);
}

/// Reconnects every service a GOA account advertises after the user
/// re-enables it in Config → Accounts, from the account's stored discovery
/// structs (`UiState::goa_accounts`) - no discovery re-run needed. Also
/// flips the relevant pages back from their empty states, since a re-enabled
/// account is live again the moment its sessions spawn.
#[allow(clippy::too_many_arguments)]
pub(crate) fn connect_goa_account(
    worker: Rc<Worker>,
    state: Rc<RefCell<UiState>>,
    calendar_state: Rc<RefCell<CalendarUiState>>,
    account_id: &AccountId,
    account: &DiscoveredGoaAccount,
    root_stack: gtk::Stack,
    current_mail_page: Rc<Cell<&'static str>>,
    mail_view_button: gtk::ToggleButton,
    current_calendar_page: Rc<Cell<&'static str>>,
    calendar_view_button: gtk::ToggleButton,
    current_tasks_page: Rc<Cell<&'static str>>,
    tasks_view_button: gtk::ToggleButton,
    current_lookout_page: Rc<Cell<&'static str>>,
    lookout_view_button: gtk::ToggleButton,
    folder_selection: gtk::SingleSelection,
    folder_scroller: gtk::ScrolledWindow,
    message_list: MessageListModel,
    message_list_stack: gtk::Stack,
    message_header: crate::message_header::MessageHeader,
    reading_stack: gtk::Stack,
    toast_overlay: adw::ToastOverlay,
    window: adw::ApplicationWindow,
    app: adw::Application,
    list_header: ListHeader,
    dashboard_refresh: Rc<dyn Fn()>,
    mark_read_button: gtk::Button,
    pin_button: gtk::Button,
    calendar_main: Rc<CalendarMain>,
    calendar_list_box: gtk::Box,
    mini_calendar: calendar_view::MiniCalendar,
    mail_overview_day: Rc<Cell<chrono::NaiveDate>>,
    mail_overview_day_list: gtk::Box,
    reminders_engine: Rc<RefCell<crate::reminders::ReminderEngine>>,
    tasks_view: Rc<crate::tasks_view::TasksView>,
    refresh_contacts_ui: Rc<dyn Fn(Option<i32>)>,
) {
    let Some(goa_client) = state.borrow().goa_client.clone() else {
        tracing::warn!("no GOA client available to reconnect {account_id}");
        return;
    };
    let account_id = account_id.clone();
    if let Some(mail) = &account.mail {
        connect_account(
            worker.clone(),
            state.clone(),
            folder_selection.clone(),
            folder_scroller.clone(),
            message_list.clone(),
            message_list_stack.clone(),
            message_header.clone(),
            reading_stack.clone(),
            toast_overlay.clone(),
            window.clone(),
            app.clone(),
            goa_client.clone(),
            list_header.clone(),
            mail.clone(),
            dashboard_refresh.clone(),
            mark_read_button.clone(),
            pin_button.clone(),
        );
        current_mail_page.set("mail");
        if mail_view_button.is_active() {
            root_stack.set_visible_child_name("mail");
        }
    }
    if let Some(calendar) = &account.calendar {
        connect_calendar_account(
            worker.clone(),
            calendar_state.clone(),
            calendar_main.clone(),
            calendar_list_box.clone(),
            mini_calendar.clone(),
            mail_overview_day.clone(),
            mail_overview_day_list.clone(),
            reminders_engine.clone(),
            toast_overlay.clone(),
            goa_client.clone(),
            calendar.clone(),
            tasks_view.clone(),
        );
        current_calendar_page.set("calendar");
        if calendar_view_button.is_active() {
            root_stack.set_visible_child_name("calendar");
        }
        current_tasks_page.set("tasks");
        if tasks_view_button.is_active() {
            root_stack.set_visible_child_name("tasks");
        }
    }
    if let Some(contacts) = &account.contacts {
        let (cmd_tx, cmd_rx) = async_channel::unbounded();
        state.borrow_mut().contact_cmd_tx.insert(account_id.clone(), cmd_tx);
        sync_contacts_account(
            worker.clone(),
            state.clone(),
            toast_overlay.clone(),
            goa_client.clone(),
            contacts.clone(),
            cmd_rx,
            refresh_contacts_ui.clone(),
        );
    }
    // Google Tasks needs a stored OAuth token to connect silently; without
    // one it just reappears as a "Connect Google Tasks" button target.
    if account.provider_type.as_deref() == Some("google") {
        if !calendar_state.borrow().google_account_emails.contains(&account.email) {
            calendar_state.borrow_mut().google_account_emails.push(account.email.clone());
        }
        if google_tasks::has_stored_token(&account.email) && !calendar_state.borrow().google_tasks.contains_key(&account.email) {
            connect_google_tasks_account(worker.clone(), calendar_state.clone(), tasks_view.clone(), toast_overlay.clone(), account.email.clone());
        }
    }
    // The Lookout dashboard goes live as soon as any account set exists.
    if !state.borrow().accounts.is_empty() || !calendar_state.borrow().accounts.is_empty() {
        current_lookout_page.set("lookout");
        if lookout_view_button.is_active() {
            root_stack.set_visible_child_name("lookout");
        }
    }
}

/// Mirrors `spawn_account_discovery` 1:1 for Calendar - a fully independent
/// GOA account set, discovered and connected the same worker-spawn +
/// `glib::spawn_future_local` way.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_calendar_discovery(
    worker: Rc<Worker>,
    state: Rc<RefCell<UiState>>,
    calendar_state: Rc<RefCell<CalendarUiState>>,
    root_stack: gtk::Stack,
    toast_overlay: adw::ToastOverlay,
    calendar_main: Rc<CalendarMain>,
    calendar_list_box: gtk::Box,
    mini_calendar: calendar_view::MiniCalendar,
    mail_overview_day: Rc<Cell<chrono::NaiveDate>>,
    mail_overview_day_list: gtk::Box,
    current_calendar_page: Rc<Cell<&'static str>>,
    calendar_view_button: gtk::ToggleButton,
    current_tasks_page: Rc<Cell<&'static str>>,
    tasks_view_button: gtk::ToggleButton,
    tasks_view: Rc<crate::tasks_view::TasksView>,
    reminders_engine: Rc<RefCell<crate::reminders::ReminderEngine>>,
    refresh_config: Rc<dyn Fn()>,
    current_lookout_page: Rc<Cell<&'static str>>,
    lookout_view_button: gtk::ToggleButton,
) {
    let (goa_tx, goa_rx) = async_channel::bounded(1);
    worker.spawn(async move {
        let result = async {
            let client = GoaClient::connect().await?;
            let accounts = client.list_calendar_accounts().await?;
            Ok::<_, lookout_goa::Error>((client, accounts))
        }
        .await;
        let _ = goa_tx.send(result).await;
    });

    glib::spawn_future_local(async move {
        let Ok(result) = goa_rx.recv().await else { return };
        let show_page = |name: &'static str| {
            current_calendar_page.set(name);
            if calendar_view_button.is_active() {
                root_stack.set_visible_child_name(name);
            }
        };
        let show_tasks_page = |name: &'static str| {
            current_tasks_page.set(name);
            if tasks_view_button.is_active() {
                root_stack.set_visible_child_name(name);
            }
        };
        // The Lookout tab goes live as soon as *any* account set exists.
        // Deliberately upgrade-only: the mail discovery owns the downgrade
        // to "lookout-empty", so finding no calendar accounts here must not
        // hide the dashboard when mail accounts exist.
        let show_lookout_page = |name: &'static str| {
            current_lookout_page.set(name);
            if lookout_view_button.is_active() {
                root_stack.set_visible_child_name(name);
            }
        };
        match result {
            Ok((client, accounts)) => {
                // Record every discovered calendar account into `state`'s GOA
                // union (disabled ones included - see the mail discovery's
                // note), then connect only the enabled ones. A nothing-but-
                // disabled result falls through to the same empty handling
                // as GOA reporting no calendar accounts at all.
                let mut enabled_accounts = Vec::new();
                for account in accounts {
                    let id = account.account_id.clone();
                    let enabled = {
                        let mut st = state.borrow_mut();
                        st.goa_client = Some(client.clone());
                        let entry = st.goa_accounts.entry(id.clone()).or_insert(DiscoveredGoaAccount {
                            display_name: String::new(),
                            email: String::new(),
                            provider_type: None,
                            mail: None,
                            calendar: None,
                            contacts: None,
                        });
                        entry.display_name = account.display_name.clone();
                        entry.email = account.display_name.clone();
                        entry.provider_type = account.provider_type.clone();
                        entry.calendar = Some(account.clone());
                        st.account_enabled(&id)
                    };
                    // A Microsoft 365 account is only a usable calendar once
                    // the user has opted into Outlook sync (Graph); until then
                    // it isn't counted as enabled, so the page stays empty.
                    let usable = !account.is_microsoft_365() || crate::microsoft_oauth::MicrosoftOAuth::new(account.account_id.clone()).graph_consented();
                    if enabled && usable {
                        enabled_accounts.push(account);
                    }
                }
                if !enabled_accounts.is_empty() {
                    show_page("calendar");
                    show_tasks_page("tasks");
                    show_lookout_page("lookout");
                    for account in enabled_accounts {
                        connect_calendar_account(
                            worker.clone(),
                            calendar_state.clone(),
                            calendar_main.clone(),
                            calendar_list_box.clone(),
                            mini_calendar.clone(),
                            mail_overview_day.clone(),
                            mail_overview_day_list.clone(),
                            reminders_engine.clone(),
                            toast_overlay.clone(),
                            client.clone(),
                            account,
                            tasks_view.clone(),
                        );
                    }
                    refresh_config();
                } else {
                    show_page("calendar-empty");
                    show_tasks_page("tasks-empty");
                }
            }
            Err(e) => {
                show_page("calendar-empty");
                show_tasks_page("tasks-empty");
                if e.is_service_unavailable() {
                    state.borrow_mut().goa_unavailable = true;
                    refresh_config();
                } else {
                    let title = glib::markup_escape_text(&format!("Couldn't reach GNOME Online Accounts: {e}"));
                    toast_overlay.add_toast(adw::Toast::new(&title));
                }
            }
        }
    });
}

pub(crate) fn ensure_checked_calendars(checked: &mut HashSet<CalendarId>, calendars: &[CalendarInfo]) {
    for calendar in calendars {
        checked.insert(calendar.id.clone());
    }
}

/// Starts the Google Tasks integration: lists every Google GOA calendar
/// account (the provider that can't store CalDAV tasks), remembers their
/// emails for the "Connect Google Tasks" button, and auto-connects those
/// with a stored refresh token - the non-interactive path, since a stored
/// token means the user authorized once already.
pub(crate) fn spawn_google_tasks_discovery(
    worker: Rc<Worker>,
    state: Rc<RefCell<UiState>>,
    calendar_state: Rc<RefCell<CalendarUiState>>,
    tasks_view: Rc<crate::tasks_view::TasksView>,
    toast_overlay: adw::ToastOverlay,
) {
    let (goa_tx, goa_rx) = async_channel::bounded(1);
    worker.spawn(async move {
        let result = async {
            let client = GoaClient::connect().await?;
            let accounts = client.list_calendar_accounts().await?;
            Ok::<_, lookout_goa::Error>(accounts)
        }
        .await;
        let _ = goa_tx.send(result).await;
    });

    glib::spawn_future_local(async move {
        let Ok(result) = goa_rx.recv().await else { return };
        match result {
            Ok(accounts) => {
                // Disabled accounts are excluded here too - a disabled Google
                // account must neither auto-connect nor appear as a "Connect
                // Google Tasks" target.
                let emails: Vec<String> = accounts
                    .iter()
                    .filter(|a| a.provider_type.as_deref() == Some("google") && state.borrow().account_enabled(&a.account_id))
                    .map(|a| a.display_name.clone())
                    .collect();
                {
                    let mut st = calendar_state.borrow_mut();
                    st.google_account_emails = emails.clone();
                }
                for email in emails {
                    if google_tasks::has_stored_token(&email) && !calendar_state.borrow().google_tasks.contains_key(&email) {
                        connect_google_tasks_account(worker.clone(), calendar_state.clone(), tasks_view.clone(), toast_overlay.clone(), email);
                    }
                }
            }
            Err(e) => {
                tracing::warn!("couldn't discover Google Tasks accounts: {e}");
            }
        }
    });
}

/// The interactive "Connect Google Tasks" action: runs the OAuth
/// authorization-code flow (opening a browser) for the account's first
/// Google GOA account, then spawns the sync session. Rejects politely when
/// there's no Google account or it's already connected.
pub(crate) fn connect_google_tasks(
    worker: &Rc<Worker>,
    calendar_state: &Rc<RefCell<CalendarUiState>>,
    tasks_view: &Rc<crate::tasks_view::TasksView>,
    toast_overlay: &adw::ToastOverlay,
) {
    let email = {
        let st = calendar_state.borrow();
        let Some(email) = st.google_account_emails.first().cloned() else {
            toast_overlay.add_toast(adw::Toast::new("Add a Google account in GNOME Online Accounts first."));
            return;
        };
        if st.google_tasks.contains_key(&email) {
            toast_overlay.add_toast(adw::Toast::new("Google Tasks is already connected."));
            return;
        }
        email
    };

    let worker = worker.clone();
    let calendar_state = calendar_state.clone();
    let tasks_view = tasks_view.clone();
    let toast_overlay = toast_overlay.clone();
    let (tx, rx) = async_channel::bounded(1);
    let email_for_auth = email.clone();
    worker.spawn(async move {
        let oauth = google_tasks::GoogleTasksOAuth::new(&email_for_auth);
        let _ = tx.send(oauth.access_token().await).await;
    });
    glib::spawn_future_local(async move {
        match rx.recv().await {
            Ok(Ok(_token)) => {
                let title = glib::markup_escape_text(&format!("Google Tasks connected for {email}"));
                toast_overlay.add_toast(adw::Toast::new(&title));
                connect_google_tasks_account(worker, calendar_state, tasks_view, toast_overlay, email);
            }
            Ok(Err(e)) => {
                let title = glib::markup_escape_text(&format!("Couldn't connect Google Tasks: {e}"));
                toast_overlay.add_toast(adw::Toast::new(&title));
            }
            Err(_) => {}
        }
    });
}

/// Spawns one Google account's Tasks session and routes its events into the
/// Tasks view: lists become picker entries (with calendar colours), task
/// snapshots merge into the view, and errors toast.
pub(crate) fn connect_google_tasks_account(
    worker: Rc<Worker>,
    calendar_state: Rc<RefCell<CalendarUiState>>,
    tasks_view: Rc<crate::tasks_view::TasksView>,
    toast_overlay: adw::ToastOverlay,
    email: String,
) {
    // Commands stay unbounded (sent via `send_blocking` from the main thread);
    // events are bounded so the drain coalesces a batch and stalls the session
    // under a flood instead of queueing unboundedly - see `EVENT_CHANNEL_CAPACITY`.
    let (cmd_tx, cmd_rx) = async_channel::unbounded();
    let (evt_tx, evt_rx) = async_channel::bounded(EVENT_CHANNEL_CAPACITY);
    calendar_state.borrow_mut().google_tasks.insert(
        email.clone(),
        GoogleTasksHandle {
            cmd_tx,
            email: email.clone(),
            task_lists: Vec::new(),
            last_tasks: Vec::new(),
            error: None,
        },
    );
    worker.spawn(google_tasks::run_google_tasks_session(email.clone(), cmd_rx, evt_tx));

    glib::spawn_future_local(async move {
        while let Ok(event) = evt_rx.recv().await {
            // Drain the whole queued batch before handling, collapsing
            // whole-snapshot events (lists, tasks) into the last copy of each
            // - the cache fast-paint and the first live poll queue the same
            // snapshots back to back.
            let mut batch = vec![event];
            while let Ok(next) = evt_rx.try_recv() {
                batch.push(next);
            }
            for event in collapse_google_tasks_events(batch) {
                match event {
                    GoogleTasksEvent::ListsUpdated(lists) => {
                        {
                            let mut st = calendar_state.borrow_mut();
                            if let Some(handle) = st.google_tasks.get_mut(&email) {
                                handle.task_lists = lists.clone();
                                handle.error = None;
                            }
                            let infos: Vec<CalendarInfo> = lists
                                .iter()
                                .map(|list| CalendarInfo {
                                    id: google_tasks::google_task_calendar_id(&list.id),
                                    account_id: AccountId(format!("googletasks:{email}")),
                                    display_name: list.title.clone(),
                                    color: None,
                                    href: String::new(),
                                    supports_tasks: true,
                                })
                                .collect();
                            calendar_colors::assign_missing(&mut st.calendar_colors, &infos);
                        }
                        refresh_tasks_view(&calendar_state, &tasks_view);
                    }
                    GoogleTasksEvent::TasksUpdated(tasks) => {
                        if let Some(handle) = calendar_state.borrow_mut().google_tasks.get_mut(&email) {
                            handle.last_tasks = tasks;
                            handle.error = None;
                        }
                        refresh_tasks_view(&calendar_state, &tasks_view);
                    }
                    GoogleTasksEvent::Error(message) => {
                        if let Some(handle) = calendar_state.borrow_mut().google_tasks.get_mut(&email) {
                            handle.error = Some(message.clone());
                        }
                        toast_overlay.add_toast(adw::Toast::new(&glib::markup_escape_text(&message)));
                    }
                }
            }
        }
    });
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn connect_calendar_account(
    worker: Rc<Worker>,
    calendar_state: Rc<RefCell<CalendarUiState>>,
    calendar_main: Rc<CalendarMain>,
    calendar_list_box: gtk::Box,
    mini_calendar: calendar_view::MiniCalendar,
    mail_overview_day: Rc<Cell<chrono::NaiveDate>>,
    mail_overview_day_list: gtk::Box,
    reminders_engine: Rc<RefCell<crate::reminders::ReminderEngine>>,
    toast_overlay: adw::ToastOverlay,
    goa_client: GoaClient,
    account: GoaCalendarAccount,
    tasks_view: Rc<crate::tasks_view::TasksView>,
) {
    let account_id = account.account_id.clone();
    let display_name = account.display_name.clone();
    // Microsoft 365 calendars are served by the Graph backend (OUTLOOK_PARITY
    // item 5.1), not CalDAV - GOA reports their Calendar interface with an
    // empty `Uri`. Graph access is opt-in (Config's "Sync with Outlook" runs
    // the consent flow), so an unconsented account has no calendar until then.
    let microsoft_365 = account.is_microsoft_365();
    if microsoft_365 && !crate::microsoft_oauth::MicrosoftOAuth::new(account_id.clone()).graph_consented() {
        tracing::debug!("Microsoft 365 calendar {account_id} has no Outlook-sync consent yet; skipping until enabled in Config");
        return;
    }
    let config = CalendarAccountConfig {
        account_id: account_id.clone(),
        display_name: display_name.clone(),
        base_url: account.uri.clone(),
        accept_ssl_errors: account.accept_ssl_errors,
        // `Account.Identity` is the login username the CalDAV server expects
        // (e.g. the Nextcloud user id); the display name is only a fallback
        // for providers that don't advertise an Identity.
        username: if account.identity.is_empty() { display_name.clone() } else { account.identity.clone() },
    };
    let credentials: Rc<dyn lookout_dav::session::CalendarCredentialProvider> = Rc::new(GoaCalendarCredentialProvider::new(goa_client, account));
    // Same unsafe Send/Sync wrapper pattern as `connect_account`, documented
    // there - `run_calendar_session` requires `Arc<dyn
    // CalendarCredentialProvider>`, but the provider is only ever driven
    // from this one worker task.
    struct SendWrapper(Rc<dyn lookout_dav::session::CalendarCredentialProvider>);
    unsafe impl Send for SendWrapper {}
    unsafe impl Sync for SendWrapper {}
    #[async_trait::async_trait]
    impl lookout_dav::session::CalendarCredentialProvider for SendWrapper {
        async fn calendar_credential(&self) -> Result<lookout_dav::Credential, String> {
            self.0.calendar_credential().await
        }
    }
    let credentials: std::sync::Arc<dyn lookout_dav::session::CalendarCredentialProvider> = std::sync::Arc::new(SendWrapper(credentials));

    // Commands stay unbounded (sent via `send_blocking` from the main thread);
    // events are bounded so the drain coalesces a batch and stalls the session
    // under a flood instead of queueing unboundedly - see `EVENT_CHANNEL_CAPACITY`.
    let (cmd_tx, cmd_rx) = async_channel::unbounded();
    let (evt_tx, evt_rx) = async_channel::bounded(EVENT_CHANNEL_CAPACITY);
    calendar_state.borrow_mut().accounts.insert(
        account_id.clone(),
        CalendarAccountHandle {
            cmd_tx,
            display_name,
            uri: if microsoft_365 { "Microsoft 365".to_string() } else { config.base_url.clone() },
            graph: microsoft_365,
            calendars: Vec::new(),
            connection_state: CalConnectionState::Connecting,
            last_occurrences: Vec::new(),
            last_synced_month: None,
            occurrences_by_month: HashMap::new(),
            last_tasks: Vec::new(),
            scheduling: SchedulingInfo::default(),
        },
    );

    if microsoft_365 {
        // The Graph session needs only the account's own OAuth token provider,
        // which `MicrosoftOAuth` supplies (`lookout_graph::TokenProvider`).
        let oauth = std::sync::Arc::new(crate::microsoft_oauth::MicrosoftOAuth::new(account_id.clone()));
        let client = lookout_graph::CalendarClient::new(lookout_graph::GraphClient::new(oauth));
        worker.spawn(lookout_graph::run_calendar_session(account_id.clone(), client, cmd_rx, evt_tx));
    } else {
        worker.spawn(lookout_dav::session::run_calendar_session(config, credentials, cmd_rx, evt_tx));
    }

    glib::spawn_future_local(async move {
        while let Ok(event) = evt_rx.recv().await {
            // Drain the whole queued batch before handling, collapsing
            // whole-snapshot events (calendars, a month's occurrences, tasks)
            // into the last copy of each - the fast-paint cache replay and
            // the first live sync queue the same snapshots back to back.
            let mut batch = vec![event];
            while let Ok(next) = evt_rx.try_recv() {
                batch.push(next);
            }
            for event in collapse_calendar_events(batch) {
                match event {
                    CalendarSessionEvent::SchedulingUpdated(info) => {
                        if let Some(handle) = calendar_state.borrow_mut().accounts.get_mut(&account_id) {
                            handle.scheduling = info;
                        }
                    }
                    CalendarSessionEvent::ConnectionStateChanged(state) => {
                        if let Some(handle) = calendar_state.borrow_mut().accounts.get_mut(&account_id) {
                            handle.connection_state = state.clone();
                        }
                        if let CalConnectionState::Error { message, retryable } = &state {
                            // Retryable failures are warnings (the session
                            // reconnects itself with backoff); the account's
                            // sidebar status text still shows the message, but no
                            // toast spams on every attempt. Only non-retryable
                            // (fatal) errors surface.
                            if !retryable {
                                let title = glib::markup_escape_text(&format!("{}: {message}", calendar_account_label(&calendar_state, &account_id)));
                                toast_overlay.add_toast(adw::Toast::new(&title));
                            }
                        }
                        refresh_calendar_checklist(&calendar_state, &calendar_list_box, &calendar_main);
                    }
                    CalendarSessionEvent::CalendarsUpdated(calendars) => {
                        if let Some(handle) = calendar_state.borrow_mut().accounts.get_mut(&account_id) {
                            handle.calendars = calendars.clone();
                        }
                        refresh_calendar_checklist(&calendar_state, &calendar_list_box, &calendar_main);
                        if let Some(handle) = calendar_state.borrow().accounts.get(&account_id) {
                            if !handle.calendars.is_empty() {
                                let _ = handle.cmd_tx.send_blocking(CalendarCommand::SyncMonth(calendar_state.borrow().displayed_month));
                            }
                        }
                    }
                    CalendarSessionEvent::OccurrencesUpdated { month, occurrences } => {
                        // An event the context menu just created (item 3.12,
                        // Duplicate / Copy) is opened once the server's own
                        // copy has synced - so the editor writes to the real
                        // href rather than re-creating the copy on save.
                        let open_after = {
                            let st = calendar_state.borrow();
                            st.pending_open_editor
                                .as_ref()
                                .and_then(|(cal_id, uid)| occurrences.iter().find(|o| o.calendar_id == *cal_id && o.uid == *uid).cloned())
                        };
                        if open_after.is_some() {
                            calendar_state.borrow_mut().pending_open_editor = None;
                        }
                        // Feed the reminder engine before `occurrences` moves into
                        // the handle below - the engine keeps its own copy of
                        // every month it's been shown, since `last_occurrences`
                        // only holds whatever month synced last.
                        reminders_engine.borrow_mut().ingest(&account_id, &occurrences);
                        if let Some(handle) = calendar_state.borrow_mut().accounts.get_mut(&account_id) {
                            handle.last_occurrences = occurrences;
                            handle.last_synced_month = Some(month);
                            insert_dashboard_occurrences(&mut handle.occurrences_by_month, month, handle.last_occurrences.clone());
                        }
                        refresh_displayed_calendar_view(&calendar_state, &calendar_main);
                        refresh_mail_overview_day_list(&calendar_state, mail_overview_day.get(), &mail_overview_day_list);
                        // The dashboard's upcoming-events section follows the
                        // synced occurrences.
                        refresh_dashboard_hook(&calendar_state);
                        // The sidebar mini-calendar's bold event-day numerals track
                        // the currently-displayed month - refresh them when a fetch
                        // for that month lands (navigating to an uncached month
                        // marks its days bold the moment the sync completes).
                        if calendar_view::mini_month(&mini_calendar) == month {
                            let event_days = calendar_event_days(&calendar_state, month);
                            calendar_view::set_mini_event_days(&mini_calendar, &event_days);
                        }
                        // Now that the new event is in the model, open its
                        // editor (item 3.12) through the window's late-bound
                        // hook.
                        if let Some(occ) = open_after {
                            let callback = calendar_state.borrow().open_event_editor.clone();
                            if let Some(callback) = callback {
                                callback(occ);
                            }
                        }
                    }
                    CalendarSessionEvent::Error(message) => {
                        let title = glib::markup_escape_text(&format!("{}: {message}", calendar_account_label(&calendar_state, &account_id)));
                        toast_overlay.add_toast(adw::Toast::new(&title));
                    }
                    CalendarSessionEvent::EventSaveFailed { uid, recurrence_id, message } => {
                        // The save actually failed server-side, so drop whatever
                        // optimistic drag-move was pending for this occurrence (a
                        // no-op if this failure didn't come from a drag) and
                        // repaint - the chip must snap back to its real,
                        // unmoved position rather than staying stuck showing the
                        // unsaved drop location.
                        calendar_state.borrow_mut().pending_calendar_moves.remove(&(uid, recurrence_id));
                        refresh_displayed_calendar_view(&calendar_state, &calendar_main);
                        let title = glib::markup_escape_text(&format!("{}: {message}", calendar_account_label(&calendar_state, &account_id)));
                        toast_overlay.add_toast(adw::Toast::new(&title));
                    }
                    CalendarSessionEvent::TasksUpdated(tasks) => {
                        if let Some(handle) = calendar_state.borrow_mut().accounts.get_mut(&account_id) {
                            handle.last_tasks = tasks;
                        }
                        refresh_tasks_view(&calendar_state, &tasks_view);
                    }
                    CalendarSessionEvent::FreeBusyUpdated { request_id, availabilities } => {
                        if let Some((handle, generation)) = calendar_state.borrow_mut().scheduling_pending.remove(&request_id) {
                            handle.deliver(generation, availabilities);
                        }
                    }
                    CalendarSessionEvent::FreeBusyFailed { request_id, message } => {
                        if let Some((handle, generation)) = calendar_state.borrow_mut().scheduling_pending.remove(&request_id) {
                            handle.fail(generation, message);
                        }
                    }
                }
            }
        }
    });
}

/// The synthetic calendar id a subscription's events carry
/// (`"webcal:<subscription id>"`) - mirrors the `lookout-dav` session's
/// construction, so the two never drift.
pub(crate) fn webcal_calendar_id(subscription_id: &str) -> CalendarId {
    CalendarId(format!("webcal:{subscription_id}"))
}

/// Starts the single webcal feed session: loads the configured subscriptions
/// into `calendar_state`, spawns the polling actor on the worker, and routes
/// its `SubscriptionsUpdated` events into the same funnel as CalDAV
/// `OccurrencesUpdated` (checklist, main view merge, mail-overview day list,
/// mini-calendar event days). Runs at startup regardless of GOA - feeds are
/// not GOA accounts, and one session polls all of them.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_webcal_session(
    worker: Rc<Worker>,
    calendar_state: Rc<RefCell<CalendarUiState>>,
    app_config: Rc<RefCell<crate::app_config::AppConfig>>,
    calendar_main: Rc<CalendarMain>,
    calendar_list_box: gtk::Box,
    mini_calendar: calendar_view::MiniCalendar,
    mail_overview_day: Rc<Cell<chrono::NaiveDate>>,
    mail_overview_day_list: gtk::Box,
    toast_overlay: adw::ToastOverlay,
) {
    let subscriptions = app_config.borrow().webcal_subscriptions.clone();
    // Commands stay unbounded (sent via `send_blocking` from the main thread);
    // events are bounded so the drain coalesces a batch and stalls the session
    // under a flood instead of queueing unboundedly - see `EVENT_CHANNEL_CAPACITY`.
    let (cmd_tx, cmd_rx) = async_channel::unbounded();
    let (evt_tx, evt_rx) = async_channel::bounded(EVENT_CHANNEL_CAPACITY);
    {
        let mut st = calendar_state.borrow_mut();
        st.webcal_cmd_tx = Some(cmd_tx);
        st.webcal_subscriptions = subscriptions.clone();
        for sub in &subscriptions {
            st.webcal_handles.insert(
                sub.id.clone(),
                WebcalHandle {
                    calendar_id: webcal_calendar_id(&sub.id),
                    display_name: sub.display_name.clone(),
                    last_occurrences: Vec::new(),
                    last_synced_month: None,
                    occurrences_by_month: HashMap::new(),
                    error: None,
                },
            );
        }
    }

    worker.spawn(lookout_dav::subscription::run_subscription_session(subscriptions, cmd_rx, evt_tx));

    glib::spawn_future_local(async move {
        while let Ok(event) = evt_rx.recv().await {
            // Drain the whole queued batch before handling, collapsing each
            // month's feeds into the last update - the poll can queue several
            // months back to back, and each one repaints the calendar.
            let mut batch = vec![event];
            while let Ok(next) = evt_rx.try_recv() {
                batch.push(next);
            }
            for event in collapse_subscription_events(batch) {
                let SubscriptionSessionEvent::SubscriptionsUpdated { month, feeds } = event;
                let mut error_toasts: Vec<String> = Vec::new();
                {
                    let mut st = calendar_state.borrow_mut();
                    for feed in feeds {
                        let Some(handle) = st.webcal_handles.get_mut(&feed.subscription_id) else { continue };
                        handle.last_occurrences = feed.occurrences;
                        handle.last_synced_month = Some(month);
                        insert_dashboard_occurrences(&mut handle.occurrences_by_month, month, handle.last_occurrences.clone());
                        // Toast on the *transition* into an error (or the first
                        // error for a fresh handle), not on every 5-minute poll.
                        if handle.error.is_none() && feed.error.is_some() {
                            error_toasts.push(handle.display_name.clone());
                        }
                        handle.error = feed.error;
                    }
                }
                for name in error_toasts {
                    let title = glib::markup_escape_text(&format!("Calendar feed \"{name}\" could not be fetched"));
                    toast_overlay.add_toast(adw::Toast::new(&title));
                }
                refresh_calendar_checklist(&calendar_state, &calendar_list_box, &calendar_main);
                refresh_displayed_calendar_view(&calendar_state, &calendar_main);
                refresh_mail_overview_day_list(&calendar_state, mail_overview_day.get(), &mail_overview_day_list);
                // The dashboard's upcoming-events section follows webcal feeds too.
                refresh_dashboard_hook(&calendar_state);
                if calendar_view::mini_month(&mini_calendar) == month {
                    let event_days = calendar_event_days(&calendar_state, month);
                    calendar_view::set_mini_event_days(&mini_calendar, &event_days);
                }
            }
        }
    });
}
