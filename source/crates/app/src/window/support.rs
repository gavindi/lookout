//! Small shared shell helpers: pane CSS, widget/icon plumbing, cache reads and the About/dialog utilities.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use gtk::{gio, glib};
use lookout_core::{display_name, AccountId, EmailSummary, Mailbox, MailboxId, MailboxRole, SystemFlagBit, Uid};
use lookout_mail::session::AccountCommand;

use crate::google_tasks::{self};
use crate::message_list::{FilterSet, MessageListModel, SortKey};
use crate::worker::Worker;

use super::*;

/// Strips `Gtk.Paned`'s default visible grey separator line - the card
/// margins already provide a visual gap between panes (see `card_section`),
/// so a painted handle on top of that just looks like a stray line. The
/// handle keeps a comfortable draggable hit-area (`min-width`/`min-height`);
/// only its painted background/border is removed.
///
/// Also gives the folder card (`.folder-pane`, see `card_section`'s caller)
/// a 50%-alpha black background instead of libadwaita's normal opaque
/// `.card` fill, so the window background image shows through it.
pub(crate) fn install_paned_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(
        "paned > separator {
            background: none;
            border: none;
            box-shadow: none;
            min-width: 12px;
            min-height: 12px;
        }
        .folder-pane {
            background-color: @lookout-pane-bg;
        }
        /* The black widget overlaid on the window background image while the
           user dims it (Config → Appearance → 'Background dimming'): its
           opacity, set from the widget, controls how much the image darkens
           toward black. */
        .window-background-dim {
            background-color: @lookout-dim;
        }
        /* The folder rows' trailing unread count. Bold and accent-blue to
           match the message list's unread rows. Tabular figures ('tnum') so
           the digits are the same width in every row - without them the
           counts visibly jitter as they update. Single-quoted because this
           whole stylesheet is a plain Rust string literal. */
        .folder-unread-count {
            color: @lookout-unread;
            font-weight: bold;
            font-size: 0.9em;
            font-feature-settings: 'tnum';
            padding-right: 6px;
        }
        .folder-pane listview,
        .folder-pane list,
        .folder-pane scrolledwindow {
            background-color: transparent;
        }
        /* A folder or tag row accepting a message drag: a rounded accent
           outline marks the drop target while the drag hovers over it. */
        .lookout-drop-target {
            outline: 2px solid alpha(currentColor, 0.45);
            outline-offset: -2px;
            border-radius: 8px;
        }
        .reading-pane-transparent {
            background-color: transparent;
        }
        .card-flush-end {
            border-top-right-radius: 0;
            border-bottom-right-radius: 0;
        }
        .card-flush-start {
            border-top-left-radius: 0;
            border-bottom-left-radius: 0;
        }
        .seamless-paned > separator {
            min-width: 6px;
            min-height: 6px;
        }
        .window-toolbars-background {
            background-color: @lookout-toolbar-band;
            border-radius: 8px;
        }
        .window-icon-toolbar-background {
            background-color: @lookout-icon-toolbar-bg;
            border-radius: 8px;
        }
        .message-header-subject {
            font-weight: bold;
            font-size: 1.05em;
        }
        .message-subject-bar {
            background-color: @lookout-subject-bar-bg;
            border-bottom: 1px solid @lookout-subject-bar-border;
            padding: 10px 12px;
        }
        .message-action-bar {
            padding: 10px 12px;
        }
        .message-header-meta {
            opacity: 0.7;
        }
        .avatar-circle {
            border-radius: 9999px;
            color: white;
            font-weight: bold;
        }
        /* The contact photo layered over the initials. Clipped to the same
           circle as the initials background so an avatar reads as one shape
           whether it shows a photo or coloured initials. */
        .avatar-photo {
            border-radius: 9999px;
        }
        .avatar-color-0 { background-color: @lookout-avatar-0; }
        .avatar-color-1 { background-color: @lookout-avatar-1; }
        .avatar-color-2 { background-color: @lookout-avatar-2; }
        .avatar-color-3 { background-color: @lookout-avatar-3; }
        .avatar-color-4 { background-color: @lookout-avatar-4; }
        .avatar-color-5 { background-color: @lookout-avatar-5; }
        .hover-quick-actions {
            background-color: @theme_bg_color;
            border-radius: 8px;
            padding: 2px;
            box-shadow: 0 1px 4px rgba(0, 0, 0, 0.35);
        }
        button.hover-quick-action {
            background-color: @theme_bg_color;
            border-radius: 6px;
            padding: 1px;
            min-width: 22px;
            min-height: 22px;
            box-shadow: none;
        }
        button.hover-quick-action:hover {
            background-color: @theme_selected_bg_color;
            color: @theme_selected_fg_color;
        }
        .ribbon-tab {
            border-radius: 8px;
            margin: 2px 0;
        }
        .ribbon-tab:checked {
            background-color: @lookout-icon-toolbar-bg;
        }
        .ribbon-group-label {
            margin-right: 6px;
        }
        button.list-header-action {
            min-width: 24px;
            min-height: 24px;
            padding: 2px;
        }
        /* Zeroing the row's own padding is what lets the unread accent bar
           sit flush against the pane's leading edge, and flattens the
           selection highlight's inset to a full-bleed band. */
        .message-list > row {
            padding: 0;
        }
        /* The empty-state placeholder's icon (empty mail folders, the empty
           Favourites bucket): the artwork is drawn white, so it reads best at
           reduced opacity against the pane in both schemes. */
        .empty-state-icon {
            opacity: 0.5;
        }
        /* The panes' vertical spacing around items (Config → Appearance →
           'Vertical spacing'): the whole point of the setting is that every
           row kind in a pane moves together, so all the folder rows and all
           the message-list row kinds share one value per level. The `tight`
           rules are the app's original look; `spacing-medium` / `spacing-
           loose` add 5% / 20% more padding (rounded to whole pixels); in
           `spacing-loose` the folder rows get 20% more again than the
           message rows. The folder row's margin rides on the name label
           (`folder-row-label`) and unread count, so `bind` must keep those
           classes on every row (headings included) - replacing them is what
           used to leave the folder items unspaced. */
        .message-list .message-text,
        .message-list .message-thread-row,
        .folder-pane .folder-row-label,
        .folder-pane .folder-unread-count {
            margin-top: 6px;
            margin-bottom: 6px;
        }
        .message-list .message-section-header {
            margin-top: 4px;
            margin-bottom: 4px;
        }
        .message-list.spacing-medium .message-text,
        .message-list.spacing-medium .message-thread-row,
        .folder-pane.spacing-medium .folder-row-label,
        .folder-pane.spacing-medium .folder-unread-count {
            margin-top: 7px;
            margin-bottom: 7px;
        }
        .message-list.spacing-medium .message-section-header {
            margin-top: 5px;
            margin-bottom: 5px;
        }
        .message-list.spacing-loose .message-text,
        .message-list.spacing-loose .message-thread-row {
            margin-top: 10px;
            margin-bottom: 10px;
        }
        .message-list.spacing-loose .message-section-header {
            margin-top: 7px;
            margin-bottom: 7px;
        }
        .folder-pane.spacing-loose .folder-row-label,
        .folder-pane.spacing-loose .folder-unread-count {
            margin-top: 12px;
            margin-bottom: 12px;
        }
        .message-row {
            border-bottom: 1px solid @lookout-row-separator;
        }
        .attachment-row {
            padding: 4px 6px;
            border-radius: 6px;
        }
        .attachment-row:hover {
            background-color: @theme_selected_bg_color;
            color: @theme_selected_fg_color;
        }
        .message-accent-bar {
            background-color: transparent;
        }
        .message-accent-bar.unread {
            background-color: @lookout-unread;
        }
        /* The attachment and meeting-invite indicators, dimmed so they stay
           secondary to the sender/subject text and the pin marker. */
        .message-row-icon {
            color: alpha(currentColor, 0.5);
        }
        /* Color-tag dots are painted directly in their category's preset
           colour (see `preset_dot_area`), so no per-category CSS is needed. */
        .message-tag-dot {
            border-radius: 9999px;
            background-color: transparent;
        }
        /* Recipient chips. The pill shape is what separates one recipient
           from the next at a glance - the whole point of chips over a run of
           comma-separated text. */
        .recipient-field {
            padding: 6px 10px;
        }
        .recipient-chip {
            background-color: @lookout-chip-bg;
            border: 1px solid @lookout-chip-border;
            border-radius: 999px;
            padding: 1px 2px 1px 10px;
        }
        /* A chip that doesn't parse as an address is flagged, never
           rejected - the user has to be able to see and fix it. */
        .recipient-chip.recipient-chip-invalid {
            background-color: @lookout-chip-invalid-bg;
            border-color: @lookout-chip-invalid-border;
        }
        .recipient-chip-remove {
            min-width: 18px;
            min-height: 18px;
            padding: 0;
        }
        .message-sender-unread,
        .message-subject-unread,
        .message-date-unread {
            color: @lookout-unread;
            font-weight: bold;
        }
        .message-sender-read,
        .message-date-read {
            color: @lookout-muted;
        }
        .message-subject-read {
            color: @lookout-subject;
        }
        .message-preview {
            color: @lookout-muted;
            opacity: 0.75;
            font-size: 0.95em;
        }
        .message-section-header {
            background-color: @lookout-section-header-bg;
            border-top: 1px solid @lookout-row-separator;
            border-bottom: 1px solid @lookout-row-separator;
        }
        /* Conversation headers read as a distinct row kind from both section
           headers (darker band) and messages (bordered): a subtle tint that
           also backs the expander's chevron so the thread's disclosure is
           visible against the background image. */
        .message-thread-row {
            background-color: @lookout-thread-row-bg;
            border-bottom: 1px solid @lookout-row-separator;
        }
        /* The participant-count badge: a pill so the number reads as a count,
           not a fourth text column. */
        .message-thread-count {
            font-size: 0.9em;
            background-color: @lookout-thread-count-bg;
            border-radius: 9999px;
            padding: 0 7px;
        }
        .message-column-header {
            background-color: @lookout-column-header-bg;
            border-top: 1px solid @lookout-row-separator;
            border-bottom: 1px solid @lookout-row-separator;
            padding: 4px 0;
        }
        .message-column-header label {
            color: @lookout-column-header-fg;
            font-size: 0.85em;
            font-weight: bold;
        }
        entry.header-search-entry {
            background-color: @lookout-header-search-bg;
        }
        .search-refiner-chip {
            background-color: @lookout-thread-count-bg;
            border-radius: 9999px;
            padding: 1px 4px 1px 10px;
        }
        .search-refiner-chip label {
            font-size: 0.9em;
        }
        .search-refiner-chip button {
            min-width: 18px;
            min-height: 18px;
            padding: 0;
        }",
    );
    // The folder rows' "⋯" menu: per-folder icon colours and the menu's
    // swatches (one class pair per palette colour, generated), plus the
    // total-count variant of the trailing count.
    let folder_menu_provider = gtk::CssProvider::new();
    folder_menu_provider.load_from_string(&format!(
        "{}
        /* A folder switched to its total count (Change folder count): muted
           and regular weight, so it doesn't read as unread mail. */
        .folder-unread-count.folder-count-total {{
            color: @lookout-muted;
            font-weight: normal;
        }}
        button.folder-color-swatch {{
            min-width: 22px;
            min-height: 22px;
            padding: 0;
            border-radius: 50%;
        }}
        button.folder-color-swatch.selected {{
            outline: 2px solid @theme_fg_color;
            outline-offset: 2px;
        }}",
        crate::folder_menu::palette_css()
    ));
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(&display, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
        gtk::style_context_add_provider_for_display(&display, &folder_menu_provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    }
}

pub(crate) fn mailbox_account_id(mailbox: &MailboxId) -> Option<AccountId> {
    mailbox.0.split_once(':').map(|(account_id, _)| AccountId(account_id.to_string()))
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
pub(crate) fn dragged_summaries(message_list: &MessageListModel, bound: &Rc<RefCell<Option<EmailSummary>>>) -> Vec<EmailSummary> {
    let Some(row_summary) = bound.borrow().clone() else {
        return Vec::new();
    };
    let selected = message_list.selected_summaries();
    if selected.len() > 1 && selected.iter().any(|s| s.mailbox == row_summary.mailbox && s.uid == row_summary.uid) {
        selected
    } else {
        vec![row_summary]
    }
}

/// Writes the dragged messages' raw bytes - when already in the flat-file
/// cache - to temp `.eml` files and returns a `text/uri-list` content
/// provider for them, so the drag can land in a file manager. `None` when
/// none of the messages are cached (the internal move payload still works).
/// The written paths are recorded in `temp_files` for cleanup on drag end.
pub(crate) fn write_external_drag_files(state: &Rc<RefCell<UiState>>, summaries: &[EmailSummary], temp_files: &Rc<RefCell<Vec<PathBuf>>>) -> Option<gtk::gdk::ContentProvider> {
    let state = state.borrow();
    let mut files: Vec<gio::File> = Vec::new();
    for summary in summaries {
        let Some(account_id) = mailbox_account_id(&summary.mailbox) else { continue };
        let Some(handle) = state.accounts.get(&account_id) else { continue };
        let Some(cache) = handle.address_cache.as_ref() else { continue };
        let Some(uidvalidity) = handle.folders.iter().find(|f| f.id == summary.mailbox).map(|f| f.uidvalidity) else {
            continue;
        };
        let Ok(Some(bytes)) = cache.load_raw_message(&summary.mailbox, summary.uid, uidvalidity) else {
            continue;
        };
        let path = std::env::temp_dir().join(format!("lookout-drag-{}.eml", uuid::Uuid::new_v4()));
        if std::fs::write(&path, bytes).is_err() {
            continue;
        }
        temp_files.borrow_mut().push(path.clone());
        files.push(gio::File::for_path(path));
    }
    if files.is_empty() {
        return None;
    }
    let file_list = gtk::gdk::FileList::from_array(&files);
    Some(gtk::gdk::ContentProvider::for_value(&file_list.to_value()))
}

/// Handles a folder row's drop: deserializes the dragged `(mailbox, uid)`
/// list (the payload the message rows' drag source publishes as a
/// `G_TYPE_BYTES` value - a JSON list, so a drop from any other source fails
/// the parse and reads as "not ours" rather than a bogus move), keeps only
/// the messages belonging to the target folder's own account (messages can't
/// move across accounts), and issues one `MoveMessagesTo` per source mailbox.
/// `false` when the payload isn't ours (or empty), which lets GTK try other
/// drop targets.
pub(crate) fn handle_message_drag_drop(state: &Rc<RefCell<UiState>>, target: &Mailbox, value: &glib::Value) -> bool {
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
    let mut groups: HashMap<AccountId, (MailboxId, Vec<Uid>)> = HashMap::new();
    for (mailbox, uid) in payload {
        let Some(account_id) = mailbox_account_id(&MailboxId(mailbox.clone())) else { continue };
        if account_id != target.account_id {
            continue;
        }
        let entry = groups.entry(account_id).or_insert_with(|| (MailboxId(mailbox), Vec::new()));
        entry.1.push(Uid(uid));
    }
    if groups.is_empty() {
        return false;
    }
    for (account_id, (source, uids)) in groups {
        let cmd_tx = state.borrow().accounts.get(&account_id).map(|handle| handle.cmd_tx.clone());
        let Some(cmd_tx) = cmd_tx else { continue };
        // Undo: move them back from the target folder to their source.
        record_undo(
            state,
            &undo_move_description(MailboxRole::Custom, uids.len()),
            vec![AccountCommand::RevertMove {
                moved_from: source.clone(),
                moved_to: target.id.clone(),
                uids: uids.clone(),
            }],
        );
        let _ = cmd_tx.send_blocking(AccountCommand::MoveMessagesTo {
            mailbox: source,
            uids,
            target: target.id.clone(),
        });
    }
    // The drop target is now a recent folder for Move ▸ / Copy ▸ (item 1.17).
    record_recent_folder(state, &target.id);
    true
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
    let folder = handle
        .and_then(|h| h.folders.iter().find(|m| &m.id == mailbox_id))
        .map(|m| display_name(&m.name))
        .or_else(|| mailbox_id.0.split_once(':').map(|(_, path)| display_name(path)))
        .unwrap_or_else(|| display_name(&mailbox_id.0));
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
        inbox
            && account
                .as_ref()
                .and_then(|id| st.accounts.get(id))
                .and_then(|handle| handle.graph_mail.clone())
                .is_some_and(|client| client.consented())
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
/// `resources.rs`'s "best-effort" bundle doc comment).
pub(crate) fn pin_icon_name(pinned: bool) -> &'static str {
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
pub(crate) fn refresh_pin_button(button: &gtk::Button, message_list: &MessageListModel) {
    let summaries = message_list.selected_summaries();
    let pinned = !summaries.is_empty() && summaries.iter().all(|s| s.is_pinned());
    set_pin_button_state(button, pinned);
}

/// The Mark Read/Unread icon+tooltip for what clicking would currently do -
/// full-opacity icons only (the stock `mail-read-symbolic` has
/// `fill-opacity: 0.5` baked into its own SVG, which reads as disabled next
/// to this toolbar's other solid icons): a checkmark when `mark_read_action`
/// (clicking marks read), an envelope otherwise (clicking marks unread).
/// Shared by `refresh_mark_read_button` (toolbar, selection-driven) and the
/// message row's own hover-quickmenu button (single-message, bind-driven).
pub(crate) fn mark_read_button_icon(mark_read_action: bool) -> (&'static str, &'static str) {
    if mark_read_action {
        (themed_icon_name(&["mail-mark-read-symbolic", "object-select-symbolic", "emblem-ok-symbolic"]), "Mark Read")
    } else {
        (
            themed_icon_name(&["mail-mark-unread-symbolic", "mail-unread-symbolic", "emblem-ok-symbolic"]),
            "Mark Unread",
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
pub(crate) fn refresh_mark_read_button(button: &gtk::Button, message_list: &MessageListModel) {
    let summaries = message_list.selected_summaries();
    let mark_read = summaries.is_empty() || summaries.iter().any(|s| s.is_unread());
    let (icon, tooltip) = mark_read_button_icon(mark_read);
    button.set_icon_name(icon);
    button.set_tooltip_text(Some(tooltip));
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

/// Help → "About Lookout": everything the dialog has to say, on one page.
///
/// `AdwAboutDialog` is deliberately not used here. It is a navigation widget:
/// its main page carries only the icon, name, developer and version, and files
/// `comments` behind a "Details" row and the licence behind a "Legal" row.
/// Those subpages are structural - no builder property flattens them - so the
/// handful of facts below are laid out directly in a plain `AdwDialog`, which
/// has no navigation stack at all.
pub(crate) fn show_about_dialog() {
    let dialog = adw::Dialog::builder().title("About Lookout").content_width(400).build();

    let header = adw::HeaderBar::builder().show_title(false).build();

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
    content.append(&gtk::Label::builder().label("Lookout").css_classes(["title-1"]).build());
    content.append(
        &gtk::Label::builder()
            .label(format!("Version {}", env!("CARGO_PKG_VERSION")))
            .css_classes(["dim-label"])
            .build(),
    );
    content.append(&gtk::Label::builder().label("Gavin Graham").css_classes(["dim-label"]).build());
    content.append(
        &gtk::Label::builder()
            .label("A native GNOME mail client for GNOME Online Accounts.")
            .wrap(true)
            .justify(gtk::Justification::Center)
            .margin_top(12)
            .build(),
    );

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
    content.append(
        &gtk::Label::builder()
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
            .build(),
    );

    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .propagate_natural_height(true)
        .child(&content)
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
