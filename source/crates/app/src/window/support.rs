//! Small shared shell helpers: widget/icon plumbing, cache reads and the About/dialog utilities.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use gtk::glib;
use lookout_core::{AccountId, EmailSummary, Mailbox, MailboxId, MailboxRole, SystemFlagBit, Uid};
use lookout_mail::session::AccountCommand;

use crate::google_tasks::{self};
use crate::graph_account::GraphFeatureClient;
use crate::message_list::{FilterSet, MessageListModel, SortKey};
use crate::worker::Worker;

use super::*;

pub(crate) fn mailbox_account_id(mailbox: &MailboxId) -> Option<AccountId> {
    mailbox.0.split_once(':').map(|(account_id, _)| AccountId(account_id.to_string()))
}

/// Whether `mailbox` belongs to an account currently running the Graph mail
/// transport (item 5.7.6). IMAP-only capabilities use this to hide themselves
/// rather than failing (item 5.7.7); the account id is a GOA object path for
/// the only accounts that can opt in, so `mailbox_account_id`'s first-`:` rule
/// is safe here.
pub(crate) fn mailbox_uses_graph_mail(state: &UiState, mailbox: &MailboxId) -> bool {
    mailbox_account_id(mailbox)
        .and_then(|account_id| state.accounts.get(&account_id))
        .is_some_and(|handle| handle.uses_graph_mail)
}

/// The role of `mailbox` as last reported by its account's folder list, or
/// `None` when the account isn't connected (or the mailbox isn't in its
/// list). The app's first role lookup helper - the folder tree and search
/// paths inline this same `folders.iter().find(...)` walk.
pub(crate) fn mailbox_role(state: &UiState, mailbox: &MailboxId) -> Option<MailboxRole> {
    let account_id = mailbox_account_id(mailbox)?;
    state
        .accounts
        .get(&account_id)?
        .folders
        .iter()
        .find(|folder| &folder.id == mailbox)
        .map(|folder| folder.role)
}

/// Whether a message-list summary is a saved draft (item 1.12): an explicit
/// `\Draft` flag - which catches drafts surfaced outside the Drafts folder,
/// such as search copies - or a message living in the account's Drafts
/// mailbox. Used to offer the "Edit" affordance and to route Enter/double
/// click into the composer instead of the read-only message window.
pub(crate) fn summary_is_draft(state: &UiState, summary: &EmailSummary) -> bool {
    summary.flags.contains(&SystemFlagBit::Draft) || mailbox_role(state, &summary.mailbox) == Some(MailboxRole::Drafts)
}

/// Which summaries a message-row drag carries: the whole current selection
/// when the dragged row is part of one, otherwise just that row's message.
pub(crate) fn dragged_summaries(message_list: &MessageListModel, bound: &Rc<RefCell<Option<Rc<EmailSummary>>>>) -> Vec<EmailSummary> {
    let Some(row_summary) = bound.borrow().clone() else {
        return Vec::new();
    };
    let selected = message_list.selected_summaries();
    if selected.len() > 1 && selected.iter().any(|s| s.mailbox == row_summary.mailbox && s.uid == row_summary.uid) {
        selected
    } else {
        vec![(*row_summary).clone()]
    }
}

/// Handles a folder row's drop: deserializes the dragged `(mailbox, uid)`
/// list (the payload the message rows' drag source publishes as a
/// `G_TYPE_BYTES` value - a JSON list, so a drop from any other source fails
/// the parse and reads as "not ours" rather than a bogus move), keeps only
/// the messages belonging to the target folder's own account (messages can't
/// move across accounts), and issues one `MoveMessagesTo` per source
/// mailbox - grouped by `(account, mailbox)`, because a selection can span
/// folders (search results, a saved search) and each source needs its own
/// command.
///
/// The rows leave the list here, before the server round trip, exactly as
/// every other move path does: `optimistic_remove_messages` hides them and
/// stashes them for `MoveFailed`/the next sync to reconcile, so a racing
/// `MessagesUpdated` can't resurrect a message the user already moved. A
/// search keeps its own results list instead, which `MessagesUpdated`
/// deliberately never touches.
///
/// `false` when the payload isn't ours (or empty), which lets GTK try other
/// drop targets.
pub(crate) fn handle_message_drag_drop(state: &Rc<RefCell<UiState>>, message_list: Option<&MessageListModel>, target: &Mailbox, value: &glib::Value) -> bool {
    let Ok(bytes) = value.get::<glib::Bytes>() else {
        return false;
    };
    let payload: Vec<(String, u32)> = match serde_json::from_slice(bytes.as_ref()) {
        Ok(payload) => payload,
        Err(e) => {
            tracing::warn!("failed to parse message drag payload: {e}");
            return false;
        }
    };
    if payload.is_empty() {
        return false;
    }
    let mut groups: HashMap<(AccountId, MailboxId), Vec<Uid>> = HashMap::new();
    for (mailbox, uid) in payload {
        let mailbox = MailboxId(mailbox);
        let Some(account_id) = mailbox_account_id(&mailbox) else { continue };
        // Cross-account moves aren't possible over one account's session;
        // a drop on the message's own folder is a no-op the session would
        // skip anyway, so it never earns an undo entry or a toast.
        if account_id != target.account_id || mailbox == target.id {
            continue;
        }
        groups.entry((account_id, mailbox)).or_default().push(Uid(uid));
    }
    if groups.is_empty() {
        return false;
    }
    let mut inverse = Vec::new();
    let mut count = 0usize;
    for ((account_id, source), mut uids) in groups {
        uids.sort_by_key(|uid| uid.0);
        uids.dedup();
        let cmd_tx = state.borrow().accounts.get(&account_id).map(|handle| handle.cmd_tx.clone());
        let Some(cmd_tx) = cmd_tx else { continue };
        // Send first: a command that never reaches the account must not leave
        // the rows hidden with no `MoveFailed` to restore them.
        if cmd_tx
            .send_blocking(AccountCommand::MoveMessagesTo {
                mailbox: source.clone(),
                uids: uids.clone(),
                target: target.id.clone(),
            })
            .is_err()
        {
            continue;
        }
        remove_dragged_rows(state, message_list, &source, &uids);
        count += uids.len();
        inverse.push(AccountCommand::RevertMove {
            moved_from: source,
            moved_to: target.id.clone(),
            uids,
        });
    }
    if count > 0 {
        record_undo(state, &undo_move_description(MailboxRole::Custom, count), inverse);
        // The drop target is now a recent folder for Move ▸ / Copy ▸ (item 1.17).
        record_recent_folder(state, &target.id);
    }
    true
}

/// Applies a drop's local removal before its command is sent. The ordinary
/// list uses the shared optimistic-removal stash (`optimistic.rs`), which
/// both hides the rows now and keeps a racing `MessagesUpdated` from
/// repainting them until the server confirms. The search view is different:
/// `MessagesUpdated` is ignored there, and the list renders `search_results`
/// rather than a mailbox snapshot, so the matching hits are removed from
/// that result set and the list re-derived from it.
fn remove_dragged_rows(state: &Rc<RefCell<UiState>>, message_list: Option<&MessageListModel>, mailbox: &MailboxId, uids: &[Uid]) {
    let Some(message_list) = message_list else { return };
    if !state.borrow().search_active {
        optimistic_remove_messages(state, message_list, mailbox, uids);
        return;
    }
    {
        let mut st = state.borrow_mut();
        st.search_results.retain(|m| !(m.mailbox == *mailbox && uids.contains(&m.uid)));
        // The unified snapshot backs the "All Inboxes" view the user returns
        // to when the search ends; drop the hits there too so leaving search
        // doesn't briefly show a moved message again before its resync.
        if let Some(snapshot) = st.unified_snapshots.get_mut(mailbox) {
            snapshot.retain(|m| !uids.contains(&m.uid));
        }
    }
    repopulate_search_results(state, message_list);
}

/// The message-list header's mutable parts: the two title lines and the
/// favorite star. Bundled so the handful of places that change the active view
/// can refresh the header without each threading four widgets through its own
/// signature.
#[derive(Clone)]
pub(crate) struct ListHeader {
    pub(crate) folder_label: gtk::Label,
    pub(crate) account_label: gtk::Label,
    pub(crate) favorite_button: gtk::ToggleButton,
    /// Set while the star's state is being written programmatically, so the
    /// `toggled` handler can tell a refresh apart from a real user click.
    pub(crate) favorite_suppress: Rc<Cell<bool>>,
    /// The removable refiner chips shown above the list during a search
    /// (M-18, roadmap 2.3). Empty and hidden otherwise.
    pub(crate) refiner_row: gtk::Box,
    /// The Focused / Other tabs (M-03 / item 5.4), shown only while an Inbox of
    /// an account that provides Focused Inbox is open.
    pub(crate) focused_box: gtk::Box,
    /// The Focused tab button, kept so a refresh can re-select it when the tabs
    /// (re)appear.
    pub(crate) focused_button: gtk::ToggleButton,
    /// Whether the tabs were shown at the last refresh, so the model's view is
    /// reset only on a transition and a user's tab choice survives a repaint.
    pub(crate) focused_available: Rc<Cell<bool>>,
    /// The list model the tabs re-filter.
    pub(crate) message_list: MessageListModel,
}

/// The header's two title lines for the active view: the open folder's display
/// name over its owning account. The unified view has no single mailbox behind
/// it, so it names itself and the whole account set.
pub(crate) fn current_view_title(state: &Rc<RefCell<UiState>>) -> (String, String) {
    let st = state.borrow();
    if matches!(st.mail_view, MailView::Search) {
        return ("Search results".to_string(), format!("“{}”", st.search_query));
    }
    if matches!(st.mail_view, MailView::UnifiedInbox) {
        return ("All Inboxes".to_string(), "All accounts".to_string());
    }
    let Some(mailbox_id) = st.current_mailbox.as_ref() else {
        return ("Inbox".to_string(), String::new());
    };
    let account_id = st.current_account.clone().or_else(|| mailbox_account_id(mailbox_id));
    let handle = account_id.as_ref().and_then(|id| st.accounts.get(id));
    // Folders can arrive after the selection does (a restored view names a
    // mailbox its account hasn't listed yet), so fall back to the id's path
    // segment rather than showing nothing.
    let role = mailbox_role(&st, mailbox_id).unwrap_or(MailboxRole::Custom);
    let folder = handle
        .and_then(|h| h.folders.iter().find(|m| &m.id == mailbox_id))
        .map(crate::settings::folder_display_name)
        .or_else(|| mailbox_id.0.split_once(':').map(|(_, path)| crate::settings::folder_display_name_of(path, role)))
        .unwrap_or_else(|| crate::settings::folder_display_name_of(&mailbox_id.0, role));
    let account = handle
        .map(|h| if h.display_name.is_empty() { h.email.clone() } else { h.display_name.clone() })
        .unwrap_or_default();
    (folder, account)
}

/// Renders the header for whatever the message list is currently showing:
/// title lines, plus the star's pressed state and whether it's actionable at
/// all (the unified view spans every account, so there's no single folder to
/// favorite).
pub(crate) fn refresh_list_header(state: &Rc<RefCell<UiState>>, header: &ListHeader) {
    let (folder, account) = current_view_title(state);
    header.folder_label.set_label(&folder);
    header.account_label.set_label(&account);
    header.account_label.set_visible(!account.is_empty());

    let (favorable, starred) = {
        let st = state.borrow();
        // Search results span folders (and accounts), so the star - which
        // favorites the *open folder* - is meaningless while searching.
        match (st.mail_view, st.current_mailbox.as_ref()) {
            (MailView::Search, _) => (false, false),
            (_, Some(mailbox)) => (true, st.favorites.contains(mailbox)),
            (_, None) => (false, false),
        }
    };
    header.favorite_suppress.set(true);
    header.favorite_button.set_sensitive(favorable);
    header.favorite_button.set_active(starred);
    apply_favorite_visual(&header.favorite_button, starred);
    header.favorite_suppress.set(false);

    // Focused / Other tabs (M-03 / item 5.4): offered only while an Inbox of an
    // account that provides Focused Inbox is open, and only reset on a
    // transition so a user's tab choice survives an unrelated repaint.
    let focused_available = {
        let st = state.borrow();
        let account = match (st.mail_view, st.current_mailbox.as_ref()) {
            (MailView::Search, _) => None,
            (_, Some(mailbox)) => mailbox_account_id(mailbox),
            (_, None) => None,
        };
        let inbox = account.as_ref().is_some_and(|account_id| {
            st.current_mailbox.as_ref().is_some_and(|mailbox| {
                st.accounts
                    .get(account_id)
                    .and_then(|handle| handle.folders.iter().find(|folder| folder.id == *mailbox))
                    .is_some_and(|folder| folder.role == lookout_core::MailboxRole::Inbox)
            })
        });
        inbox && account.as_ref().is_some_and(|account_id| focused_inbox_available(&st, account_id))
    };
    if focused_available != header.focused_available.get() {
        header.focused_available.set(focused_available);
        header.focused_box.set_visible(focused_available);
        if focused_available {
            header.focused_button.set_active(true);
            header.message_list.set_focused(crate::message_list::FocusedView::Focused);
        } else {
            header.message_list.set_focused(crate::message_list::FocusedView::All);
        }
    }
}

/// The bundled bytes of an SVG resource, preferring the GResource bundle
/// (see `resources.rs`) and falling back to `include_bytes!` constants for
/// builds whose bundle couldn't be compiled.
pub(crate) fn svg_bytes(resource_path: &str, fallback: &'static [u8]) -> glib::Bytes {
    crate::resources::bytes(resource_path).unwrap_or_else(|| glib::Bytes::from_static(fallback))
}

/// Decodes a bundled SVG into a fixed-size `gtk::Image`. The bundled copy is
/// preferred (see `resources.rs`); `fallback` covers builds whose GResource
/// bundle couldn't be compiled.
pub(crate) fn svg_image(resource_path: &str, fallback: &'static [u8], pixel_size: i32) -> gtk::Image {
    let bytes = svg_bytes(resource_path, fallback);
    let texture = gtk::gdk::Texture::from_bytes(&bytes).expect("bundled SVG should decode");
    let image = gtk::Image::from_paintable(Some(&texture));
    image.set_pixel_size(pixel_size);
    image
}

/// Decodes a nav-rail SVG into a fixed-size `gtk::Image` for the view
/// buttons, which use full-colour artwork rather than theme icon names.
pub(crate) fn nav_rail_image(resource_path: &str, fallback: &'static [u8]) -> gtk::Image {
    svg_image(resource_path, fallback, 28)
}

/// Keeps the favorite star's icon and tooltip in step with its pressed state.
pub(crate) fn apply_favorite_visual(button: &gtk::ToggleButton, starred: bool) {
    let icon = if starred {
        themed_icon_name(&["starred-symbolic", "mail-mark-important-symbolic"])
    } else {
        themed_icon_name(&["non-starred-symbolic", "starred-symbolic", "mail-mark-important-symbolic"])
    };
    button.set_icon_name(icon);
    button.set_tooltip_text(Some(if starred { "Remove from Favorites" } else { "Add to Favorites" }));
}

/// The Pin/Unpin glyph for a given "is this pinned" state - the bundled
/// `pinned-1`/`unpinned-1` icons (filled/outline pin shapes, `data/resources/
/// icons/`), falling back to the generic themed pin icon (and then a
/// universally-available one) if the resource bundle isn't compiled in (see
/// `resources.rs`'s "best-effort" bundle doc comment). In Fluent mode the
/// pack's symbolic pins replace that full-colour artwork, because GTK only
/// recolours names ending in `-symbolic`.
pub(crate) fn pin_icon_name(pinned: bool) -> &'static str {
    if crate::icon_style::is_fluent() {
        return if pinned { "view-pin-symbolic" } else { "pin-off-symbolic" };
    }
    if pinned {
        themed_icon_name(&["pinned-1", "view-pin-symbolic", "mail-mark-important-symbolic"])
    } else {
        themed_icon_name(&["unpinned-1", "view-pin-symbolic", "mail-mark-important-symbolic"])
    }
}

/// Sets `button`'s icon to reflect `pinned`, swapping between the filled and
/// outline pin glyphs - the Pin/Unpin counterpart of `apply_favorite_visual`'s
/// icon-swap, used at every Pin/Unpin toggle site (bind-time, both click
/// handlers, and `refresh_pin_button` below).
pub(crate) fn set_pin_button_state(button: &gtk::Button, pinned: bool) {
    button.set_icon_name(pin_icon_name(pinned));
}

/// The CSS classes for a message or conversation row's sender label: the
/// read/unread weight plus the privacy blur's sensitive tag. The row's bind
/// replaces the label's whole class list with these, so the tag `connect_setup`
/// added has to be part of it or the partial blur never reaches the sender.
pub(crate) fn sender_label_classes(unread: bool) -> &'static [&'static str] {
    if unread {
        &["message-sender-unread", crate::privacy::SENSITIVE_CLASS]
    } else {
        &["message-sender-read", crate::privacy::SENSITIVE_CLASS]
    }
}

/// The message-list importance glyph + tooltip for a message's Outlook
/// importance (`!` for high, `↓` for low). `Normal` has no glyph and is
/// never shown; the caller gates on that.
pub(crate) fn importance_icon_and_tooltip(importance: lookout_core::Importance) -> (&'static str, &'static str) {
    match importance {
        lookout_core::Importance::High => ("emblem-important-symbolic", "High importance"),
        lookout_core::Importance::Low => ("go-down-symbolic", "Low importance"),
        lookout_core::Importance::Normal => ("emblem-important-symbolic", "Normal importance"),
    }
}

/// Sets `pin_button`'s active state to reflect whether the selected
/// message(s) are pinned (`\Flagged`) - active only when every selected
/// message is pinned (including nothing selected, which reads as inactive).
/// Mirrors `apply_favorite_visual`'s icon-swap convention.
///
/// Pin has no Graph counterpart (item 5.7.7): when any selected message is on
/// the Graph transport the button is disabled with a tooltip saying so, rather
/// than offering an action the session cannot persist.
pub(crate) fn refresh_pin_button(icon: &crate::ribbon::RibbonIcon, message_list: &MessageListModel, state: &Rc<RefCell<UiState>>) {
    let summaries = message_list.selected_summaries();
    let pinned = !summaries.is_empty() && summaries.iter().all(|s| s.is_pinned());
    let supported = {
        let st = state.borrow();
        summaries.iter().all(|summary| !mailbox_uses_graph_mail(&st, &summary.mailbox))
    };
    icon.set_icon_name(pin_icon_name(pinned));
    icon.set_sensitive(supported);
    icon.set_tooltip_text(Some(if supported { "Pin/Unpin" } else { "Pin isn't available on Graph mail" }));
}

/// The Mark as read / Mark as unread icon+label for what clicking would do -
/// full-opacity icons only (the stock `mail-read-symbolic` has
/// `fill-opacity: 0.5` baked into its own SVG, which reads as disabled next
/// to this toolbar's other solid icons): a checkmark when `mark_read_action`
/// (clicking marks read), an envelope otherwise (clicking marks unread).
/// Shared by `refresh_mark_read_button` (toolbar, selection-driven) and the
/// message row's own hover-quickmenu button (single-message, bind-driven).
pub(crate) fn mark_read_button_icon(mark_read_action: bool) -> (&'static str, &'static str) {
    if mark_read_action {
        (
            themed_icon_name(&["mail-mark-read-symbolic", "object-select-symbolic", "emblem-ok-symbolic"]),
            "Mark as read",
        )
    } else {
        (
            themed_icon_name(&["mail-mark-unread-symbolic", "mail-unread-symbolic", "emblem-ok-symbolic"]),
            "Mark as unread",
        )
    }
}

/// Sets `mark_read_button`'s icon/tooltip to reflect what clicking it would
/// currently do, from the message list's live selection: a checkmark when
/// any selected message is unread (clicking marks everything read), an
/// envelope when the selection is already all read (clicking marks
/// everything unread) - matching the same aggregate direction the click
/// handler itself computes. Mirrors `apply_favorite_visual`'s icon-swap
/// convention.
pub(crate) fn refresh_mark_read_button(icon: &crate::ribbon::RibbonIcon, message_list: &MessageListModel) {
    let summaries = message_list.selected_summaries();
    let mark_read = summaries.is_empty() || summaries.iter().any(|s| s.is_unread());
    let (icon_name, label) = mark_read_button_icon(mark_read);
    icon.set_icon_name(icon_name);
    icon.set_label(label);
    icon.set_tooltip_text(Some(label));
}

/// The first of `candidates` the current icon theme actually has. Icon names
/// in this app resolve against the machine's theme rather than a bundled set
/// (see `folder_icon_name`), and the header's sort/filter/pin icons are names
/// this codebase hasn't used before - so fall back to one that's already
/// proven in-tree instead of rendering a "missing image" box. The last
/// candidate is used unconditionally if none match.
pub(crate) fn themed_icon_name(candidates: &[&'static str]) -> &'static str {
    let theme = gtk::gdk::Display::default().map(|display| gtk::IconTheme::for_display(&display));
    if let Some(theme) = theme {
        for name in candidates {
            if theme.has_icon(name) {
                return name;
            }
        }
    }
    candidates.last().copied().unwrap_or("image-missing-symbolic")
}

/// The sort-direction toggle's icon for the current order.
pub(crate) fn sort_direction_icon(descending: bool) -> &'static str {
    if descending {
        themed_icon_name(&["view-sort-descending-symbolic", "go-down-symbolic"])
    } else {
        themed_icon_name(&["view-sort-ascending-symbolic", "go-up-symbolic"])
    }
}

/// The sort-direction toggle's tooltip: key-specific wording (A-Z, largest
/// first, …) rather than the old date-only "Newest first".
pub(crate) fn sort_direction_tooltip(key: SortKey, descending: bool) -> &'static str {
    if descending {
        key.descending_label()
    } else {
        key.ascending_label()
    }
}

/// The filter button's label: "Filter" when no filters are active, otherwise
/// the active count, so the narrowed list is obvious at a glance.
pub(crate) fn filter_button_label(set: FilterSet) -> String {
    let count = set.count();
    if count == 0 {
        "Filter".to_string()
    } else {
        format!("Filter ({count})")
    }
}

/// Runs `query` against `cache` on the `Worker`'s blocking thread pool and
/// returns a receiver for the result, instead of running it inline on the
/// GTK thread. Mirrors the mail session's own `cache_op` helper for the
/// blocking-pool dispatch, and `contacts_view`'s `dispatch_contact_command`
/// for the bounded(1)-reply-channel shape that gets the value back without
/// blocking the main loop: the caller sends nothing and blocks on nothing,
/// it `.await`s `reply_rx.recv()` inside a `glib::spawn_future_local`.
///
/// A dropped reply (the query panicked, or the runtime is shutting down)
/// simply closes the channel - callers already treat a closed/empty receiver
/// as "no result," the same as a cache miss.
pub(crate) fn spawn_cache_read<T: Send + 'static>(
    worker: &Worker,
    cache: Arc<lookout_mail::Cache>,
    query: impl FnOnce(&lookout_mail::Cache) -> T + Send + 'static,
) -> async_channel::Receiver<T> {
    let (reply_tx, reply_rx) = async_channel::bounded(1);
    worker.spawn(async move {
        if let Ok(result) = tokio::task::spawn_blocking(move || query(&cache)).await {
            let _ = reply_tx.send(result).await;
        }
    });
    reply_rx
}

/// The account's shared calendar-cache slot, created lazily. Cheap: the cache
/// itself is only opened on the worker's blocking pool when a helper below
/// actually needs it.
pub(crate) fn calendar_cache_slot(state: &Rc<RefCell<CalendarUiState>>, account_id: &AccountId) -> Arc<CalendarCacheSlot> {
    state.borrow_mut().calendar_cache_slots.entry(account_id.clone()).or_default().clone()
}

/// Writes one month of a non-CalDAV backend's occurrences into the calendar
/// cache, so event search (CAL-22) sees Microsoft 365 and any other
/// app-supplied source. CalDAV and webcal sessions write their own cache in
/// `sync_month`; this is the app-side path for Graph, whose session has no
/// cache of its own. Best-effort: a failure only costs search coverage for
/// that month, never the live view. A store superseded by a newer refresh of
/// the same month is dropped, so the older snapshot can never be the one left
/// indexed (see `CalendarCacheSlot`).
pub(crate) fn spawn_calendar_cache_store(
    worker: &Worker,
    slot: Arc<CalendarCacheSlot>,
    account_id: AccountId,
    month: chrono::NaiveDate,
    occurrences: Vec<lookout_core::EventOccurrence>,
) {
    // The generation is taken here, on the GTK thread, so call order is the
    // order that decides which snapshot wins - not whichever blocking task
    // happens to acquire the write lock first.
    let generation = slot.begin_store(month);
    worker.spawn(async move {
        let warn_id = account_id.clone();
        let result = tokio::task::spawn_blocking(move || match slot.open(&account_id) {
            Some(cache) => slot.store_if_newest(&cache, month, generation, &occurrences),
            None => Ok(false),
        })
        .await;
        match result {
            Ok(Ok(true)) => {}
            Ok(Ok(false)) => tracing::debug!("skipping a superseded calendar cache store for {warn_id} {month}"),
            Ok(Err(e)) => tracing::warn!("failed to cache {warn_id}'s calendar month {month} for search: {e}"),
            Err(e) => tracing::warn!("calendar cache write for {warn_id} panicked: {e}"),
        }
    });
}

/// Searches the account's shared calendar cache on the `Worker`'s blocking
/// thread pool and returns the matching occurrences - the calendar
/// counterpart of [`spawn_cache_read`]. Reuses the slot's already-open
/// connection rather than opening the database (WAL pragmas, DDL and the
/// schema check) on every keystroke.
pub(crate) fn spawn_calendar_cache_search(
    worker: &Worker,
    slot: Arc<CalendarCacheSlot>,
    account_id: AccountId,
    terms: Vec<String>,
) -> async_channel::Receiver<Vec<lookout_dav::CachedEventHit>> {
    let (reply_tx, reply_rx) = async_channel::bounded(1);
    let warn_id = account_id.clone();
    worker.spawn(async move {
        let result = tokio::task::spawn_blocking(move || match slot.open(&account_id) {
            Some(cache) => cache.search_events(&terms, CALENDAR_SEARCH_ROW_LIMIT),
            None => Ok(Vec::new()),
        })
        .await;
        let hits = match result {
            Ok(Ok(hits)) => hits,
            Ok(Err(e)) => {
                tracing::warn!("calendar search failed for {warn_id}: {e}");
                Vec::new()
            }
            Err(e) => {
                tracing::warn!("calendar search for {warn_id} panicked: {e}");
                Vec::new()
            }
        };
        let _ = reply_tx.send(hits).await;
    });
    reply_rx
}

/// The months in `months` a calendar cache has never stored - the backfill's
/// "don't re-fetch what we already indexed" check, run off the GTK thread.
pub(crate) fn spawn_calendar_cache_missing_months(
    worker: &Worker,
    slot: Arc<CalendarCacheSlot>,
    account_id: AccountId,
    months: Vec<chrono::NaiveDate>,
) -> async_channel::Receiver<Vec<chrono::NaiveDate>> {
    let (reply_tx, reply_rx) = async_channel::bounded(1);
    let warn_id = account_id.clone();
    worker.spawn(async move {
        let fallback = months.clone();
        let result = tokio::task::spawn_blocking(move || match slot.open(&account_id) {
            Some(cache) => cache.missing_months(&months),
            None => Ok(months),
        })
        .await;
        let missing = match result {
            Ok(Ok(missing)) => missing,
            Ok(Err(e)) => {
                tracing::warn!("calendar backfill check failed for {warn_id}: {e}");
                fallback
            }
            Err(e) => {
                tracing::warn!("calendar backfill check for {warn_id} panicked: {e}");
                fallback
            }
        };
        let _ = reply_tx.send(missing).await;
    });
    reply_rx
}

/// Raw rows scanned per account before the caller dedupes recurring series.
/// Generous enough for a long-running weekly series while still bounding a
/// single account's memory use.
pub(crate) const CALENDAR_SEARCH_ROW_LIMIT: usize = 2000;

/// Results shown after dedupe (one per series).
pub(crate) const CALENDAR_SEARCH_RESULT_LIMIT: usize = 200;

/// Opens a mail account's address-book cache on the `Worker`'s blocking
/// thread pool instead of synchronously on the GTK thread - the schema DDL
/// (`CREATE TABLE IF NOT EXISTS` etc.) is cheap in steady state but is still
/// a blocking file-open + SQLite call sequence, and `connect_account`/
/// `connect_other_account` don't need the result before they can proceed
/// (see `AccountHandle::address_cache`'s doc comment: "a failure here only
/// costs suggestions"). Mirrors `spawn_cache_read`'s shape, except there's no
/// existing `Cache` to query - this is what produces one in the first place.
pub(crate) fn spawn_cache_open(worker: &Worker, account_id: AccountId) -> async_channel::Receiver<Option<Arc<lookout_mail::Cache>>> {
    let (reply_tx, reply_rx) = async_channel::bounded(1);
    worker.spawn(async move {
        let warn_id = account_id.clone();
        let opened = tokio::task::spawn_blocking(move || lookout_mail::Cache::open(&account_id)).await;
        let cache = match opened {
            Ok(Ok(cache)) => Some(Arc::new(cache)),
            Ok(Err(e)) => {
                tracing::warn!("no address-book cache for {warn_id}, recipient autocomplete disabled: {e}");
                None
            }
            Err(_) => None,
        };
        let _ = reply_tx.send(cache).await;
    });
    reply_rx
}

pub(crate) fn account_label(state: &Rc<RefCell<UiState>>, account_id: &AccountId) -> String {
    state
        .borrow()
        .accounts
        .get(account_id)
        .map(|h| h.display_name.clone())
        .unwrap_or_else(|| account_id.0.clone())
}

/// The pure availability rule behind [`focused_inbox_available`]: Microsoft
/// 365 needs Graph, every other account reads the local classifier's opt-in
/// setting. Split out so the four-way matrix is testable without building a
/// Graph client.
pub(crate) fn focused_inbox_available_for(microsoft_365: bool, graph_available: bool, local_enabled: bool) -> bool {
    if microsoft_365 {
        graph_available
    } else {
        local_enabled
    }
}

/// Whether `account_id` provides a Focused/Other classification (M-03):
/// Microsoft 365 through a consented Graph client (or the Graph mail
/// transport, which owns its own Graph access), every other account through
/// `mail-focused-inbox-local` and the local classifier (item 2.12).
pub(crate) fn focused_inbox_available(state: &UiState, account_id: &AccountId) -> bool {
    let Some(handle) = state.accounts.get(account_id) else {
        return false;
    };
    let graph_available = handle.uses_graph_mail || handle.graph_mail.as_ref().is_some_and(|client| client.consented());
    focused_inbox_available_for(handle.microsoft_365, graph_available, state.settings.get_bool(crate::settings::MAIL_FOCUSED_INBOX_LOCAL))
}

/// The deduped, lowercased address union of every account's contact
/// suggestions - the known-sender set the local Focused Inbox classifier
/// receives (item 2.12). Sorted so two identical snapshots produce identical
/// vectors and the push below can skip a no-op command.
pub(crate) fn local_focus_contacts(state: &UiState) -> Vec<String> {
    let mut addresses: Vec<String> = state
        .contacts_by_account
        .values()
        .flat_map(|snapshot| snapshot.suggestions.iter())
        .map(|contact| lookout_core::normalize_address(&contact.address))
        .filter(|address| !address.is_empty())
        .collect();
    addresses.sort();
    addresses.dedup();
    addresses
}

/// Pushes the current contact set to every non-Microsoft 365 session's local
/// Focused Inbox classifier (item 2.12), skipping sessions already holding
/// the same set. Called after a contact snapshot changes; the per-account
/// opt-in itself travels separately through the Settings switch.
pub(crate) fn push_local_focus_contacts(state: &Rc<RefCell<UiState>>) {
    let (contacts, enabled) = {
        let st = state.borrow();
        (local_focus_contacts(&st), st.settings.get_bool(crate::settings::MAIL_FOCUSED_INBOX_LOCAL))
    };
    let mut st = state.borrow_mut();
    for handle in st.accounts.values_mut().filter(|handle| !handle.microsoft_365) {
        if handle.local_focus_contacts == contacts {
            continue;
        }
        handle.local_focus_contacts = contacts.clone();
        let _ = handle.cmd_tx.send_blocking(AccountCommand::ConfigureLocalFocus {
            enabled,
            contacts: contacts.clone(),
            backfill: false,
        });
    }
}

/// The snapshot `rebuild_folder_tree` takes of `UiState` before touching the
/// selection: whether to auto-select the first Inbox, then each account's
/// (id, label, folders), then the resolved starred mailboxes.
pub(crate) type FolderTreeSnapshot = (bool, Vec<(AccountId, String, Vec<Mailbox>)>, Vec<Mailbox>);

/// Exactly the data the sidebar draws - per account its id and label, and per
/// folder the id, display name, icon-selecting role, delimiter (the tree's
/// parent/child structure is derived from it), unread and total counts (the
/// row's number and its "⋯" menu heading) and local display prefs (icon
/// colour, which count shows) - plus the favorites section's membership and
/// order. Compared against the previous rebuild's
/// value to decide whether a `FoldersUpdated` needs a rebuild at all.
///
/// Deliberately *not* the whole `Mailbox`: a STATUS refresh writes `uidnext`
/// and `uidvalidity` on every pass, and including fields the tree never
/// renders would make the guard fire on changes that can't be seen.
pub(crate) type FolderTreeSignature = Vec<(AccountId, String, Vec<FolderRowSignature>)>;
pub(crate) type FolderRowSignature = (MailboxId, String, MailboxRole, char, u32, u32, crate::ui_state_db::FolderPrefs);

/// Everything About Lookout has to say, on one column. Shared by Help →
/// About's dialog and the File page's About page, so the two can never drift.
///
/// `AdwAboutDialog` is deliberately not used: it is a navigation widget whose
/// main page carries only the icon, name, developer and version, filing
/// `comments` behind a "Details" row and the licence behind a "Legal" row.
/// Those subpages are structural - no builder property flattens them - so the
/// handful of facts below are laid out directly.
pub(crate) fn about_content() -> gtk::Box {
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .halign(gtk::Align::Center)
        .margin_top(24)
        .margin_bottom(24)
        .margin_start(24)
        .margin_end(24)
        .build();

    content.append(&svg_image(
        "/io/github/gavindi/Lookout/icons/io.github.gavindi.Lookout.svg",
        include_bytes!("../../../../data/icons/hicolor/scalable/apps/io.github.gavindi.Lookout.svg"),
        128,
    ));
    let about_title = gtk::Label::builder().label("Lookout").css_classes(["title-1"]).build();
    crate::privacy::mark_ui(&about_title);
    content.append(&about_title);
    let version = gtk::Label::builder()
        .label(format!("Version {}", env!("CARGO_PKG_VERSION")))
        .css_classes(["dim-label"])
        .build();
    crate::privacy::mark_ui(&version);
    content.append(&version);
    let author = gtk::Label::builder().label("Gavin Graham").css_classes(["dim-label"]).build();
    crate::privacy::mark_ui(&author);
    content.append(&author);
    let description = gtk::Label::builder()
        .label("A native GNOME mail client for GNOME Online Accounts.")
        .wrap(true)
        .justify(gtk::Justification::Center)
        .margin_top(12)
        .build();
    crate::privacy::mark_ui(&description);
    content.append(&description);

    let links = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .halign(gtk::Align::Center)
        .margin_top(6)
        .build();
    links.append(&gtk::LinkButton::with_label("https://lookout-mail.com/", "Website"));
    links.append(&gtk::LinkButton::with_label("https://github.com/gavindi/lookout/issues", "Report an Issue"));
    content.append(&links);

    // The former "Legal" subpage, inlined. Markup is on for the licence link,
    // so the copyright line's ampersand has to be escaped.
    let license = gtk::Label::builder()
        .label(
            "© 2026 Gavin Graham &amp; Contributors\n\
             Licensed under the <a href=\"https://www.gnu.org/licenses/gpl-3.0.html\">GNU General Public \
             License, version 3 or later</a>. This application comes with absolutely no warranty.",
        )
        .use_markup(true)
        .wrap(true)
        .justify(gtk::Justification::Center)
        .css_classes(["caption", "dim-label"])
        .margin_top(12)
        .build();
    crate::privacy::mark_ui(&license);
    content.append(&license);

    content
}

/// Help → "About Lookout": [`about_content`] in a small modal dialog.
pub(crate) fn show_about_dialog() {
    let dialog = adw::Dialog::builder().title("About Lookout").content_width(400).build();

    let header = adw::HeaderBar::builder().show_title(false).build();

    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .propagate_natural_height(true)
        .child(&about_content())
        .build();

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&header);
    toolbar_view.set_content(Some(&scroller));
    dialog.set_child(Some(&toolbar_view));

    dialog.present(gtk::Window::NONE);
}

/// Config → Google Tasks → "OAuth client id": a small modal dialog with an
/// entry prefilled from the current configuration. Saving writes the config
/// file (an environment variable still takes precedence at runtime, but the
/// file is what a GUI app can actually manage); clearing removes the file.
pub(crate) fn show_google_tasks_client_id_dialog(window: &adw::ApplicationWindow, toast_overlay: &adw::ToastOverlay, row: &adw::ActionRow) {
    let dialog = adw::Window::builder()
        .transient_for(window)
        .modal(true)
        .title("Google Tasks OAuth client id")
        .default_width(600)
        .build();

    let help = gtk::Label::builder()
        .label(
            "Register a Desktop OAuth client in Google Cloud (console.cloud.google.com → APIs & Services → Credentials) \
             and paste its client id here. The sign-in step needs the client's redirect to http://localhost.",
        )
        .css_classes(["dim-label", "caption"])
        .wrap(true)
        .xalign(0.0)
        .build();
    crate::privacy::mark_ui(&help);
    let entry = gtk::Entry::builder().placeholder_text("e.g. 1234567890-abc.apps.googleusercontent.com").build();
    entry.set_text(&google_tasks::configured_client_id());

    let cancel_button = gtk::Button::with_label("Cancel");
    let save_button = gtk::Button::with_label("Save");
    save_button.add_css_class("suggested-action");
    let top_bar = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).css_classes(["toolbar"]).build();
    top_bar.append(&cancel_button);
    top_bar.append(&save_button);

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(10)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    content.append(&help);
    content.append(&entry);

    let toolbar_view = adw::ToolbarView::new();
    toolbar_view.add_top_bar(&top_bar);
    toolbar_view.set_content(Some(&content));
    dialog.set_content(Some(&toolbar_view));

    {
        let dialog = dialog.clone();
        cancel_button.connect_clicked(move |_| dialog.close());
    }
    {
        let dialog = dialog.clone();
        let row = row.clone();
        let toast_overlay = toast_overlay.clone();
        save_button.connect_clicked(move |_| {
            let id = entry.text().trim().to_string();
            if id.is_empty() {
                let _ = std::fs::remove_file(google_tasks::client_id_file_path());
                toast_overlay.add_toast(adw::Toast::new("Google Tasks client id cleared"));
            } else if let Err(e) = google_tasks::set_client_id(&id) {
                let title = glib::markup_escape_text(&format!("Couldn't save client id: {e}"));
                toast_overlay.add_toast(adw::Toast::new(&title));
                return;
            } else {
                toast_overlay.add_toast(adw::Toast::new("Google Tasks client id saved"));
            }
            crate::config_view::refresh_google_tasks_client_row(&row);
            dialog.close();
        });
    }

    dialog.present();
}
