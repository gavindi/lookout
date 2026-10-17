//! Title-bar search: scope, refiners, cached and live result plumbing, and the unified inbox.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use lookout_core::{display_name, AccountId, EmailSummary, MailboxId, MailboxRole, Uid};
use lookout_mail::session::AccountCommand;
use lookout_mail::{SearchQuery, SearchScope};

use crate::last_view::{self, LastSelection};
use crate::message_list::{unified_merge_order, MessageListModel, SortKey};
use crate::worker::Worker;

use super::*;

/// The local-index pass of a search (M-18, roadmap 2.2/2.3): query the
/// read-side cache of every account in scope, with each account's folder set
/// narrowed by the scope dropdown and any `folder:` token, and merge the hits
/// (deduplicated by `(mailbox, uid)` in the caller). Fires one off-thread query
/// per account and returns a receiver for each - the caller joins them (see
/// `start_search`) rather than blocking the GTK thread.
pub(crate) fn search_cached_results(state: &Rc<RefCell<UiState>>, worker: &Rc<Worker>, query: &str) -> Vec<async_channel::Receiver<Vec<EmailSummary>>> {
    let parsed = SearchQuery::parse(query);
    let (scope, current_account, current_mailbox) = {
        let st = state.borrow();
        (st.search_scope, st.current_account.clone(), st.current_mailbox.clone())
    };
    let folder_token = parsed.folder.clone();
    let jobs: Vec<(std::sync::Arc<lookout_mail::Cache>, Option<Vec<MailboxId>>)> = {
        let st = state.borrow();
        st.accounts
            .iter()
            .filter(|(id, _)| match scope {
                SearchScope::Folder | SearchScope::Subfolders | SearchScope::Account => current_account.as_ref() == Some(id),
                SearchScope::All => true,
            })
            .filter_map(|(_, handle)| {
                let cache = handle.address_cache.clone()?;
                let mailbox_scope = resolve_search_mailboxes(&handle.folders, scope, current_mailbox.as_ref(), folder_token.as_deref());
                Some((cache, mailbox_scope))
            })
            .collect()
    };
    jobs.into_iter()
        .map(|(cache, mailbox_scope)| {
            let parsed = parsed.clone();
            spawn_cache_read(worker, cache, move |cache| {
                cache.search_query(&parsed, mailbox_scope.as_deref(), SEARCH_RESULT_LIMIT).unwrap_or_default()
            })
        })
        .collect()
}

/// The mailboxes a search covers in one account: `None` means every folder of
/// the account (the `Account`/`All` scopes with no `folder:` token), `Some(ids)`
/// restricts to those folders, and `Some(empty)` matches nothing (e.g. on a
/// folder scope with no open mailbox, or a `folder:` token that names no
/// folder). A `folder:` token wins over the scope dropdown and resolves by
/// path or display name.
pub(crate) fn resolve_search_mailboxes(folders: &[lookout_core::Mailbox], scope: SearchScope, current: Option<&MailboxId>, folder_token: Option<&str>) -> Option<Vec<MailboxId>> {
    if let Some(token) = folder_token {
        let needle = token.to_lowercase();
        let ids: Vec<MailboxId> = folders
            .iter()
            .filter(|mailbox| mailbox.name.to_lowercase() == needle || display_name(&mailbox.name).to_lowercase() == needle)
            .map(|mailbox| mailbox.id.clone())
            .collect();
        return Some(ids);
    }
    match scope {
        SearchScope::Folder => Some(current.map(|mailbox| vec![mailbox.clone()]).unwrap_or_default()),
        SearchScope::Subfolders => {
            let Some(current) = current else {
                return Some(Vec::new());
            };
            // A child's MailboxId is `<account>:<parent path>/<child>`, so the
            // parent's id plus the delimiter is the subtree prefix.
            let prefix = format!("{}/", current.0);
            let ids: Vec<MailboxId> = folders
                .iter()
                .filter(|mailbox| &mailbox.id == current || mailbox.id.0.starts_with(&prefix))
                .map(|mailbox| mailbox.id.clone())
                .collect();
            Some(ids)
        }
        SearchScope::Account | SearchScope::All => None,
    }
}

/// Repopulates the message list from the accumulated search results, under the
/// current sort, with copies of one message across folders collapsed (see
/// `collapse_search_copies`).
pub(crate) fn repopulate_search_results(state: &Rc<RefCell<UiState>>, message_list: &MessageListModel) {
    let (key, descending) = current_sort(state);
    let results = {
        let st = state.borrow();
        collapse_search_copies(&st.search_results, |mailbox| {
            let account_id = mailbox_account_id(mailbox)?;
            st.accounts.get(&account_id)?.folders.iter().find(|f| &f.id == mailbox).map(|f| f.role)
        })
    };
    message_list.repopulate(results, key, descending);
}

/// Collapses the copies of one message a search found in several folders of
/// the same account - matched by Message-ID - to a single row. The index
/// covers every folder (the prefetch indexes the ones never opened), and
/// Gmail exposes each message once per label plus again in All Mail, so
/// without this every Gmail hit would list at least twice. The copy kept is
/// the one in the most specific folder: Inbox, then any other folder, then
/// Archive/All Mail, then Junk/Trash; ties keep the first-found copy. Hits
/// without a Message-ID are never merged. `role_of` resolves a hit's folder.
pub(crate) fn collapse_search_copies(results: &[EmailSummary], role_of: impl Fn(&MailboxId) -> Option<MailboxRole>) -> Vec<EmailSummary> {
    pub(crate) fn rank(role: Option<MailboxRole>) -> u8 {
        match role {
            Some(MailboxRole::Inbox) => 0,
            Some(MailboxRole::Archive) => 2,
            Some(MailboxRole::Junk | MailboxRole::Trash) => 3,
            _ => 1,
        }
    }
    let mut out: Vec<EmailSummary> = Vec::with_capacity(results.len());
    let mut kept: HashMap<(Option<AccountId>, String), (usize, u8)> = HashMap::new();
    for summary in results {
        let message_id = summary
            .message_id
            .as_deref()
            .map(|id| id.trim().trim_start_matches('<').trim_end_matches('>'))
            .filter(|id| !id.is_empty());
        let Some(message_id) = message_id else {
            out.push(summary.clone());
            continue;
        };
        let this_rank = rank(role_of(&summary.mailbox));
        match kept.entry((mailbox_account_id(&summary.mailbox), message_id.to_string())) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                let (index, kept_rank) = *entry.get();
                if this_rank < kept_rank {
                    out[index] = summary.clone();
                    entry.insert((index, this_rank));
                }
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert((out.len(), this_rank));
                out.push(summary.clone());
            }
        }
    }
    out
}

/// Asks the live IMAP pass to cover the mailbox the user was viewing - or
/// every account's Inbox when the pre-search view was the unified one (the
/// same folders `account_inboxes` names). Searches arrive in these folders, so
/// this is where the local index's gaps are: mail that has never been synced,
/// and body text for messages never fetched. (Folders never opened are covered
/// locally up to the background prefetch's newest-N window - see
/// `Cache::store_partial_messages`.) One `SearchMailbox` per folder
/// (a SELECT + `UID SEARCH` + fetch each), which is why the fan-out stops at
/// the open view instead of covering every folder of every account - a search
/// over an unopened folder would otherwise pay a round trip for nothing the
/// user is looking at.
pub(crate) fn dispatch_search_fallbacks(state: &Rc<RefCell<UiState>>, query: &str) {
    let targets: Vec<(AccountId, MailboxId)> = {
        let st = state.borrow();
        if st.current_mailbox.is_some() {
            st.current_account.clone().zip(st.current_mailbox.clone()).into_iter().collect()
        } else {
            account_inboxes(state)
        }
    };
    let mut sent: Vec<(AccountId, MailboxId)> = Vec::new();
    {
        let mut st = state.borrow_mut();
        for (account_id, mailbox) in targets {
            if st.search_pending.insert((account_id.clone(), mailbox.clone())) {
                sent.push((account_id, mailbox));
            }
        }
    }
    for (account_id, mailbox) in sent {
        if let Some(handle) = state.borrow().accounts.get(&account_id) {
            let _ = handle.cmd_tx.send_blocking(AccountCommand::SearchMailbox {
                mailbox,
                query: query.to_string(),
            });
        }
    }
}

/// The dropdown index for a [`SearchScope`], matching the order the header and
/// ribbon scope dropdowns are built with.
pub(crate) fn search_scope_index(scope: SearchScope) -> u32 {
    match scope {
        SearchScope::Folder => 0,
        SearchScope::Subfolders => 1,
        SearchScope::Account => 2,
        SearchScope::All => 3,
    }
}

/// The [`SearchScope`] a dropdown index selects.
pub(crate) fn search_scope_at(index: u32) -> SearchScope {
    match index {
        0 => SearchScope::Folder,
        1 => SearchScope::Subfolders,
        2 => SearchScope::Account,
        _ => SearchScope::All,
    }
}

/// One refiner chip: an operator drawn from the parsed query, so a chip's ✕ can
/// remove exactly that operator and rebuild the query string.
#[derive(Clone, PartialEq)]
pub(crate) enum Refiner {
    From(String),
    To(String),
    Cc(String),
    Subject(String),
    Body(String),
    HasAttachment,
    Unread,
    Read,
    Flagged,
    Pinned,
    Mentions,
    Category(String),
    Received(String),
    Size(String),
    Folder(String),
}

/// Every chip a parsed query should show, in a stable order.
pub(crate) fn refiners_of(query: &SearchQuery) -> Vec<Refiner> {
    let mut out = Vec::new();
    out.extend(query.from.iter().cloned().map(Refiner::From));
    out.extend(query.to.iter().cloned().map(Refiner::To));
    out.extend(query.cc.iter().cloned().map(Refiner::Cc));
    out.extend(query.subject.iter().cloned().map(Refiner::Subject));
    out.extend(query.body.iter().cloned().map(Refiner::Body));
    if query.has_attachment {
        out.push(Refiner::HasAttachment);
    }
    match query.is_unread {
        Some(true) => out.push(Refiner::Unread),
        Some(false) => out.push(Refiner::Read),
        None => {}
    }
    if query.is_flagged {
        out.push(Refiner::Flagged);
    }
    if query.is_pinned {
        out.push(Refiner::Pinned);
    }
    if query.is_mentions {
        out.push(Refiner::Mentions);
    }
    out.extend(query.category.iter().cloned().map(Refiner::Category));
    if let Some(received) = &query.received {
        out.push(Refiner::Received(received.label.clone()));
    }
    if let Some(size) = &query.size {
        out.push(Refiner::Size(size.label.clone()));
    }
    if let Some(folder) = &query.folder {
        out.push(Refiner::Folder(folder.clone()));
    }
    out
}

/// The chip's label, using Outlook's wording (UX-02, UX-03).
pub(crate) fn refiner_label(refiner: &Refiner) -> String {
    match refiner {
        Refiner::From(value) => format!("From: {value}"),
        Refiner::To(value) => format!("To: {value}"),
        Refiner::Cc(value) => format!("Cc: {value}"),
        Refiner::Subject(value) => format!("Subject: {value}"),
        Refiner::Body(value) => format!("Body: {value}"),
        Refiner::HasAttachment => "Has files".to_string(),
        Refiner::Unread => "Unread".to_string(),
        Refiner::Read => "Read".to_string(),
        Refiner::Flagged => "Flagged".to_string(),
        Refiner::Pinned => "Pinned".to_string(),
        Refiner::Mentions => "Mentions me".to_string(),
        Refiner::Category(value) => format!("Category: {value}"),
        Refiner::Received(value) => format!("Received: {value}"),
        Refiner::Size(value) => format!("Size: {value}"),
        Refiner::Folder(value) => format!("Folder: {value}"),
    }
}

/// A copy of `query` with `refiner`'s operator removed.
pub(crate) fn remove_refiner(query: &SearchQuery, refiner: &Refiner) -> SearchQuery {
    let mut next = query.clone();
    match refiner {
        Refiner::From(value) => next.from.retain(|item| item != value),
        Refiner::To(value) => next.to.retain(|item| item != value),
        Refiner::Cc(value) => next.cc.retain(|item| item != value),
        Refiner::Subject(value) => next.subject.retain(|item| item != value),
        Refiner::Body(value) => next.body.retain(|item| item != value),
        Refiner::HasAttachment => next.has_attachment = false,
        Refiner::Unread | Refiner::Read => next.is_unread = None,
        Refiner::Flagged => next.is_flagged = false,
        Refiner::Pinned => next.is_pinned = false,
        Refiner::Mentions => next.is_mentions = false,
        Refiner::Category(value) => next.category.retain(|item| item != value),
        Refiner::Received(_) => next.received = None,
        Refiner::Size(_) => next.size = None,
        Refiner::Folder(_) => next.folder = None,
    }
    next
}

/// Rebuilds the refiner-chip row from the active query. Each chip's ✕ rewrites
/// the search entry's text, which re-runs the search through the entry's own
/// `search-changed` path (or exits it when the last operator is gone).
pub(crate) fn refresh_search_refiners(state: &Rc<RefCell<UiState>>, header: &ListHeader) {
    while let Some(child) = header.refiner_row.first_child() {
        header.refiner_row.remove(&child);
    }
    let (active, query, entry) = {
        let st = state.borrow();
        (st.search_active, st.search_query.clone(), st.search_entry.clone())
    };
    let Some(entry) = entry else {
        return;
    };
    if active {
        for refiner in refiners_of(&SearchQuery::parse(&query)) {
            let chip = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(2)
                .css_classes(["search-refiner-chip"])
                .build();
            let refiner_chip_label = gtk::Label::new(Some(&refiner_label(&refiner)));
            // From/To/Cc refiners carry an address: blurred by privacy mode.
            if matches!(refiner, Refiner::From(_) | Refiner::To(_) | Refiner::Cc(_)) {
                crate::privacy::mark(&refiner_chip_label);
            }
            chip.append(&refiner_chip_label);
            let remove = gtk::Button::from_icon_name("window-close-symbolic");
            remove.add_css_class("flat");
            remove.set_tooltip_text(Some("Remove"));
            {
                let entry = entry.clone();
                let refiner = refiner.clone();
                remove.connect_clicked(move |_| {
                    let next = remove_refiner(&SearchQuery::parse(&entry.text()), &refiner);
                    entry.set_text(next.to_string().trim());
                });
            }
            chip.append(&remove);
            header.refiner_row.append(&chip);
        }
    }
    header.refiner_row.set_visible(header.refiner_row.first_child().is_some());
}

/// The advanced-search form behind both "Refine" buttons (UX-11, roadmap 2.3):
/// fields for the operators the query language supports. Applying it merges the
/// filled fields into whatever is already typed and rewrites the entry, which
/// re-runs the search.
pub(crate) fn build_refine_popover(state: &Rc<RefCell<UiState>>) -> gtk::Popover {
    let popover = gtk::Popover::new();
    popover.set_has_arrow(true);
    let grid = gtk::Grid::builder()
        .row_spacing(6)
        .column_spacing(8)
        .margin_top(10)
        .margin_bottom(10)
        .margin_start(10)
        .margin_end(10)
        .build();

    let from = gtk::Entry::new();
    let to = gtk::Entry::new();
    let subject = gtk::Entry::new();
    let body = gtk::Entry::new();
    let category = gtk::Entry::new();
    let size = gtk::Entry::new();
    let has_files = gtk::CheckButton::with_label("Has files");
    let unread = gtk::CheckButton::with_label("Unread");
    let flagged = gtk::CheckButton::with_label("Flagged");
    let mentions = gtk::CheckButton::with_label("Mentions me");
    let received = gtk::DropDown::from_strings(&["Any time", "Today", "Last 7 days", "This month", "Last month", "This year"]);

    let fields: [(&str, &gtk::Widget); 6] = [
        ("From", from.upcast_ref()),
        ("To", to.upcast_ref()),
        ("Subject", subject.upcast_ref()),
        ("Keywords", body.upcast_ref()),
        ("Category", category.upcast_ref()),
        ("Size", size.upcast_ref()),
    ];
    for (row, (label, widget)) in fields.iter().enumerate() {
        let field_label = gtk::Label::builder().label(*label).xalign(1.0).build();
        crate::privacy::mark_ui(&field_label);
        grid.attach(&field_label, 0, row as i32, 1, 1);
        grid.attach(*widget, 1, row as i32, 1, 1);
    }
    let row = fields.len() as i32;
    let received_label = gtk::Label::builder().label("Received").xalign(1.0).build();
    crate::privacy::mark_ui(&received_label);
    grid.attach(&received_label, 0, row, 1, 1);
    grid.attach(&received, 1, row, 1, 1);
    grid.attach(&has_files, 0, row + 1, 1, 1);
    grid.attach(&unread, 1, row + 1, 1, 1);
    grid.attach(&flagged, 0, row + 2, 1, 1);
    grid.attach(&mentions, 1, row + 2, 1, 1);

    let apply = gtk::Button::with_label("Search");
    apply.add_css_class("suggested-action");
    apply.set_halign(gtk::Align::End);
    grid.attach(&apply, 1, row + 3, 1, 1);
    popover.set_child(Some(&grid));

    {
        let popover = popover.clone();
        let state = state.clone();
        apply.connect_clicked(move |_| {
            let Some(entry) = state.borrow().search_entry.clone() else {
                return;
            };
            let mut query = SearchQuery::parse(&entry.text());
            // Field values append to whatever the query already carries.
            for (slot, value) in [
                (&mut query.from, &from),
                (&mut query.to, &to),
                (&mut query.subject, &subject),
                (&mut query.body, &body),
                (&mut query.category, &category),
            ] {
                let text = value.text().trim().to_string();
                if !text.is_empty() && !slot.contains(&text) {
                    slot.push(text);
                }
            }
            let size_text = size.text().trim().to_string();
            if !size_text.is_empty() {
                query.size = SearchQuery::parse(&format!("size:{size_text}")).size;
            }
            match received.selected() {
                1 => query.received = SearchQuery::parse("received:today").received,
                2 => query.received = SearchQuery::parse("received:last7days").received,
                3 => query.received = SearchQuery::parse("received:thismonth").received,
                4 => query.received = SearchQuery::parse("received:lastmonth").received,
                5 => query.received = SearchQuery::parse("received:thisyear").received,
                _ => {}
            }
            if has_files.is_active() {
                query.has_attachment = true;
            }
            if unread.is_active() {
                query.is_unread = Some(true);
            }
            if flagged.is_active() {
                query.is_flagged = true;
            }
            if mentions.is_active() {
                query.is_mentions = true;
            }
            entry.set_text(query.to_string().trim());
            popover.popdown();
        });
    }
    popover
}

/// search mode, dispatches the live IMAP pass on the open view, and repopulates
/// from the local FTS index across every account as each one's off-thread
/// query answers (an empty list paints immediately in the meantime). An empty
/// query is `exit_search` by another name (clearing the entry's X ends the
/// search).
pub(crate) fn start_search(state: &Rc<RefCell<UiState>>, worker: &Rc<Worker>, message_list: &MessageListModel, list_header: &ListHeader, query: &str) {
    let query = query.trim().to_string();
    debug_assert!(!query.is_empty(), "start_search is only called with a non-empty query; empty text takes the exit path");
    let scope = state.borrow().search_scope;
    {
        let mut st = state.borrow_mut();
        st.search_active = true;
        st.search_query = query.clone();
        st.mail_view = MailView::Search;
        // Drop any folders the *previous* query's live pass was waiting on.
        // An answer arriving for them is then both un-pending and (via the
        // `query != search_query` check in the event loop) for the wrong
        // query, so it's discarded either way.
        st.search_pending.clear();
        st.search_results.clear();
        // Recent-search history (roadmap 2.3): newest first, de-duplicated and
        // capped.
        st.recent_searches.retain(|existing| existing != &query);
        st.recent_searches.insert(0, query.clone());
        st.recent_searches.truncate(SEARCH_HISTORY_LIMIT);
        // Reveal the contextual Search ribbon row, remembering what to restore.
        if let Some(stack) = st.search_ribbon.clone() {
            if st.search_ribbon_prev.is_none() {
                st.search_ribbon_prev = Some(stack.visible_child_name().map(|name| name.to_string()).unwrap_or_default());
            }
            stack.set_visible_child_name("search");
        }
    }
    {
        let db = state.borrow().ui_db.clone();
        let recent = state.borrow().recent_searches.clone();
        if let Some(db) = db {
            if let Err(e) = db.set_recent_searches(&recent) {
                tracing::warn!("failed to persist recent searches: {e}");
            }
        }
    }
    refresh_search_refiners(state, list_header);
    dispatch_search_fallbacks(state, &query);
    repopulate_search_results(state, message_list);
    refresh_list_header(state, list_header);

    let receivers = search_cached_results(state, worker, &query);
    let state = state.clone();
    let message_list = message_list.clone();
    glib::spawn_future_local(async move {
        let mut seen: HashSet<(MailboxId, Uid)> = HashSet::new();
        let mut results = Vec::new();
        for rx in receivers {
            if let Ok(hits) = rx.recv().await {
                for m in hits {
                    if seen.insert((m.mailbox.clone(), m.uid)) {
                        results.push(m);
                    }
                }
            }
        }
        // The query or scope changed (or search was left entirely) while these
        // were in flight - the reply is for a search that's no longer current.
        if state.borrow().search_query != query || state.borrow().search_scope != scope {
            return;
        }
        state.borrow_mut().search_results = results;
        repopulate_search_results(&state, &message_list);
    });
}

/// Leaves full-text search: clears the query entry and restores the
/// pre-search view - the open mailbox (`MailView::Single`, repopulated from
/// its cache with a fresh sync requested) or the unified "All Inboxes" view
/// (`MailView::UnifiedInbox`, repopulated from the per-mailbox snapshots). A
/// no-op when no search is active, aside from clearing an idle (empty) entry.
pub(crate) fn exit_search(
    state: &Rc<RefCell<UiState>>,
    worker: &Rc<Worker>,
    message_list: &MessageListModel,
    message_list_stack: &gtk::Stack,
    list_header: &ListHeader,
    search_entry: &gtk::SearchEntry,
) {
    if !state.borrow().search_active {
        search_entry.set_text("");
        return;
    }
    let restored_view = {
        let mut st = state.borrow_mut();
        st.search_active = false;
        st.search_query.clear();
        st.search_results.clear();
        st.search_pending.clear();
        if st.current_mailbox.is_some() {
            MailView::Single
        } else {
            MailView::UnifiedInbox
        }
    };
    state.borrow_mut().mail_view = restored_view;
    // Restore the ribbon row the search replaced, and clear the refiner chips.
    {
        let stack = state.borrow().search_ribbon.clone();
        let previous = state.borrow_mut().search_ribbon_prev.take();
        if let Some(stack) = stack {
            match previous.filter(|name| !name.is_empty()) {
                Some(name) => stack.set_visible_child_name(&name),
                None => stack.set_visible_child_name(ribbon_stack_name("mail", "home")),
            }
        }
    }
    search_entry.set_text("");
    refresh_list_header(state, list_header);
    refresh_search_refiners(state, list_header);
    if restored_view == MailView::UnifiedInbox {
        for (account_id, inbox_id) in account_inboxes(state) {
            request_mailbox_sync(state, &account_id, &inbox_id);
        }
        let all = merge_unified_snapshots(&state.borrow().unified_snapshots);
        let (key, descending) = current_sort(state);
        message_list.repopulate(all, key, descending);
    } else {
        // Single view: paint from the mailbox's cache now so the list doesn't
        // linger on search results, then let the requested sync refresh it.
        let (account_id, mailbox_id) = {
            let st = state.borrow();
            (st.current_account.clone(), st.current_mailbox.clone())
        };
        if let Some((account_id, mailbox_id)) = account_id.zip(mailbox_id) {
            request_mailbox_sync(state, &account_id, &mailbox_id);
            if let Some(cache) = state.borrow().accounts.get(&account_id).and_then(|h| h.address_cache.clone()) {
                let epoch = state.borrow().mailbox_sync_epoch.get(&mailbox_id).copied().unwrap_or(0);
                let reply_rx = spawn_cache_read(worker, cache, {
                    let mailbox_id = mailbox_id.clone();
                    move |cache| cache.load_messages(&mailbox_id).ok()
                });
                let state = state.clone();
                let message_list = message_list.clone();
                let message_list_stack = message_list_stack.clone();
                glib::spawn_future_local(async move {
                    let Ok(Some(messages)) = reply_rx.recv().await else { return };
                    // Discard a reply for a mailbox the user has since
                    // navigated away from.
                    if state.borrow().current_mailbox.as_ref() != Some(&mailbox_id) {
                        return;
                    }
                    // Skip a stale window if the live sync landed first - see
                    // `select_mailbox`'s identical guard.
                    if state.borrow().mailbox_sync_epoch.get(&mailbox_id).copied().unwrap_or(0) != epoch {
                        return;
                    }
                    let (key, descending) = current_sort(&state);
                    message_list.repopulate(messages, key, descending);
                    // See `select_mailbox`'s identical call: without this the
                    // spinner set below (before this reply lands) only clears
                    // once the live IMAP sync's `MessagesUpdated` arrives,
                    // even though the cache already painted the real rows.
                    refresh_message_loading_state(&state, &message_list, &message_list_stack);
                });
            }
        }
    }
    refresh_message_loading_state(state, message_list, message_list_stack);
}

/// Enters the "All Inboxes" view: asks every connected account that has an
/// Inbox to sync it, and immediately repopulates the list from whatever the
/// per-mailbox snapshots already hold.
pub(crate) fn enter_unified_inbox(state: &Rc<RefCell<UiState>>, message_list: &MessageListModel, message_list_stack: &gtk::Stack) {
    let inboxes = account_inboxes(state);
    {
        let mut st = state.borrow_mut();
        st.mail_view = MailView::UnifiedInbox;
        // No single mailbox owns the view; leave these unset so a stray
        // single-mailbox `MessagesUpdated` can't match, and the startup
        // adopt-first logic can't override the unified view.
        st.current_account = None;
        st.current_mailbox = None;
        st.restore_pending = false;
        last_view::save(&st.settings, &LastSelection { unified: true, mailbox: None });
    }
    for (account_id, inbox_id) in inboxes {
        request_mailbox_sync(state, &account_id, &inbox_id);
    }
    let all = merge_unified_snapshots(&state.borrow().unified_snapshots);
    let (key, descending) = current_sort(state);
    message_list.repopulate(all, key, descending);
    refresh_message_loading_state(state, message_list, message_list_stack);
}

/// Merges the unified view's per-mailbox snapshots into a single newest-first
/// list, deduplicating by `(mailbox, uid)` - a message lives in exactly one
/// mailbox, but a mailbox can be re-synced more than once, so the same
/// `(mailbox, uid)` must never appear twice.
pub(crate) fn merge_unified_snapshots(snapshots: &HashMap<MailboxId, Vec<EmailSummary>>) -> Vec<EmailSummary> {
    let mut seen: HashSet<(MailboxId, Uid)> = HashSet::new();
    let mut merged: Vec<EmailSummary> = Vec::new();
    for messages in snapshots.values() {
        for m in messages {
            if seen.insert((m.mailbox.clone(), m.uid)) {
                merged.push(m.clone());
            }
        }
    }
    merged.sort_by(unified_merge_order);
    merged
}

/// The active sort, snapshotted so callers don't hold a `state` borrow across
/// a `repopulate_message_list` (whose splice re-enters the selection handler).
pub(crate) fn current_sort(state: &Rc<RefCell<UiState>>) -> (SortKey, bool) {
    let st = state.borrow();
    (st.sort_key, st.sort_descending)
}

/// Re-orders the visible list under the current sort. The rebuild goes through
/// `MessageListModel::repopulate`, so the selected message stays selected and
/// the reading pane doesn't re-render (and re-crossfade) the email it's
/// already showing.
pub(crate) fn resort_message_list(state: &Rc<RefCell<UiState>>, message_list: &MessageListModel) {
    let messages = message_list.all_messages();
    if messages.is_empty() {
        return;
    }
    let (key, descending) = current_sort(state);
    message_list.repopulate(messages, key, descending);
}

/// The viewer's own lowercased email addresses across every connected mail
/// account - used to evaluate the "To me" message filter.
pub(crate) fn viewer_addresses(state: &UiState) -> HashSet<String> {
    state.accounts.values().map(|handle| handle.email.to_lowercase()).collect()
}

/// The addresses and `@Name` tokens used to detect `@mentions` (M-25) for one
/// account: every sending identity's address, and as names its full display
/// name, that name's first word and the address local-part - so `@Gavin`,
/// `@Gavin Graham` and `@gavin.graham` all match. Compiled into a
/// [`lookout_core::MentionMatcher`] by the mail session.
pub(crate) fn mention_identity_lists(identities: &[lookout_core::Identity]) -> (Vec<String>, Vec<String>) {
    let mut addresses = Vec::new();
    let mut names = Vec::new();
    for identity in identities {
        addresses.push(identity.email.clone());
        let name = identity.name.trim();
        if !name.is_empty() {
            names.push(name.to_string());
            if let Some(first) = name.split_whitespace().next() {
                names.push(first.to_string());
            }
        }
        if let Some((local, _)) = identity.email.split_once('@') {
            if !local.is_empty() {
                names.push(local.to_string());
            }
        }
    }
    (addresses, names)
}
