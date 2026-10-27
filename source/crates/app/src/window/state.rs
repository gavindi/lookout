//! Shared window state: the per-module handles, cached dialogs and helper structs that `UiState` and `CalendarUiState` carry.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use gtk::{gio, glib};
use lookout_core::calendar_protocol::{CalendarCommand, ConnectionState as CalConnectionState};
use lookout_core::{
    AccountId, BodyPart, CalendarId, CalendarInfo, CalendarTask, EmailBody, EmailSummary, EventOccurrence, EventUid, Mailbox, MailboxId, SchedulingInfo, Uid, VCard,
    WebcalSubscription,
};
use lookout_dav::subscription::SubscriptionCommand;
use lookout_goa::{GoaCalendarAccount, GoaClient};
use lookout_mail::session::AccountCommand;
use lookout_mail::SearchScope;

use crate::calendar_colors;
use crate::contacts_view::{find_contact_by_address, ContactCommand, ContactsAccountSnapshot};
use crate::google_tasks::{GoogleTasksCommand, TaskList};
use crate::last_view::LastSelection;
use crate::message_list::{SortKey, ThreadId};
use crate::ui_state_db::UiStateDb;

use super::*;

/// Per-account state the UI needs once an `AccountSession` actor is running:
/// how to send it commands, its identity (for compose "From" and toast
/// labeling), and the last folder list it reported (kept here so the
/// multi-account folder tree can be rebuilt in full from all accounts'
/// latest snapshots whenever any one of them changes).
/// `pub(crate)` because `contacts_view` reads `cmd_tx`/`email`/`display_name`
/// (and `address_cache`) off the account handles.
pub(crate) struct AccountHandle {
    /// The session's *background* command channel - syncs, moves, searches,
    /// flag changes, refreshes. FIFO with every other background command;
    /// drained only after the interactive channel (see `interactive_cmd_tx`).
    pub(crate) cmd_tx: async_channel::Sender<AccountCommand>,
    /// The session's *interactive* command channel: user-facing fetches
    /// (`FetchBody`, `FetchAttachment`, `FetchRawMessage`) that must never
    /// queue behind background work - the session drains this channel ahead
    /// of `cmd_tx` and yields background work (STATUS drains, prefetch
    /// batches, the inline new-email body fetch) when it's non-empty, so an
    /// open-message click is answered as fast as the one in-flight IMAP
    /// round trip allows.
    pub(crate) interactive_cmd_tx: async_channel::Sender<AccountCommand>,
    pub(crate) email: String,
    pub(crate) display_name: String,
    /// Connection parameters, kept for the Config view's account overview
    /// (the Config view shows how each account is configured, not just that
    /// it exists).
    pub(crate) imap_host: String,
    pub(crate) imap_port: u16,
    pub(crate) smtp_host: String,
    pub(crate) smtp_port: u16,
    pub(crate) folders: Vec<Mailbox>,
    /// Read-side handle on this account's cache, used for folder-switch/
    /// search/composer-autocomplete reads. Deliberately a second connection
    /// to the file the session writes: routing a lookup through
    /// `AccountCommand` would put every keystroke behind whatever IMAP round
    /// trip the session is mid-way through. The cache opens WAL for exactly
    /// this, and a failed open just means no suggestions. `pub(crate)` for
    /// `contacts_view`'s attendee autocomplete, which unions the mail-history
    /// caches across every connected account. `Arc` rather than `Rc`: reads
    /// off it are dispatched onto the `Worker`'s thread pool via
    /// `spawn_cache_read`, and `Cache` is already `Send + Sync` (its
    /// connection is `Mutex`-guarded), so `Arc` is what makes that possible.
    pub(crate) address_cache: Option<Arc<lookout_mail::Cache>>,
    /// Present only for Microsoft 365 accounts - lets pin/unpin mirror to
    /// Outlook's own MAPI-property pin via Graph, alongside (never instead
    /// of) the IMAP `\Flagged` toggle that remains this app's own source of
    /// truth. `None` for every other account, which is the entire guarantee
    /// that non-M365 accounts see zero behavior change from this feature -
    /// see `mirror_pin_to_graph`. An independent `MicrosoftOAuth` instance
    /// from the one wrapped in `SendWrapper` for the session actor in
    /// `connect_account` (that one is handed to the worker thread and never
    /// touched from the GTK thread again); both safely share the same
    /// on-disk token file via its atomic tmp+rename save.
    pub(crate) graph_pin: Option<Rc<crate::graph_pin::GraphPinClient>>,
    /// Present only for Microsoft 365 accounts: syncs follow-up flags with
    /// Outlook's own `flag` property through Graph (item 1.16), both ways,
    /// once the account has granted Graph access. See `graph_mail`.
    pub(crate) graph_mail: Option<Rc<crate::graph_mail::GraphMailClient>>,
    /// Present only for Microsoft 365 accounts: syncs the shared category
    /// master list with Outlook's `masterCategories` through Graph (item 2.1),
    /// once the account has granted Graph access. See `graph_categories`.
    pub(crate) graph_categories: Option<Rc<crate::graph_categories::GraphCategoriesClient>>,
    /// Present only for Microsoft 365 accounts: syncs the user's `Graph`-backend
    /// rules with Outlook's inbox `messageRules` (item 2.5), once Graph access
    /// is granted. See `graph_rules`.
    pub(crate) graph_rules: Option<Rc<crate::graph_rules::GraphRulesClient>>,
    /// Present only for Microsoft 365 accounts: reads and writes the
    /// mailbox's automatic replies (item 2.11 / M-24) through Graph, once the
    /// account has granted Graph access. See `graph_automatic_replies`.
    pub(crate) graph_automatic_replies: Option<Rc<crate::graph_automatic_replies::GraphAutomaticRepliesClient>>,
    /// Whether this account is a Microsoft 365 mailbox (including a shared
    /// mailbox using its parent's token). Focused Inbox availability branches
    /// on this: Microsoft 365 uses Graph, every other account the local
    /// classifier's opt-in setting (M-03 / item 2.12).
    pub(crate) microsoft_365: bool,
    /// The contact addresses last pushed to this account's local Focused
    /// Inbox classifier. Compared before sending another
    /// `ConfigureLocalFocus`, so a contact poll that changed nothing doesn't
    /// churn the session.
    pub(crate) local_focus_contacts: Vec<String>,
    /// Whether this account's server advertised ManageSieve at login (item
    /// 2.5), from `AccountEvent::SieveAdvertised`. Gates offering the Sieve
    /// backend for the account.
    pub(crate) sieve_advertised: bool,
    /// Whether this account's running session is the Graph mail transport
    /// rather than IMAP/SMTP (item 5.7.6 / A-06). Set from the per-account
    /// opt-in at connect time, so IMAP-only surfaces (folder move, Sieve) can
    /// hide themselves while it's on.
    pub(crate) uses_graph_mail: bool,
}

/// One GNOME Online Accounts account as discovered at startup, keeping the
/// raw GOA structs so a disabled account can be reconnected from Config
/// without re-running discovery. Each of the three services the account
/// advertises (Mail/Calendar/Contacts) is stored under its own field;
/// Google Tasks keys off the calendar entry's display name (the email).
/// `pub(crate)` because `contacts_view` inserts contacts entries into it.
#[derive(Clone)]
pub(crate) struct DiscoveredGoaAccount {
    pub display_name: String,
    pub email: String,
    pub provider_type: Option<String>,
    pub mail: Option<lookout_goa::GoaMailAccount>,
    pub calendar: Option<GoaCalendarAccount>,
    pub contacts: Option<lookout_goa::GoaContactsAccount>,
}

/// What the message list is currently showing - either a single mailbox (the
/// classic folder-selection view), the synthetic "All Inboxes" unified view
/// merging every connected account's Inbox, or full-text search results.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum MailView {
    Single,
    UnifiedInbox,
    /// Full-text search results across the local FTS index plus the live IMAP
    /// pass on the open mailbox. Only ever active with a non-empty
    /// `UiState::search_query`; exiting search restores the pre-search view.
    Search,
}

/// Every widget one message-list row owns, stashed on the `Gtk.ListItem` at
/// setup so `bind` can address them by name instead of walking the widget
/// tree with a chain of `first_child()`/`last_child()` downcasts - which the
/// two-branch row below is far too deep for.
///
/// Stored under a single `set_data` key: the `set_data`/`data` pair is
/// `unsafe` and reads back whatever type you name, so one struct under one
/// key keeps the two ends impossible to mismatch.
#[derive(Clone)]
pub(crate) struct MessageRowWidgets {
    pub(crate) header_box: gtk::Box,
    pub(crate) expander: gtk::TreeExpander,
    pub(crate) header_label: gtk::Label,
    pub(crate) message_box: gtk::Box,
    /// The message row's three-line text column (sender / subject / preview)
    /// and its single-line counterpart below. Toggled together with the
    /// conversation header's variants by [`RowDisplay::apply`].
    pub(crate) three_line: gtk::Box,
    /// The conversation-header row: an expander over the same sender /
    /// subject / date line a message row shows, plus the participant count.
    /// Built in `connect_setup` and toggled in `bind` like the other two row
    /// kinds (see the `header_box`/`message_box` notes there).
    pub(crate) thread_box: gtk::Box,
    /// The conversation header's three-line column and single-line band.
    pub(crate) thread_three_line: gtk::Box,
    pub(crate) thread_single_line: gtk::Box,
    pub(crate) thread_single_cells: Vec<(crate::message_columns::MessageColumn, gtk::Widget)>,
    pub(crate) thread_single_sender: gtk::Label,
    pub(crate) thread_single_subject: gtk::Label,
    pub(crate) thread_single_date: gtk::Label,
    pub(crate) thread_single_size: gtk::Label,
    pub(crate) thread_single_categories: gtk::Label,
    pub(crate) thread_single_to: gtk::Label,
    pub(crate) thread_single_attachment_icon: gtk::Image,
    pub(crate) thread_single_calendar_icon: gtk::Image,
    pub(crate) thread_single_mention_icon: gtk::Image,
    pub(crate) thread_single_importance_icon: gtk::Image,
    pub(crate) thread_single_pin_icon: gtk::Image,
    pub(crate) thread_single_flag_icon: gtk::Image,
    pub(crate) thread_single_toggle: gtk::Button,
    /// The `TreeListRow` this conversation header is bound to, so the
    /// single-line layout's own chevron can expand/collapse it (the
    /// three-line layout keeps the `TreeExpander` doing that).
    pub(crate) thread_row: Rc<RefCell<Option<gtk::TreeListRow>>>,
    pub(crate) thread_expander: gtk::TreeExpander,
    pub(crate) thread_sender: gtk::Label,
    /// Attachment/calendar indicator icons on the conversation-header row,
    /// sitting right after the sender column exactly like the message row's
    /// own pair.
    pub(crate) thread_attachment_icon: gtk::Image,
    pub(crate) thread_calendar_icon: gtk::Image,
    /// `@` glyph on the conversation-header row when any message in the
    /// thread mentions the user (M-25); hidden otherwise.
    pub(crate) thread_mention_icon: gtk::Image,
    /// Importance glyph on the conversation-header row (`!` high, `↓` low);
    /// hidden on normal-importance threads.
    pub(crate) thread_importance_icon: gtk::Image,
    pub(crate) thread_subject: gtk::Label,
    pub(crate) thread_count: gtk::Label,
    pub(crate) thread_pin_icon: gtk::Image,
    /// The conversation's most urgent open follow-up flag (item 1.16).
    pub(crate) thread_flag_icon: gtk::Image,
    pub(crate) thread_date: gtk::Label,
    /// Batch-select checkbox, kept in sync with the row's real selection
    /// state (see `checkbox_suppress`'s note) rather than driving it
    /// directly - clicking it and ctrl/shift-clicking the row are two inputs
    /// to the same `MultiSelection`.
    pub(crate) checkbox: gtk::CheckButton,
    /// Set while `bind()`/the model's `notify::selected` handler are the
    /// ones writing `checkbox.set_active()`, so `checkbox`'s own
    /// `connect_toggled` (installed once in `connect_setup`) can tell a
    /// programmatic sync apart from a real click and not feed it back into
    /// the selection model - the same pattern as `ListHeader::favorite_suppress`.
    pub(crate) checkbox_suppress: Rc<Cell<bool>>,
    pub(crate) accent: gtk::Box,
    /// The sender avatar: a fixed 32x32 overlay showing the contact's photo
    /// when one is known and "Show sender images" is on, else coloured
    /// initials. Replaced in place by `checkbox` while Select mode is on.
    pub(crate) avatar: gtk::Overlay,
    pub(crate) avatar_label: gtk::Label,
    pub(crate) avatar_photo: gtk::Picture,
    pub(crate) sender_label: gtk::Label,
    /// Attachment/calendar indicator icons, right after the sender column.
    /// Hidden on rows that have neither, so they take no width at all there -
    /// the same discipline as `pin_icon`, and the fixed sender column keeps
    /// their position aligned across rows.
    pub(crate) attachment_icon: gtk::Image,
    pub(crate) calendar_icon: gtk::Image,
    /// `@` glyph shown when the message body mentions the user (M-25); hidden
    /// otherwise, so it takes no width on the common row.
    pub(crate) mention_icon: gtk::Image,
    /// Importance glyph (`!` high, `↓` low), right after the sender column
    /// next to the attachment/calendar pair; hidden on normal-importance
    /// messages.
    pub(crate) importance_icon: gtk::Image,
    pub(crate) subject_label: gtk::Label,
    pub(crate) pin_icon: gtk::Image,
    /// Follow-up flag indicator (item 1.16): the due label ("Due Tue",
    /// "Overdue") and the flag (or completed check) icon. Hidden, like
    /// `pin_icon`, on rows without a flag.
    pub(crate) flag_label: gtk::Label,
    pub(crate) flag_icon: gtk::Image,
    /// The category chip strip (UX-28), a line of its own below the preview:
    /// one preset-coloured dot plus the category's name per chip, up to
    /// `MAX_CATEGORY_CHIPS`, then a `+N` chip for the rest. Hidden wholesale
    /// when the message has no categories, so an uncategorised row is exactly
    /// three lines.
    pub(crate) category_row: gtk::Box,
    pub(crate) category_chips: Vec<CategoryChip>,
    pub(crate) category_overflow: gtk::Label,
    pub(crate) date_label: gtk::Label,
    pub(crate) preview_label: gtk::Label,
    /// The message row's single-line layout (M-07 / UX-27): a horizontal band
    /// of Outlook's columns, shown instead of the three-line sender / subject
    /// / preview column when the list is wide enough (or the reading pane is
    /// bottom/off). Both variants are built once per recycled row and toggled
    /// by [`RowDisplay::apply`]; `single_cells` carries each column's cell
    /// widget so the header's column choice can hide/show it.
    pub(crate) single_line: gtk::Box,
    pub(crate) single_cells: Vec<(crate::message_columns::MessageColumn, gtk::Widget)>,
    pub(crate) single_sender: gtk::Label,
    pub(crate) single_to: gtk::Label,
    pub(crate) single_subject: gtk::Label,
    pub(crate) single_date: gtk::Label,
    pub(crate) single_size: gtk::Label,
    pub(crate) single_categories: gtk::Label,
    pub(crate) single_attachment_icon: gtk::Image,
    pub(crate) single_calendar_icon: gtk::Image,
    pub(crate) single_mention_icon: gtk::Image,
    pub(crate) single_importance_icon: gtk::Image,
    pub(crate) single_pin_icon: gtk::Image,
    pub(crate) single_flag_icon: gtk::Image,
    /// The conversation header's body preview (the newest message's snippet),
    /// on the third line of its three-line row.
    pub(crate) thread_preview_label: gtk::Label,
    /// The conversation header's category chips, from the newest message's
    /// keywords - the same strip a message row shows.
    pub(crate) thread_category_row: gtk::Box,
    pub(crate) thread_category_chips: Vec<CategoryChip>,
    pub(crate) thread_category_overflow: gtk::Label,
    pub(crate) action_box: gtk::Box,
    /// Hover-quickmenu toggle buttons - their icon/tooltip is set in `bind`
    /// from this row's message state (unlike `delete_btn`/`reply_btn`, which
    /// aren't state-dependent and so aren't stored here).
    pub(crate) mark_read_btn: gtk::Button,
    pub(crate) follow_up_btn: gtk::Button,
    pub(crate) pin_btn: gtk::Button,
    /// The message this row currently shows. Set by `bind`, read by the
    /// quick-action handlers when they fire.
    pub(crate) bound: Rc<RefCell<Option<Rc<EmailSummary>>>>,
    /// The conversation this row heads, when it's a conversation header -
    /// what its right-click menu acts on (item 1.17).
    pub(crate) bound_thread: Rc<RefCell<Option<ThreadId>>>,
}

/// One category chip on a message row (UX-28): Outlook's preset-coloured dot
/// plus the category's name. Built once per row (up to `MAX_CATEGORY_CHIPS`)
/// then recoloured and relabelled on `bind`, so scrolling a large mailbox
/// never constructs widgets.
#[derive(Clone)]
pub(crate) struct CategoryChip {
    pub(crate) root: gtk::Box,
    pub(crate) dot: TagDot,
    pub(crate) label: gtk::Label,
}

impl CategoryChip {
    pub(crate) fn new() -> Self {
        let dot = TagDot::new("#808080", 8);
        let label = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            // A long category name truncates inside its own chip rather than
            // pushing the preview or the trailing "+N" out of the row.
            .max_width_chars(16)
            .css_classes(["message-category-chip-label"])
            .build();
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(4)
            .valign(gtk::Align::Center)
            .css_classes(["message-category-chip"])
            .visible(false)
            .build();
        root.append(&dot.area);
        root.append(&label);
        CategoryChip { root, dot, label }
    }

    /// Shows this chip for `category`, or hides it when there is none.
    pub(crate) fn bind(&self, category: Option<&crate::categories::CategoryDef>) {
        match category {
            Some(category) => {
                self.dot.set_rgb(category.color());
                self.label.set_label(&category.name);
                self.root.set_tooltip_text(Some(&category.name));
                self.root.set_visible(true);
            }
            None => self.root.set_visible(false),
        }
    }
}

/// Fills one row kind's category strip: the chips show the first
/// `MAX_CATEGORY_CHIPS` categories, a trailing `+N` folds the rest away, and
/// the whole row is hidden when the message has no categories. Recomputed on
/// every bind, so a recycled row never keeps its previous occupant's chips.
pub(crate) fn bind_category_chips(row: &gtk::Box, chips: &[CategoryChip], overflow: &gtk::Label, categories: &[&crate::categories::CategoryDef]) {
    let (shown, hidden) = crate::categories::category_chip_plan(categories, MAX_CATEGORY_CHIPS);
    for (chip, category) in chips.iter().zip(shown.iter()) {
        chip.bind(Some(category));
    }
    for chip in chips.iter().skip(shown.len()) {
        chip.bind(None);
    }
    if hidden > 0 {
        let names: Vec<&str> = categories[shown.len()..].iter().map(|category| category.name.as_str()).collect();
        overflow.set_label(&format!("+{hidden}"));
        overflow.set_tooltip_text(Some(&names.join(", ")));
        overflow.set_visible(true);
    } else {
        overflow.set_visible(false);
    }
    row.set_visible(!categories.is_empty());
}

/// Weak handles to a row's display-dependent widgets - the avatar/checkbox
/// pair and the two preview labels (a message row's and a conversation
/// header's) - whose appearance follows app-wide settings (Select mode, "Show
/// sender images", the preview line count) rather than the row's own message.
/// The list view creates a bounded pool of row widgets and recycles them, so
/// the registry in `build_window` holds weak references and prunes the dead
/// ones whenever a setting changes (see `update_row_display`).
pub(crate) struct RowDisplayRefs {
    pub(crate) avatar: glib::WeakRef<gtk::Overlay>,
    pub(crate) checkbox: glib::WeakRef<gtk::CheckButton>,
    pub(crate) preview_label: glib::WeakRef<gtk::Label>,
    pub(crate) thread_preview_label: glib::WeakRef<gtk::Label>,
    /// The two message-row variants and the two conversation-header variants,
    /// so a list-style change reaches rows already on screen (see
    /// [`RowDisplay::apply`]).
    pub(crate) three_line: glib::WeakRef<gtk::Box>,
    pub(crate) single_line: glib::WeakRef<gtk::Box>,
    pub(crate) thread_three_line: glib::WeakRef<gtk::Box>,
    pub(crate) thread_single_line: glib::WeakRef<gtk::Box>,
    /// The conversation row's `TreeExpander` wraps the three-line column; it
    /// has to be hidden as a whole in the single-line layout, or its chevron
    /// would offset the columns.
    pub(crate) thread_expander: glib::WeakRef<gtk::TreeExpander>,
}

/// Configures a row's preview label for `lines` body-preview lines: 0 hides
/// it (dropping the reserved line so the row shrinks), 1 keeps the label on a
/// single ellipsized line, and 2-3 wrap and cap the label at that many lines.
pub(crate) fn apply_preview_lines(label: &gtk::Label, lines: i32) {
    let lines = crate::settings::preview_lines(lines);
    if lines <= 0 {
        label.set_visible(false);
        return;
    }
    label.set_visible(true);
    label.set_wrap(true);
    label.set_single_line_mode(false);
    label.set_lines(lines);
}

/// The app-wide settings every message row's display follows: Select mode,
/// "Show sender images" and the preview line count. Held in one shared cell
/// that `build_window` seeds from the settings and the three controls update,
/// so a row's bind reads it instead of querying GSettings per row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RowDisplay {
    pub(crate) select_mode: bool,
    pub(crate) show_sender_images: bool,
    pub(crate) preview_lines: i32,
    /// Outlook's single-line columnar row (M-07 / UX-27) instead of the
    /// three-line sender / subject / preview row.
    pub(crate) single_line: bool,
}

impl RowDisplay {
    /// The persisted settings, with Select mode off (it never persists).
    pub(crate) fn from_settings(settings: &crate::settings::SettingsStore) -> Self {
        RowDisplay {
            select_mode: false,
            show_sender_images: settings.get_bool(crate::settings::MAIL_SHOW_SENDER_IMAGES),
            preview_lines: settings.get_int(crate::settings::LAYOUT_PREVIEW_LINES),
            single_line: false,
        }
    }

    /// Applies these settings to one row: Select mode hides the avatar in
    /// favour of the checkbox, and the list style swaps the three-line text
    /// column for the single-line column band.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn apply(self, avatar: &gtk::Overlay, checkbox: &gtk::CheckButton, preview: &gtk::Label, three_line: &gtk::Box, single_line: &gtk::Box) {
        avatar.set_visible(!self.select_mode && self.show_sender_images);
        checkbox.set_visible(self.select_mode);
        apply_preview_lines(preview, self.preview_lines);
        three_line.set_visible(!self.single_line);
        single_line.set_visible(self.single_line);
    }
}

/// Applies `display` to every live row in `registry`. Dead weak references
/// are dropped on the way.
pub(crate) fn update_row_display(registry: &Rc<RefCell<Vec<RowDisplayRefs>>>, display: RowDisplay) {
    registry.borrow_mut().retain(|refs| {
        let (
            Some(avatar),
            Some(checkbox),
            Some(preview),
            Some(thread_preview),
            Some(three_line),
            Some(single_line),
            Some(thread_three_line),
            Some(thread_single_line),
            Some(thread_expander),
        ) = (
            refs.avatar.upgrade(),
            refs.checkbox.upgrade(),
            refs.preview_label.upgrade(),
            refs.thread_preview_label.upgrade(),
            refs.three_line.upgrade(),
            refs.single_line.upgrade(),
            refs.thread_three_line.upgrade(),
            refs.thread_single_line.upgrade(),
            refs.thread_expander.upgrade(),
        )
        else {
            return false;
        };
        display.apply(&avatar, &checkbox, &preview, &three_line, &single_line);
        display.apply(&avatar, &checkbox, &thread_preview, &thread_three_line, &thread_single_line);
        thread_expander.set_visible(!display.single_line);
        true
    });
}

/// The sender's contact photo as a decoded texture, memoized per address in
/// `cache`. `None` when there is no contact for the address, no photo on the
/// card, or the photo can't be decoded - all of which fall back to initials.
pub(crate) fn sender_photo_texture(state: &Rc<RefCell<UiState>>, address: &str, cache: &Rc<RefCell<HashMap<String, Option<gtk::gdk::Texture>>>>) -> Option<gtk::gdk::Texture> {
    if let Some(cached) = cache.borrow().get(address) {
        return cached.clone();
    }
    let texture = find_contact_by_address(state, address, None)
        .and_then(|(_, entry)| Rc::unwrap_or_clone(entry.record).card.photo)
        .and_then(|photo| photo.data)
        .and_then(|data| gtk::gdk::Texture::from_bytes(&glib::Bytes::from_owned(data)).ok());
    cache.borrow_mut().insert(address.to_string(), texture.clone());
    texture
}

/// Per-row widgets for the folder tree rows, stored under a `set_data` key on
/// the row's `TreeExpander` (same pattern as `MessageRowWidgets` under
/// `row-widgets`): `bind` refreshes which folder the expunge button acts on
/// and whether the row is eligible at all, and the click handler reads the
/// target at click time.
#[derive(Clone)]
pub(crate) struct FolderRowWidgets {
    pub(crate) expunge_btn: gtk::Button,
    /// Which mailbox the expunge button acts on - `None` on rows that aren't
    /// Trash/Junk (where the button never shows anyway).
    pub(crate) expunge_data: Rc<RefCell<Option<Mailbox>>>,
    /// Whether this row's folder can be expunged; shows or hides the button
    /// inside the action box.
    pub(crate) expunge_enabled: Rc<Cell<bool>>,
    /// The row's "⋯" menu (see `folder_menu`).
    pub(crate) menu: crate::folder_menu::RowMenu,
    /// Which folder the menu acts on - `None` on rows that aren't a folder
    /// (All Inboxes, account groups, the Favorites heading), which show no
    /// action box at all.
    pub(crate) menu_data: Rc<RefCell<Option<Mailbox>>>,
    /// Re-evaluates whether the action box shows: on a folder row while it's
    /// hovered or its menu is open.
    pub(crate) refresh_actions: Rc<dyn Fn()>,
}

/// How many recently-viewed message bodies to keep in memory. Every message
/// switch used to re-fetch the whole body over IMAP - there was no cache
/// beyond the single currently-open message - so flipping between emails (or
/// re-opening one already read) re-downloaded everything, network round trip
/// included. This LRU makes a revisit render instantly; `lookout-mail`'s disk
/// cache covers the cross-restart and long-session cases.
pub(crate) const BODY_CACHE_IN_MEMORY: usize = 25;

/// If WebKit hasn't reported `Finished` for a body load within this long, the
/// reading pane reveals the page anyway instead of sitting on the empty
/// placeholder. Revealing is normally gated on `Finished` so the fade-in
/// never shows a blank page, but a slow/hung load (e.g. a message referencing
/// remote resources) must not hold the pane blank indefinitely.
pub(crate) const HTML_REVEAL_TIMEOUT_MS: u64 = 400;

/// How long an uncached message body may stay in flight before the reading
/// pane gives up and shows the "message unavailable" page instead of spinning
/// forever. Generous enough that a slow link isn't mistaken for a failure; the
/// reconnect path (see `FoldersUpdated`) retries automatically once the
/// connection comes back, and the page's Retry button re-requests on demand.
pub(crate) const BODY_FETCH_TIMEOUT_MS: u64 = 30_000;

/// The same backstop as `BODY_FETCH_TIMEOUT_MS`, used when the network is
/// already known to be down: local disk-cache reads answer in milliseconds, so
/// the pane can report the outage almost immediately instead of spinning for
/// the full timeout. Long enough that a disk hit still wins the race.
pub(crate) const BODY_FETCH_OFFLINE_TIMEOUT_MS: u64 = 1_000;

/// How narrow the mail screen's folder pane may be dragged, in pixels. The
/// pane holds a `Gtk.ScrolledWindow`, which reports no meaningful minimum
/// width of its own, so without this the separator can be dragged until the
/// folder names are a sliver. Applied to `folder_card`, not the scroller -
/// see the call site.
pub(crate) const FOLDER_PANE_MIN_WIDTH: i32 = 200;

/// How wide the mail screen's folder pane may be, in pixels. The separator
/// stops here no matter how wide the window grows, so the pane never becomes
/// an unreadably wide column.
pub(crate) const FOLDER_PANE_MAX_WIDTH: i32 = 320;

/// How many times a pane-width restore retries at the same 150 ms settle
/// interval as everything else in that block - about three seconds.
///
/// Two separate things are being waited on, and the slower one sets the bound.
/// A pane that is mapped but not yet allocated only needs a frame or two. The
/// window width settling is what actually takes time: `window.maximize()` runs
/// before `present()`, so the window is first allocated at its stored
/// un-maximized width and only reaches the real maximized width once the
/// compositor has acted on the request - which is a round trip, and behind a
/// maximize animation on some desktops. Restoring against that transient width
/// is precisely the bug this bound exists to avoid, so it is generous.
pub(crate) const PANE_RESTORE_ATTEMPTS: u8 = 20;

/// Per-pane bookkeeping for what a restore last put on screen: the paned's own
/// allocated width at the time, and the position that was set against it.
///
/// This is what tells a user's drag apart from GTK's own re-clamp. GtkPaned
/// clamps its position during a layout pass whenever its allocation changes,
/// and emits `notify::position` from there - asynchronously, long after the
/// restore's `pane_widths_applying` flag has been cleared, so that flag alone
/// cannot catch it. But a clamp only ever happens *because* the allocation
/// changed, and a drag never changes it, so comparing the paned's current width
/// against the width recorded here separates the two exactly. See
/// [`pane_change_is_a_drag`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PaneRestoreState {
    /// The paned's own allocated extent along its split axis (`width()` for a
    /// horizontal paned, `height()` for a vertical one) when the position
    /// below was applied.
    pub(crate) paned_extent: i32,
    /// The position the restore set, or the clamp last observed.
    pub(crate) position: i32,
}

/// Whether a `notify::position` on a restored pane is a user's drag, and so
/// worth persisting, or GTK re-clamping the position to fit a new allocation,
/// which must never be persisted.
///
/// `None` for `recorded` means the pane has not been restored during this map
/// cycle: it is still showing its built-in default, or GTK's own
/// allocation-time clamp of that default, and neither is a width the user
/// chose. Saving then would overwrite the remembered percentage with a number
/// that never came from them.
///
/// A changed `paned_width` is the signature of a clamp - the allocation moved,
/// so GTK re-fitted the separator. The reading pane's minimum growing when a
/// wide HTML message loads does this, as does every window resize and stack
/// page switch. An unchanged `paned_width` with a moved position can only be
/// the user dragging the separator.
pub(crate) fn pane_change_is_a_drag(recorded: Option<PaneRestoreState>, paned_extent: i32, position: i32) -> bool {
    match recorded {
        Some(recorded) => recorded.paned_extent == paned_extent && recorded.position != position,
        None => false,
    }
}

/// Whether the window width has stopped changing, which is the precondition for
/// restoring a pane against it (and for marking the pane restored, which is
/// what reopens its saves).
///
/// `previous` is the width seen on the last attempt, `None` on the first. A
/// width is only trusted once two consecutive attempts agree on it: a single
/// reading cannot distinguish the final maximized width from the transient
/// un-maximized one the window is created at.
pub(crate) fn window_width_is_stable(previous: Option<i32>, current: i32) -> bool {
    current > 0 && previous == Some(current)
}

/// Where a paned's separator goes so its *start* child ends up `pct` of the
/// window's width - the maths behind every pane-width restore, kept out of the
/// GTK wiring so it can be tested without a display.
///
/// `None` means "nothing to restore right now", for three distinct reasons the
/// caller treats differently: the pane was never dragged (`pct` is the `-1`
/// GSettings default), the window has no width yet, or the paned is mapped but
/// not yet allocated (`paned_width` 0). The last one is why the restore can be
/// retried rather than collapsing the pane to its minimum - a pane that maps as
/// its tab or dialog appears has no allocation until the next layout pass.
///
/// `cap` is the folder pane's hard ceiling ([`FOLDER_PANE_MAX_WIDTH`]); every
/// other pane passes `None`. The `.max(min)` after each bound is load-bearing:
/// when the two children's minimums together exceed the paned's width (a very
/// narrow window), `max` would otherwise fall below `min` and `clamp` panics.
pub(crate) fn paned_position_for_percent(pct: f64, window_width: i32, paned_width: i32, start_min: i32, end_min: i32, cap: Option<i32>) -> Option<i32> {
    if pct <= 0.0 || window_width <= 0 || paned_width <= 0 {
        return None;
    }
    let min = start_min;
    let mut max = paned_width.saturating_sub(end_min).max(min);
    if let Some(cap) = cap {
        max = max.min(cap).max(min);
    }
    // Rounded, not truncated: the stored percentage came from dividing an
    // integer pixel width by an integer window width, so multiplying it back
    // out lands a hair under the original (232/2880 replayed against 2880 is
    // 231.99999999999997). Truncating that loses a pixel, which is then saved
    // back as a slightly smaller percentage - a pane that creeps one pixel
    // narrower on every launch.
    Some(((pct / 100.0 * window_width as f64).round() as i32).clamp(min, max))
}

/// [`paned_position_for_percent`] for a pane that is its paned's *end* child -
/// the mail tab's overview pane. Its stored percentage is the width of the pane
/// itself rather than the separator's position, so the position is derived by
/// subtracting the clamped target width from the paned's own width.
pub(crate) fn paned_position_for_end_percent(pct: f64, window_width: i32, paned_width: i32, start_min: i32, end_min: i32) -> Option<i32> {
    if pct <= 0.0 || window_width <= 0 || paned_width <= 0 {
        return None;
    }
    let max = paned_width.saturating_sub(start_min).max(end_min);
    // Rounded for the same reason as `paned_position_for_percent`.
    let target_end_width = ((pct / 100.0 * window_width as f64).round() as i32).clamp(end_min, max);
    // `.max(0)`: when the paned is narrower than the pane's own minimum there
    // is no room left for the start child at all, and a negative position is
    // not a position.
    Some(paned_width.saturating_sub(target_end_width).max(0))
}

/// What to store for a pane `child_width` pixels wide in a `window_width`
/// window. `None` when the window has no width to be a percentage of - nothing
/// is written in that case, since the percentage would be meaningless.
pub(crate) fn pane_width_percent(child_width: i32, window_width: i32) -> Option<f64> {
    if window_width <= 0 {
        return None;
    }
    Some(child_width as f64 * 100.0 / window_width as f64)
}

/// A paned's own allocated length along its split axis: the width for a
/// horizontal paned (children side by side), the height for a vertical one
/// (children stacked). The message list's paned flips to vertical when the
/// reading pane sits at the bottom, so every restore/save computation is
/// expressed against this rather than `width()` directly.
pub(crate) fn paned_axis_extent(paned: &gtk::Paned) -> i32 {
    match paned.orientation() {
        gtk::Orientation::Vertical => paned.height(),
        _ => paned.width(),
    }
}

/// The window's length along a paned's split axis - what a pane's percentage
/// is taken against (see `pane_width_percent`).
pub(crate) fn window_axis_extent(window: &gtk::Window, orientation: gtk::Orientation) -> i32 {
    match orientation {
        gtk::Orientation::Vertical => window.height(),
        _ => window.width(),
    }
}

/// The GSettings percentages a pane persists its extent under, one per split
/// axis. Every pane but the message list has a fixed orientation and passes
/// the same key twice; the message list is vertical when the reading pane is
/// at the bottom, so its width and its height are remembered separately.
#[derive(Clone, Copy)]
pub(crate) struct PaneExtentKeys {
    pub(crate) horizontal: &'static str,
    pub(crate) vertical: &'static str,
}

impl PaneExtentKeys {
    /// The keys for a pane that never changes orientation (every pane but the
    /// message list): the same percentage regardless of axis.
    pub(crate) fn fixed(key: &'static str) -> Self {
        Self { horizontal: key, vertical: key }
    }

    /// The key for the orientation the paned is in *right now*.
    pub(crate) fn for_paned(self, paned: &gtk::Paned) -> &'static str {
        match paned.orientation() {
            gtk::Orientation::Vertical => self.vertical,
            _ => self.horizontal,
        }
    }
}

/// The reading-pane dropdown's three choices, in View-ribbon and Settings
/// order. "right"/"bottom" are the two positions; "off" hides the pane.
pub(crate) const READING_PANE_CHOICES: [&str; 3] = ["right", "bottom", "off"];

/// The index of a reading-pane choice in [`READING_PANE_CHOICES`], for the
/// Settings `ComboRow`.
pub(crate) fn reading_pane_choice_index(choice: &str) -> u32 {
    READING_PANE_CHOICES.iter().position(|c| *c == choice).unwrap_or(0) as u32
}

/// Applies a reading-pane choice ("right"/"bottom"/"off") and, when the second
/// argument is true, writes it through to GSettings. Shared by the View
/// ribbon's dropdown and the Settings `ComboRow`.
pub(crate) type ApplyReadingPane = Rc<dyn Fn(&str, bool)>;

/// Applies a conversation-order choice ("newest-top"/"newest-bottom", item
/// 1.7) and, when the second argument is true, writes it through to
/// GSettings. Shared by View ▸ Messages and the Settings `ComboRow`.
pub(crate) type ApplyConversationOrder = Rc<dyn Fn(&str, bool)>;

/// Applies a density choice ("loose"/"medium"/"tight", UX-83) and, when the
/// second argument is true, writes it through to GSettings. Shared by View ▸
/// Layout ▸ Density and the Settings `ComboRow`.
pub(crate) type ApplyDensity = Rc<dyn Fn(&str, bool)>;

/// The initial state of the View ribbon's reading-pane dropdown action
/// (`win.reading-pane-position`): "off" when the reading pane is hidden,
/// otherwise the stored position ("right" or "bottom").
pub(crate) fn reading_pane_action_state(settings: &crate::settings::SettingsStore) -> String {
    if !settings.get_bool(crate::settings::LAYOUT_READING_PANE) {
        return "off".to_string();
    }
    settings.get_string(crate::settings::LAYOUT_READING_PANE_POSITION)
}

/// The `win.message-organization` state for a `mail-threaded` value: Outlook's
/// View ▸ Messages ▸ "Show as conversations" / "Show as individual messages".
pub(crate) fn message_organization_value(threaded: bool) -> &'static str {
    if threaded {
        "conversations"
    } else {
        "messages"
    }
}

/// The reading-pane dropdown's tooltip, naming its current position.
pub(crate) fn reading_pane_tooltip(settings: &crate::settings::SettingsStore) -> String {
    if !settings.get_bool(crate::settings::LAYOUT_READING_PANE) {
        return "Reading pane: Off".to_string();
    }
    match crate::settings::reading_pane_position(&settings.get_string(crate::settings::LAYOUT_READING_PANE_POSITION)) {
        crate::settings::ReadingPanePosition::Bottom => "Reading pane: Bottom".to_string(),
        crate::settings::ReadingPanePosition::Right => "Reading pane: Right".to_string(),
    }
}

/// How long after the last keystroke before a search query is committed. Long
/// enough that a burst of typing runs one search, not one per keypress - the
/// live IMAP pass costs a round trip, and even the FTS pass re-renders the
/// list - but short enough that results feel immediate.
pub(crate) const SEARCH_DEBOUNCE_MS: u32 = 300;

/// How many hits each source contributes to a search: the FTS cache pass and
/// the live IMAP pass each cap their answer so a search over a large mailbox
/// doesn't hand the list an unbounded set to rebuild from. Generous - the
/// user can refine the query to narrow further.
pub(crate) const SEARCH_RESULT_LIMIT: usize = 300;

/// How many recent queries the search box remembers (roadmap 2.3), shown in its
/// history popover and persisted in the UI-state database.
pub(crate) const SEARCH_HISTORY_LIMIT: usize = 10;

/// How many category chips one message row renders before folding the rest
/// into a trailing "+N" (UX-28). Outlook's chip strip is one line under the
/// preview; past this a heavily-categorised row would crowd out the text, so
/// the remaining names live in the "+N" chip's tooltip.
pub(crate) const MAX_CATEGORY_CHIPS: usize = 3;

/// How long a Save-button attachment fetch may take before the UI gives up
/// and restores the button. The session answers every `FetchAttachment`
/// (with `PartFetched` or `PartFetchFailed`) except when the connection dies
/// mid-fetch and the command is lost to the reconnect; this timeout is the
/// backstop that guarantees the button can never be stuck on "Fetching…"
/// forever. Generous: the fetch is a single IMAP literal, and multi-megabyte
/// attachments over slow connections legitimately take a while.
pub(crate) const ATTACHMENT_FETCH_TIMEOUT_MS: u64 = 60_000;

/// How many session events may queue before the session stalls on send. The
/// UI drains each loop's channel in batches and collapses whole-snapshot
/// events into the last copy of each (see `collapse_last_wins`), so a full
/// channel is a transient burst - and backpressure is the point: an unbounded
/// channel lets a sync storm grow memory without bound while the UI serially
/// repopulates. The UI -> session *command* channels stay unbounded on
/// purpose: commands are sent with `send_blocking` from the GTK main thread,
/// and the session can be mid-fetch for seconds, so a bounded command channel
/// would freeze the UI on click.
pub(crate) const EVENT_CHANNEL_CAPACITY: usize = 64;

/// The "Switch message theme" toggle's stylesheet, injected as a WebKit
/// *user* style sheet into the reading pane's WebView: it strips whatever
/// background colour the email itself defines so the app theme's background
/// shows through instead, and inverts the document's colours so the content
/// still reads against that (mostly darker) theme background. `html`/`body`
/// covers the page background proper (inline styles, `<style>` blocks, or
/// `<body bgcolor>`), while the attribute selectors catch the legacy
/// `bgcolor`/`background` attributes Outlook-style clients plaster onto
/// wrapper tables and cells - the common full-bleed case that an
/// `html, body` override alone would miss. Deliberate backgrounds on content
/// (code blocks, quoted sections) are left alone. The `!important` is what
/// wins: user-level `!important` outranks both author `!important` and
/// inline styles in the cascade, so no email CSS can fight the override.
/// Injected at the user level so it applies to the already loaded document
/// the moment the toggle flips (plus a re-render, see the toggle's handler),
/// and applies to every `load_html` while it's armed.
///
/// The inversion is the standard reader-mode trick: `filter: invert()`
/// flips the whole document (dark text becomes light, coloured headings and
/// links keep a readable hue via the compensating `hue-rotate`), then media
/// elements are inverted a second time so photos and inline graphics render
/// as themselves instead of negatives. Because the email's own backgrounds
/// are gone above, only the text and any remaining accent colours invert -
/// against the app theme's background, which is untouched by the page's
/// filter.
pub(crate) const MESSAGE_THEME_OVERRIDE_CSS: &str = "\
html, body {
    background-color: transparent !important;
    background-image: none !important;
}
*[bgcolor] {
    background-color: transparent !important;
}
*[background] {
    background-image: none !important;
}
html {
    filter: invert(1) hue-rotate(180deg);
}
html img, html picture, html video, html svg, html canvas {
    filter: invert(1) hue-rotate(180deg);
}
";

/// Which of an attachment row's actions a `PendingAttachment` fetch was for -
/// decides what happens to the bytes once `AccountEvent::PartFetched` lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PendingAttachmentAction {
    /// Open the bytes with the MIME type's *default* application, resolved via
    /// GIO and launched directly (no chooser), from a temporary file deleted
    /// when Lookout exits. Falls back to the XDG portal when there's no
    /// default app for the type (or the direct launch fails, e.g. sandboxed).
    Open,
    /// Ask the user which application to open the file with, via the XDG
    /// desktop portal's `org.freedesktop.portal.OpenURI` (the `ask` option).
    OpenWith,
    /// Offer a save-location dialog and write the bytes there.
    Save,
}

/// One attachment action in flight for the reading pane, tracked in
/// `UiState::pending_attachment`. `button` is the strip row's menu button,
/// disabled while the fetch is outstanding so the user can see which row is
/// preparing; it's restored on completion.
pub(crate) struct PendingAttachment {
    pub(crate) mailbox: MailboxId,
    pub(crate) uid: Uid,
    /// The `BodyPart::part_number` being fetched - matched against a late
    /// `PartFetched` so a response meant for a different part (or a different
    /// message) can't re-enable the wrong row.
    pub(crate) part_number: String,
    /// What to do with the bytes once they arrive.
    pub(crate) action: PendingAttachmentAction,
    /// The row's menu button, to re-enable once the fetch lands.
    pub(crate) button: gtk::MenuButton,
}

/// One fetched part in an [`AttachmentBatch`]: the part (for classifying it
/// as an attachment vs an inline `cid:` image) plus its de-duplicated save
/// name and bytes.
pub(crate) struct CollectedAttachment {
    pub(crate) part: BodyPart,
    /// A de-duplicated file name, computed as the part is collected.
    pub(crate) name: String,
    pub(crate) bytes: Vec<u8>,
}

/// What a batch of fetched attachments is for.
pub(crate) enum AttachmentBatchAction {
    /// Write each attachment into this folder, one file per part (item 1.23 /
    /// M-33).
    SaveAll(gio::File),
    /// Write the attachments as a single zip archive at this file (item 1.23).
    Zip(gio::File),
    /// Open a forward composer with the fetched original's parts (item 1.11 /
    /// C-13): the `base` prefill already carries the `Fwd:` subject and both
    /// body forms; the collected attachments fill its `attachments` list and
    /// its inline `cid:` images are resolved from the same batch.
    Forward(Box<ForwardBatch>),
}

/// The context a forward-with-attachments batch opens its composer with once
/// the original's parts have drained (item 1.11 / C-13).
pub(crate) struct ForwardBatch {
    /// The prefill built by `build_forward_prefill`, with `attachments` empty
    /// and `html_body` still carrying raw `cid:` references - both are filled
    /// in at drain time.
    pub(crate) prefill: crate::compose::ComposePrefill,
    /// The composer's send channel (the message's account), captured at
    /// launch so the composer opens even if the selection has moved on.
    pub(crate) cmd_tx: async_channel::Sender<AccountCommand>,
    pub(crate) title: String,
    pub(crate) account_id: Option<AccountId>,
    pub(crate) signature_kind: Option<crate::config_view::SignatureDefault>,
    /// Open the composer straight into its own window instead of the reading
    /// pane (the popped-out message window's Forward).
    pub(crate) pop_out_on_open: bool,
}

/// A batched fetch of one message's parts, tracked in
/// `UiState::pending_attachment_batch`: the parts' bytes are fetched one part
/// at a time (the session serves one `FetchAttachment` per command), then
/// either written to disk ("Save all" / "Download as zip") or handed to a
/// composer (forward) once the queue drains.
pub(crate) struct AttachmentBatch {
    pub(crate) mailbox: MailboxId,
    pub(crate) uid: Uid,
    /// The `BodyPart::part_number` currently being fetched, matched against a
    /// late `PartFetched` so a response for a different part (or an inline
    /// `cid:` request racing the batch) isn't mistaken for the next in line.
    pub(crate) current: String,
    /// Parts still to fetch, in the order they were listed.
    pub(crate) queue: VecDeque<BodyPart>,
    /// Parts already fetched, in order.
    pub(crate) collected: Vec<CollectedAttachment>,
    pub(crate) action: AttachmentBatchAction,
}

/// One .eml export in flight for the reading pane's "More" menu, tracked in
/// `UiState::pending_raw_message`. `initial_name` is the save dialog's
/// suggested filename (built from the message's subject at request time, so
/// it survives even if the selection changes before the bytes arrive).
pub(crate) struct PendingRawMessage {
    pub(crate) mailbox: MailboxId,
    pub(crate) uid: Uid,
    pub(crate) initial_name: String,
}

/// A message-menu action waiting for `(mailbox, uid)`'s body
/// (`UiState::body_waiter`).
pub(crate) struct BodyWaiter {
    pub(crate) mailbox: MailboxId,
    pub(crate) uid: Uid,
    pub(crate) run: Box<dyn FnOnce(Rc<EmailBody>)>,
}

/// The single "Message details" dialog (item 1.10 / M-30) while one is open,
/// tracked in `UiState::message_details`: its window plus the widgets the
/// async raw-message reply fills, and the `(mailbox, uid)` it belongs to so
/// `AccountEvent::RawMessageFetched` can route the bytes to it. One at a
/// time, like `compose_popout_window`; opening it for the same message
/// re-presents the existing dialog. Unlike the .eml export, this request is
/// not tied to the list selection - navigating the reading pane while the
/// dialog is open must not cancel it.
pub(crate) struct MessageDetailsDialog {
    pub(crate) window: adw::Window,
    pub(crate) mailbox: MailboxId,
    pub(crate) uid: Uid,
    /// Whether the raw bytes have been applied; a second arrival (a Retry)
    /// simply overwrites.
    pub(crate) fetched: bool,
    /// The pages below the header: "loading" (spinner), "content" (the two
    /// text views), "error".
    pub(crate) status_stack: gtk::Stack,
    /// The "Internet headers" text view, filled from the parsed header block.
    pub(crate) headers_view: gtk::TextView,
    /// The raw RFC 5322 source text view.
    pub(crate) source_view: gtk::TextView,
    /// The failure page's message label, set when the fetch errors or times
    /// out.
    pub(crate) error_label: gtk::Label,
    /// The Details / Source pages, so re-presenting the dialog can switch to
    /// the page asked for.
    pub(crate) pages: adw::ViewStack,
}

/// Which page "View message details" opens on: Details, or Source (the
/// message context menu's View ▸ View message source, item 1.17).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DetailsPage {
    Details,
    Source,
}

impl DetailsPage {
    pub(crate) fn name(self) -> &'static str {
        match self {
            DetailsPage::Details => "details",
            DetailsPage::Source => "source",
        }
    }
}

/// A saved draft whose "Edit" action is waiting on its raw bytes (item 1.12),
/// tracked in `UiState::pending_draft_open`. The bytes are parsed by
/// `lookout_mail::body::parse_draft_prefill` and swapped into the composer
/// once `AccountEvent::RawMessageFetched` lands. The summary rides along so
/// the composer prefill can be built without re-reading the list selection
/// (which may have moved on by then).
pub(crate) struct PendingDraftOpen {
    pub(crate) mailbox: MailboxId,
    pub(crate) uid: Uid,
    pub(crate) summary: EmailSummary,
}

/// A "Forward as attachment" action waiting on the original's raw bytes (item
/// 1.11 / C-13), tracked in `UiState::pending_forward_raw`. The whole message
/// goes out as a `message/rfc822` attachment, so the composer opens once
/// `AccountEvent::RawMessageFetched` lands. The summary names the attachment
/// and the `Fwd:` subject; `cmd_tx`/`account_id` are captured at launch so
/// the composer opens through the right account even if the selection moves.
pub(crate) struct PendingForwardRaw {
    pub(crate) mailbox: MailboxId,
    pub(crate) uid: Uid,
    pub(crate) summary: EmailSummary,
    pub(crate) cmd_tx: async_channel::Sender<AccountCommand>,
    pub(crate) account_id: Option<AccountId>,
}

/// A rule-initiated Forward/Redirect (item 2.5 / M-14) waiting on the
/// message's raw bytes: when `AccountEvent::RawMessageFetched` lands for the
/// `(mailbox, uid)`, the message is sent on to `to`. A queue rather than a
/// single slot, since several rules can forward different arrivals at once;
/// see `rules::deliver_rule_forwards`.
pub(crate) struct PendingRuleForward {
    pub(crate) mailbox: MailboxId,
    pub(crate) uid: Uid,
    pub(crate) to: Vec<String>,
    pub(crate) subject: Option<String>,
    pub(crate) kind: RuleForwardKind,
}

/// Whether a pending rule send is a forward (a new `Fwd:` message with the
/// original attached) or a redirect (the original resent to the new
/// recipients).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuleForwardKind {
    Forward,
    Redirect,
}

/// Marks a value as `Send` so it can cross from WebKit's scheme-handler
/// thread to the main loop. `webkit::URISchemeRequest` is a ref-counted
/// GObject whose `finish`/`finish_error` are documented thread-safe; the
/// wrapper exists only because the gtk-rs bindings don't declare it `Send`.
/// The request is only ever *touched* on the main thread after arrival -
/// the same pattern as the credential providers' `SendWrapper` in
/// `build_window`, where the justification is spelled out in full.
pub(crate) struct SendWrapper<T>(pub(crate) T);
unsafe impl<T> Send for SendWrapper<T> {}

/// A `cid:` image request forwarded from WebKit's scheme-handler thread to
/// the main loop: `cid` is the reference as it appeared in the message's
/// HTML, and `request` is the WebKit request that must be finished with the
/// part's bytes (or an error) once the main loop has resolved it.
pub(crate) struct CidSchemeRequest {
    pub(crate) request: SendWrapper<webkit::URISchemeRequest>,
    pub(crate) cid: String,
}

/// The inline `cid:` image fetches in flight for the reading pane's message
/// (or its conversation's cards, item 1.7), tracked in
/// `UiState::pending_cid` keyed by `(mailbox, uid, part number)`. Each holds
/// every WebKit URI-scheme request waiting on that part - a body can embed
/// the same image twice - answered together with the part's bytes
/// (`AccountEvent::PartFetched`) or an error (`PartFetchFailed`, a timeout,
/// or the user moving to another message). The part itself isn't kept - the
/// response arms re-read it from the event.
pub(crate) type PendingCidKey = (MailboxId, Uid, String);

/// A small bounded LRU of recently-viewed message bodies, keyed by
/// `(mailbox, uid)`, front = most recently used. See `BODY_CACHE_IN_MEMORY`.
/// Bodies are shared (`Rc`): an HTML body can be megabytes, and every reader
/// only borrows it, so a hit hands out the same allocation rather than a copy.
pub(crate) struct BodyCache {
    pub(crate) capacity: usize,
    pub(crate) entries: VecDeque<(MailboxId, Uid, Rc<EmailBody>)>,
}

impl BodyCache {
    pub(crate) fn new(capacity: usize) -> Self {
        BodyCache {
            capacity,
            entries: VecDeque::new(),
        }
    }

    /// Returns the cached body for `(mailbox, uid)`, promoting it to most
    /// recently used, or `None` on a miss. O(capacity) - the cache is tiny.
    pub(crate) fn get(&mut self, mailbox: &MailboxId, uid: &Uid) -> Option<Rc<EmailBody>> {
        let pos = self.entries.iter().position(|(m, u, _)| m == mailbox && u == uid)?;
        let entry = self.entries.remove(pos).expect("position came from this deque");
        self.entries.push_front(entry);
        Some(Rc::clone(&self.entries.front().expect("just pushed").2))
    }

    /// Whether `(mailbox, uid)` is cached, without promoting it.
    pub(crate) fn contains(&self, mailbox: &MailboxId, uid: &Uid) -> bool {
        self.entries.iter().any(|(m, u, _)| m == mailbox && u == uid)
    }

    pub(crate) fn insert(&mut self, mailbox: MailboxId, uid: Uid, body: Rc<EmailBody>) {
        if let Some(pos) = self.entries.iter().position(|(m, u, _)| *m == mailbox && *u == uid) {
            self.entries.remove(pos);
        }
        self.entries.push_front((mailbox, uid, body));
        while self.entries.len() > self.capacity {
            self.entries.pop_back();
        }
    }
}

/// One account's ManageSieve capability answer (item 2.11 / M-24), from
/// `AccountEvent::SieveCapabilities`. The `vacation` extension gates the Sieve
/// backend for automatic replies at all; `date` + `relational` (RFC 5260
/// `currentdate`) gate scheduled replies. A false `reachable` means the probe
/// couldn't talk to the Sieve port, so nothing else is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SieveVacationSupport {
    pub(crate) reachable: bool,
    pub(crate) vacation: bool,
    pub(crate) date: bool,
    pub(crate) relational: bool,
}

impl SieveVacationSupport {
    /// Whether the server can host scheduled replies (`currentdate`).
    pub(crate) fn supports_scheduling(self) -> bool {
        self.vacation && self.date && self.relational
    }
}

/// A late-bound action on one message (`UiState::create_task_from_message`).
pub(crate) type MessageActionFn = Rc<dyn Fn(&EmailSummary)>;
/// Opens the "Import from Evolution…" dialog, anchored to the widget that
/// was clicked.
pub(crate) type ImportEvolutionFn = Rc<dyn Fn(&gtk::Widget)>;
/// A window-level callback installed after the widgets it needs exist.
pub(crate) type LateCallback = Rc<dyn Fn()>;
/// The slot one [`LateCallback`] is held in until its owner has been built.
pub(crate) type LateCallbackSlot = Rc<RefCell<Option<LateCallback>>>;
/// Opens File ▸ Options at a named Settings category; an empty name leaves the
/// page's scroll position alone.
pub(crate) type OpenSettingsFn = Rc<dyn Fn(&str)>;
/// The slot an [`OpenSettingsFn`] is held in until the File page exists.
pub(crate) type OpenSettingsSlot = Rc<RefCell<Option<OpenSettingsFn>>>;

/// A late-bound action on a conversation's messages
/// (`UiState::reply_with_meeting`): opens a new event prefilled with their
/// participants and subject (item 3.19 / C-20). Takes the messages the menu
/// acted on (a thread's, or the selection's).
pub(crate) type ReplyWithMeetingFn = Rc<dyn Fn(&[EmailSummary])>;

/// Mutable UI-thread state the various signal handlers close over. Plain
/// `Rc<RefCell<_>>` is fine here - GTK is single-threaded, so there's no
/// need for `Arc<Mutex<_>>` on this side of the worker-thread boundary.
/// `pub(crate)` because `contacts_view` (the People screen module) reads and
/// writes the contacts fields below from its own file.
pub(crate) struct UiState {
    /// Per-account mail session handles. `pub(crate)` for `contacts_view`,
    /// whose attendee autocomplete unions the mail-history caches across
    /// every connected account.
    pub(crate) accounts: HashMap<AccountId, AccountHandle>,
    /// CardDAV-derived contacts discovered per account, including both
    /// category buckets (for Contacts UI) and flattened suggestions (for
    /// composer autocomplete). Owned by the People screen module
    /// (`contacts_view`), read by the composer/attendees here.
    pub(crate) contacts_by_account: HashMap<AccountId, ContactsAccountSnapshot>,
    /// Contacts starred via the People screen's row toggle, keyed by
    /// `(account, contact_identity)`. Local-only: never written back to the
    /// vCard or synced to the server, so it resets if the identity a contact
    /// resolves to (its `UID`, falling back to its first email) changes.
    /// Persisted in the UI-state database (`ui_state_db`), loaded at startup
    /// and written through on every star toggle.
    pub(crate) starred_contacts: HashSet<(AccountId, String)>,
    /// The People pane's search query (P-06), typed into the pane's own
    /// search box or the title-bar search while People is active. Session
    /// state only - not persisted - and read by `rebuild_contacts_list_ui`
    /// to filter the visible bucket.
    pub(crate) contacts_filter: String,
    /// Repaints the whole People screen (bucket rail and list) when the
    /// contacts behind it change, such as the desktop's address books - set
    /// once the contacts widgets exist. A no-op until wired.
    pub(crate) contacts_refresh: Option<Rc<dyn Fn()>>,
    /// Rebuilds only the People middle list for the selected bucket, for
    /// changes that can't affect the bucket rail: the search query, the
    /// Filter/Sort options and "File contacts as". A no-op until wired.
    pub(crate) contacts_list_refresh: Option<Rc<dyn Fn()>>,
    /// The People list's Filter menu options (UX-65): a category to narrow
    /// to plus the "has email" / "has phone" quick filters, applied alongside
    /// `contacts_filter` by `rebuild_contacts_list_ui`. Session state only.
    pub(crate) contacts_filter_options: crate::contacts_view::ContactsFilterOptions,
    /// Repaints the People details pane for the current list selection
    /// (UX-65) - set once the details widgets exist, and called whenever the
    /// selection changes or the list is rebuilt. A no-op until wired.
    pub(crate) contacts_details_refresh: Option<Rc<dyn Fn()>>,
    /// The desktop's own address books (EDS_TODO.md E1.5), shown read-only
    /// in the People screen's "Desktop" section. Refilled by
    /// `desktop_contacts::reload`; empty when the feature is off or the
    /// desktop has no Evolution Data Server.
    pub(crate) desktop_contacts: Vec<crate::eds_directory::DesktopBook>,
    /// The UI-state database backing `starred_contacts`, opened best-effort
    /// at startup. `None` when the database couldn't be opened - favourites
    /// then fall back to session-only, never an error.
    pub(crate) ui_db: Option<Rc<crate::ui_state_db::UiStateDb>>,
    /// The GSettings-backed preference store (see `settings`), resolved once
    /// in `build_window` and written through on every preference change.
    pub(crate) settings: Rc<crate::settings::SettingsStore>,
    /// The relational-data config (`settings.json`): sending identities and
    /// folder-role overrides. Loaded at startup, written through by the
    /// manage-identities dialog. `pub(crate)` so the rules engine
    /// (`rules.rs`) can read the user's rules and identities; writes go
    /// through `save_app_config`.
    pub(crate) app_config: Rc<RefCell<crate::app_config::AppConfig>>,
    /// The shared category master list (item 2.1 / M-09, CAL-07, T-08). The
    /// same live set the message rows, Categorize menu and editors read; held
    /// here too so the Graph category sync and settings can reach it. Persisted
    /// through `AppConfig::categories`.
    pub(crate) categories: Rc<RefCell<crate::categories::CategorySet>>,
    /// Contacts that were present in a previous CardDAV sync for an account
    /// but are missing from the latest one - the People screen's Deleted
    /// bucket. Filled by `contacts_view`'s sync diff; survives restarts
    /// because the first poll of a session diffs against the cached
    /// snapshot, so deletions made while the app was closed land here too.
    pub(crate) deleted_contacts: HashMap<AccountId, Vec<VCard>>,
    /// Per-account command channel into the CardDAV poll loop
    /// (`sync_contacts_account`) - the write path for the People screen's
    /// create/edit/delete/import flows. Keyed by account, inserted as each
    /// account's session starts in `spawn_contacts_discovery`.
    pub(crate) contact_cmd_tx: HashMap<AccountId, async_channel::Sender<ContactCommand>>,
    /// Every GOA account discovered at startup, keyed by account id -
    /// including disabled ones, so Config's account list (and a re-enable)
    /// can refer to them without re-running discovery. Populated by the
    /// mail/calendar/contacts discovery passes; the config view reads it and
    /// the enable toggle reconnects from the stored structs.
    pub(crate) goa_accounts: HashMap<AccountId, DiscoveredGoaAccount>,
    /// One GOA D-Bus handle kept from the mail discovery pass, reused to
    /// reconnect a re-enabled account without opening another session-bus
    /// connection. `None` until mail discovery runs (or fails).
    pub(crate) goa_client: Option<GoaClient>,
    /// Set once any discovery pass (mail/calendar/contacts) finds GOA simply
    /// isn't on the session bus (`Error::is_service_unavailable`) - the
    /// normal case on KDE, which has no GOA daemon. Config reads this to
    /// hide the "GNOME Online Accounts" section instead of showing it empty,
    /// and the discovery passes skip their "couldn't reach GOA" toast.
    pub(crate) goa_unavailable: bool,
    /// Which account owns the currently-open mailbox - drives command
    /// routing (FetchBody, compose "From") and which account's
    /// `MessagesUpdated` events are allowed to update the message list (a
    /// background IDLE resync on some *other* account must not clobber
    /// whatever the user is currently looking at).
    pub(crate) current_account: Option<AccountId>,
    pub(crate) current_mailbox: Option<MailboxId>,
    /// A message the `app.open-message` action (an AI Chat link click) wants
    /// selected once the mailbox it lives in has actually finished loading -
    /// that load is async (see `select_mailbox`). Consumed - and cleared -
    /// the next time any mailbox's cache-read reply lands there, whether or
    /// not it matches, so a stale entry from an interrupted navigation can
    /// never fire later on an unrelated load.
    pub(crate) pending_message_selection: Option<(MailboxId, Uid)>,
    /// Which of the two message-list views is active. `Single` keys the list
    /// off `current_mailbox` exactly as before; `UnifiedInbox` instead merges
    /// every connected account's Inbox (see `unified_snapshots`).
    pub(crate) mail_view: MailView,
    /// The latest per-mailbox message sets for the unified view, filled from
    /// each account's `MessagesUpdated` events while `mail_view` is
    /// `UnifiedInbox`. The visible list is the union of these, deduplicated
    /// by `(mailbox, uid)` and sorted newest-first.
    pub(crate) unified_snapshots: HashMap<MailboxId, Vec<EmailSummary>>,
    /// Bumped for a mailbox every time an authoritative `MessagesUpdated`
    /// lands for it. `select_mailbox`/the startup restore capture a mailbox's
    /// value before spawning their off-thread disk-cache read and re-check it
    /// when the reply arrives, so a read that started before the live sync
    /// (and therefore returns a stale, possibly empty set - a never-opened
    /// folder has only `partial` search-index rows, which `load_messages`
    /// excludes) can never overwrite the fresh list the sync just painted.
    pub(crate) mailbox_sync_epoch: HashMap<MailboxId, u64>,
    /// Rows hidden from the message list on an optimistic delete/archive/
    /// report-as-junk, keyed by source mailbox, kept around so a matching
    /// `AccountEvent::MoveFailed` can restore exactly these rows. Cleared for
    /// a mailbox the moment any `AccountEvent::MessagesUpdated` lands for it -
    /// an authoritative sync always supersedes the optimistic stash.
    pub(crate) pending_optimistic_removals: HashMap<MailboxId, Vec<EmailSummary>>,
    /// Pre-toggle summaries for an optimistic mark-read/unread, keyed by
    /// source mailbox, kept around so a matching
    /// `AccountEvent::StoreFlagsFailed` can restore exactly their original
    /// flags. Cleared for a mailbox the moment any
    /// `AccountEvent::MessagesUpdated` lands for it, same convention as
    /// `pending_optimistic_removals`.
    pub(crate) pending_optimistic_flag_changes: HashMap<MailboxId, Vec<EmailSummary>>,
    /// The pin/unpin sibling of `pending_optimistic_flag_changes` - kept
    /// as its own stash rather than sharing that one, since a message can
    /// have a read-toggle and a pin-toggle in flight at once and the two
    /// need independent "before" snapshots and reconciliation (see
    /// `optimistic_toggle_pinned`).
    pub(crate) pending_optimistic_pinned_changes: HashMap<MailboxId, Vec<EmailSummary>>,
    /// The most recently requested body fetch, used to ignore stale
    /// `BodyFetched` updates that arrive after the user has moved on to a
    /// different message.
    pub(crate) pending_body_request: Option<(MailboxId, Uid)>,
    /// The `(mailbox, uid)` whose body fetch failed while it was still the
    /// message on the reading pane (the connection dropped, the server was
    /// unreachable, or the fetch timed out). The reading pane shows its
    /// "error" page for this message and its Retry button re-requests it.
    /// Cleared whenever a body renders, the selection changes, or a retry is
    /// dispatched.
    pub(crate) body_fetch_error: Option<(MailboxId, Uid)>,
    /// Bumped every time a body fetch is dispatched or abandoned. The
    /// fetch-timeout backstop captures the value it was armed under and only
    /// fails the pane if it's unchanged, so a timeout armed for a message the
    /// user has already navigated away from (or one that has since rendered)
    /// can never raise a stale error.
    pub(crate) body_fetch_generation: u64,
    /// An attachment-part fetch currently in flight for the reading pane's
    /// row - its menu button is disabled while the bytes are coming, and
    /// re-enabled when `AccountEvent::PartFetched` lands (or discarded if the
    /// user navigates away first). One at a time; the strip's buttons ignore
    /// a click while one is outstanding. See `PendingAttachment`.
    pub(crate) pending_attachment: Option<PendingAttachment>,
    /// A "Save all" / "Download as zip" run currently in flight (item 1.23) -
    /// the attachments' parts are fetched one at a time, then written together
    /// once the queue drains. One at a time; see `AttachmentBatch`.
    pub(crate) pending_attachment_batch: Option<AttachmentBatch>,
    /// A whole-message .eml export currently in flight for the "More" menu -
    /// one at a time, cleared when `AccountEvent::RawMessageFetched`/
    /// `RawMessageFetchFailed` lands (or discarded if the user navigates
    /// away first, in which case the late response is dropped as stale). See
    /// `PendingRawMessage`.
    pub(crate) pending_raw_message: Option<PendingRawMessage>,
    /// A Report phishing run (item 6.7 / S-09) waiting on the target's raw
    /// bytes: when `RawMessageFetched` lands for this `(mailbox, uid)`, the
    /// message is forwarded as a `.eml` attachment to the configured address
    /// and then moved to Junk. Only ever one in flight; cleared on delivery or
    /// failure.
    pub(crate) pending_report_phishing: Option<(MailboxId, Uid)>,
    /// Rule-initiated forwards/redirects (item 2.5 / M-14) waiting on their
    /// raw bytes. See `PendingRuleForward`.
    pub(crate) pending_rule_forwards: Vec<PendingRuleForward>,
    /// Arriving messages whose rule evaluation is deferred until their body
    /// (and headers) are fetched, because an enabled rule has a Body or
    /// Header condition (item 2.5 / M-14, A5b). The summary rides along so
    /// the rule's other conditions can still be checked on delivery; capped
    /// so a body that never arrives can't grow the queue without bound.
    pub(crate) pending_rule_evals: Vec<EmailSummary>,
    /// Open "message in new window" pop-outs (item 1.9 / M-31), keyed by
    /// `(mailbox, uid)` - one window per message; `open_message_window`
    /// re-presents an existing window instead of opening a second. The stored
    /// `Rc` keeps the window alive (GTK only keeps a presented window alive
    /// while a strong reference exists), and the window's close handler
    /// removes its entry. `message_is_visible` treats a match here as "on
    /// screen" for the attachment flows, so opening an
    /// attachment from a pop-out completes even when the reading pane has
    /// since moved to a different message.
    pub(crate) message_windows: HashMap<(MailboxId, Uid), Rc<RefCell<crate::message_window::MessageWindowState>>>,
    /// The "Message details" dialog (item 1.10 / M-30), while one is open.
    /// One at a time; `RawMessageFetched` routes the dialog's raw bytes here.
    /// See `MessageDetailsDialog`.
    pub(crate) message_details: Option<MessageDetailsDialog>,
    /// The `(mailbox, uid)` of the saved draft currently on the reading pane,
    /// stashed by `render_body` from the rendered summary's `\Draft` flag or
    /// Drafts-folder role so the "Edit" banner's handler knows what to open
    /// (item 1.12). Cleared on every navigation.
    pub(crate) rendered_draft: Option<(MailboxId, Uid)>,
    /// The bare `Message-ID` of the draft in `rendered_draft`, so its banner
    /// can tell a scheduled send waiting in Drafts (item 1.14 / UX-45).
    pub(crate) rendered_draft_message_id: Option<String>,
    /// The message on the reading pane when it's waiting in the Scheduled
    /// folder with a wake time (item 1.15): `(mailbox, uid, until)`, behind
    /// the "Snoozed until …" bar's Unsnooze. Cleared on every navigation.
    pub(crate) rendered_snooze: Option<(MailboxId, Uid, chrono::DateTime<chrono::Utc>)>,
    /// The message on the reading pane when it carries a follow-up flag
    /// (item 1.16): `(mailbox, uid, flag)`, behind the "Follow up. Due by …"
    /// bar's button. Cleared on every navigation.
    pub(crate) rendered_follow_up: Option<(MailboxId, Uid, lookout_core::FollowUpFlag)>,
    /// Each account's flagged mail as last reported by `FlaggedMessages`
    /// (item 1.16) - the Tasks view's "Flagged email" section.
    pub(crate) flagged_mail: HashMap<AccountId, Vec<EmailSummary>>,
    /// Each account's local outbox as last reported by `OutboxContents`
    /// (item 1.14): scheduled sends, messages waiting to go out, and ones
    /// that failed.
    pub(crate) outbox: HashMap<AccountId, Vec<lookout_mail::session::OutboxItem>>,
    /// The "N scheduled messages" banner above the message list. `None`
    /// only in tests.
    pub(crate) outbox_banner: Option<adw::Banner>,
    /// The "Scheduled messages" dialog and its row container, while open,
    /// so a fresh `OutboxContents` repaints it in place.
    pub(crate) outbox_dialog: Option<(adw::Dialog, gtk::Box)>,
    /// Each account's ManageSieve capability answer (item 2.11 / M-24), from
    /// `AccountEvent::SieveCapabilities`: what the Automatic replies dialog
    /// can offer on a Sieve account.
    pub(crate) sieve_support: HashMap<AccountId, SieveVacationSupport>,
    /// Each Microsoft 365 account's automatic replies as last fetched from
    /// Graph (item 2.11 / M-24). Sieve accounts read `app_config` instead;
    /// the Mail banner merges both sources.
    pub(crate) automatic_replies: HashMap<AccountId, lookout_core::AutomaticReplies>,
    /// The "Automatic replies are on" banner above the message list (item
    /// 2.11 / M-24). `None` only in tests.
    pub(crate) automatic_replies_banner: Option<adw::Banner>,
    /// The open Automatic replies dialog's repaint hook (item 2.11), while one
    /// is open: a fresh Graph fetch or ManageSieve capability answer calls it
    /// so the form reflects the new state.
    pub(crate) automatic_replies_dialog_refresh: Option<Rc<dyn Fn()>>,
    /// A reopened draft's raw fetch in flight (item 1.12), waiting on
    /// `AccountEvent::RawMessageFetched` to build the composer prefill.
    /// Cleared on navigation and by whichever response lands, so a late
    /// response can't open a composer over a message the user has since
    /// selected. See `open_draft_composer`.
    pub(crate) pending_draft_open: Option<PendingDraftOpen>,
    /// A "Forward as attachment" fetch in flight (item 1.11 / C-13), waiting
    /// on `AccountEvent::RawMessageFetched` to build the composer prefill with
    /// the whole message as a `message/rfc822` part. One at a time; cleared by
    /// whichever response lands.
    pub(crate) pending_forward_raw: Option<PendingForwardRaw>,
    /// Inline `cid:` image fetches in flight for the message on the reading
    /// pane, keyed by `BodyPart::part_number`. A message can embed several
    /// images, so - unlike `pending_attachment` - this is a map. Each entry
    /// holds the WebKit `URISchemeRequest` that must be finished with the
    /// fetched bytes (`AccountEvent::PartFetched`) or an error
    /// (`PartFetchFailed`, a timeout, or the user moving on). See
    /// `PendingCid`.
    pub(crate) pending_cid: HashMap<PendingCidKey, Vec<webkit::URISchemeRequest>>,
    /// The conversation on the reading pane's "conversation" page (item
    /// 1.7), or `None` while the pane shows a single message or nothing.
    pub(crate) rendered_conversation: Option<ConversationPane>,
    /// The `cid:`-bearing parts of each conversation card whose body has
    /// loaded, keyed by message - the conversation counterpart of
    /// `rendered_inline_parts`.
    pub(crate) conversation_inline_parts: HashMap<(MailboxId, Uid), Vec<BodyPart>>,
    /// The conversation page's widgets, set once when the reading pane is
    /// built, so the account event loop can reach them.
    pub(crate) conversation_ui: Option<Rc<ConversationUi>>,
    /// The conversation's unread cards the reader has seen, to be marked
    /// read when the selection moves away ("on-selection-change" mode, item
    /// 1.8) - the conversation counterpart of `mark_read_on_leave`.
    pub(crate) conversation_mark_on_leave: Vec<(MailboxId, Uid)>,
    /// The `cid:`-bearing parts of the message currently on the reading pane
    /// (a subset of the last rendered `EmailBody::parts`), used to resolve
    /// the `cid:` references WebKit's scheme handler forwards. Stale the
    /// moment the user navigates away; `render_body` re-stashes it.
    pub(crate) rendered_inline_parts: Vec<BodyPart>,
    /// Temporary files written for the row's "Open" action - attachments
    /// materialized on disk so the system's default handler can open them.
    /// Deleted when Lookout exits (`app.connect_shutdown` in `build_window`),
    /// so a viewer process is never left holding a file that's already gone.
    pub(crate) temp_attachment_files: HashSet<PathBuf>,
    /// The window's toast overlay, kept so the attachment Save flow (which
    /// runs from widget callbacks, not the account event loop) can surface
    /// fetch-timeout feedback. `None` only in tests, where no window exists.
    pub(crate) toast_overlay: Option<adw::ToastOverlay>,
    /// The running application, kept so `refresh_unread_indicators` can
    /// withdraw a mailbox's "new mail" notification once its unread count
    /// is back to 0 - see `mail_notifications::withdraw_new_mail_notification`.
    /// `None` only in tests, where no real `Application` exists.
    pub(crate) app: Option<adw::Application>,
    /// Sends started from a composer that haven't gone out yet, each with its
    /// "Sending: <subject>" toast (item 1.13). Matched to account events by
    /// `client_token` until `OutboxQueued` names the outbox row, then by row
    /// id - so a read receipt or invitation reply going out in between can't
    /// retract the wrong toast.
    pub(crate) pending_sends: Vec<PendingSend>,
    /// The `client_token` for the next composer send.
    pub(crate) next_send_token: u64,
    /// The reversible-action history behind the Undo toast and Ctrl+Z
    /// (item 0.4). Most-recent last; entries time out with their toast (see
    /// `show_undo_toast`) and are bounded by `MAX_UNDO_ENTRIES`.
    pub(crate) undo_stack: Vec<UndoEntry>,
    /// Monotonic id for the next `UndoEntry`, so a toast's Undo button undoes
    /// exactly the action it names even after later actions push more.
    pub(crate) next_undo_id: u64,
    /// A body is currently loading into the reading pane's WebView and
    /// should be revealed when its load finishes. Cleared on every selection
    /// change so a load started for a message the user has already navigated
    /// away from can never pop the pane back open (the persistent
    /// `load-changed` handler in the reading-pane build consults this before
    /// revealing).
    pub(crate) pending_html_reveal: bool,
    /// The summary of the message whose body is next to be revealed. The
    /// reading-pane header is updated from this only when the message page
    /// actually renders (`render_body`), never when the selection changes -
    /// so the previous message's header stays on screen for the whole
    /// fade-out instead of being swapped to the next email mid-fade.
    /// Cleared/overwritten on every selection change.
    pub(crate) pending_header: Option<EmailSummary>,
    /// Recently-viewed message bodies, so switching back to a message already
    /// read this session - or quoting it via Reply/Reply-All/Forward - does
    /// not re-fetch it from the server. Bounded LRU; see `BodyCache`. The
    /// worker-side `bodies` table in `lookout-mail`'s cache complements this
    /// with a disk layer that survives restarts.
    pub(crate) body_cache: BodyCache,
    /// Bumped on every selection change (alongside disarming
    /// `pending_html_reveal`). The HTML reveal-fallback timeout in
    /// `render_body` captures the value it was armed under and only reveals
    /// if it's unchanged, so a stale timeout armed for a message the user has
    /// already moved on from can never pop the *next* message's page open
    /// mid-load.
    pub(crate) reveal_generation: u64,
    /// The folder pane's last-selected view, loaded from disk at startup and
    /// rewritten whenever the selection changes (`select_mailbox` /
    /// `enter_unified_inbox`), so the pane reopens where the user left off -
    /// see `last_view`. `None` on the very first run, when the pane defaults
    /// to the "All Inboxes" unified view.
    pub(crate) last_selection: Option<LastSelection>,
    /// True while a remembered selection still has to be restored on startup.
    /// The restore is retried on every `FoldersUpdated` until the remembered
    /// mailbox's account has connected (see `restore_or_default_initial_view`),
    /// so a slow account can't cause its folder to be skipped; the default
    /// "All Inboxes" fallback only applies once restore is done or abandoned.
    /// Stops being relevant the moment the user clicks anything.
    pub(crate) restore_pending: bool,
    /// The `(mailbox, uid)` of the message whose body is currently displayed
    /// on the reading pane's "message" page. Lets the message-selection
    /// handler tell a re-selection of the already-open email apart from a
    /// real navigation, so a list rebuild that keeps the same row selected
    /// doesn't route the pane through "empty" and crossfade the same email
    /// again. `None` while the pane shows nothing.
    pub(crate) rendered_message: Option<(MailboxId, Uid)>,
    /// A pending "mark as read after a delay" timer (item 1.8): the timer's
    /// source, plus the message it will mark if it's still selected when it
    /// fires. `None` when nothing is pending. Cancelled on every navigation.
    pub(crate) pending_mark_read: Option<(glib::SourceId, MailboxId, Uid)>,
    /// The message currently on the reading pane that should be marked read
    /// when the selection moves away from it (item 1.8,
    /// "on-selection-change" mode). Only set when the message was unread when
    /// displayed; `None` otherwise, or after it's been flushed.
    pub(crate) mark_read_on_leave: Option<(MailboxId, Uid)>,
    /// The parsed List-Unsubscribe actions of the message currently on the
    /// reading pane, re-derived by `render_body` from the rendered body's
    /// headers and read by the banner's button handler when it fires. `None`
    /// when the pane shows nothing or the message offers no unsubscribe
    /// action.
    pub(crate) unsubscribe_info: Option<lookout_core::ListUnsubscribe>,
    /// The `(mailbox, uid)` of the message whose unsubscribe banner the user
    /// acted on (clicked "Unsubscribe" - the banner's only affordance;
    /// Adw.Banner has no close button); while that message stays on screen
    /// the banner must not come back. Cleared on every navigation, so
    /// returning to the message later shows the banner again.
    pub(crate) unsubscribe_dismissed: Option<(MailboxId, Uid)>,
    /// The parsed iMIP invitation (or cancellation / RSVP reply) carried by
    /// the message currently on the reading pane's `text/calendar` part,
    /// re-derived by `render_body` and read by the banner's button handler
    /// when it fires. `None` when the pane shows nothing or the message
    /// carries no iMIP payload.
    pub(crate) imip: Option<lookout_core::ImipInvitation>,
    /// The `(mailbox, uid)` of the message whose iMIP banner the user acted
    /// on (chose a response, removed the event, or dismissed the notice);
    /// while that message stays on screen the banner must not come back.
    /// Cleared on every navigation, so returning to the message later shows
    /// the banner again.
    pub(crate) imip_dismissed: Option<(MailboxId, Uid)>,
    /// The `(mailbox, uid)` of every REPLY message whose `PARTSTAT`s have
    /// already been applied to the stored organizer event (item 3.6 /
    /// CAL-16). `render_body` re-runs on navigation, so without this the
    /// same reply would be merged and PUT again before the resync lands.
    pub(crate) imip_replies_applied: HashSet<(MailboxId, Uid)>,
    /// A back-reference to the shared calendar state, filled once
    /// `build_window` has built it. `render_body` applies incoming iMIP
    /// replies to the stored organizer event through here, without every
    /// render path having to thread the calendar handle through (the same
    /// shape as `toast_overlay`).
    pub(crate) calendar_state: Option<Rc<RefCell<CalendarUiState>>>,
    /// The addresses a read receipt for the message currently on the reading
    /// pane should go to - parsed from its `Disposition-Notification-To`
    /// header by `render_body`, read by the banner's button handler and the
    /// automatic-send path. `None` when the pane shows nothing or the
    /// message doesn't request a receipt.
    pub(crate) read_receipt_request: Option<Vec<String>>,
    /// The `(mailbox, uid)` of the message whose read-receipt banner the user
    /// acted on (sent the receipt); while that message stays on screen the
    /// banner must not come back. Cleared on every navigation, like
    /// `unsubscribe_dismissed`.
    pub(crate) read_receipt_dismissed: Option<(MailboxId, Uid)>,
    /// Every `(mailbox, uid)` a read receipt has been sent for this session -
    /// the fire-once guard for the automatic policy (a message re-opened
    /// later, or re-rendered by a Config toggle, must not receipt twice).
    /// Session-only, like the `load_once_images` override.
    pub(crate) read_receipts_sent: HashSet<(MailboxId, Uid)>,
    /// The original-message details a read receipt for the message currently
    /// on the reading pane needs, stashed by `render_body` (which has the
    /// body) alongside `read_receipt_request` and read by the send path. The
    /// `imip`/`unsubscribe_info` equivalent - per-render data, so the
    /// banner's button handler doesn't need the (LRU-evictable) body cache.
    pub(crate) read_receipt_context: Option<ReadReceiptContext>,
    /// Mailboxes with a `SyncMailbox` request outstanding - sent but not yet
    /// answered by a `MessagesUpdated`. The startup burst (the session's
    /// cache replay plus the app's on-demand syncs) would otherwise queue
    /// several identical inbox syncs; dedupe them in `request_mailbox_sync`.
    /// Entries are cleared when the mailbox delivers, or when its account
    /// reconnects and sends fresh folders, so a request that dies with a
    /// dropped connection can't suppress a later one.
    pub(crate) syncing: HashSet<MailboxId>,
    /// Mailboxes the background body prefetch (see `PrefetchState` in
    /// `lookout_mail::session`) is currently warming - between its
    /// `AccountEvent::PrefetchStarted` and the matching `PrefetchFinished`.
    /// Kept separate from `syncing` above rather than folded into it: the two
    /// only share a UI surface (the folder-pane spinner, see
    /// `folder_row_spinners`/`set_mailbox_prefetching`), and if a mailbox
    /// mid-prefetch were marked `syncing` too, `request_mailbox_sync`'s dedup
    /// check would wrongly suppress a real user-initiated sync for it.
    pub(crate) prefetching: HashSet<MailboxId>,
    /// The folder-pane row spinner(s) currently bound to each mailbox that
    /// may show one (i.e. is `syncing` or `prefetching`), if a row for it is
    /// in view - refreshed on every `bind` (see `folder_factory`'s setup/
    /// bind in `build_ui`). A `Vec` rather than a single entry because a
    /// starred mailbox is bound to *two* simultaneous rows - its place in
    /// the account's own tree (`TreeItem::Folder`) and its pinned copy in
    /// the Favorites section (`TreeItem::Favorite`) - each with its own
    /// spinner widget; a single-valued map would only ever track whichever
    /// row bound last, leaving the other's spinner stuck at its state from
    /// its own last bind. `GtkListView` recycles row widgets as they scroll,
    /// so an entry can go stale (still pointing at a spinner that's since
    /// been rebound to a different folder); a stamped `"lookout-sync-mailbox"`
    /// qdata on the spinner itself is checked before every toggle to guard
    /// against that, and dead/stale weak refs are pruned on each refresh.
    /// Read and written by `set_mailbox_syncing`/`set_mailbox_prefetching`
    /// via `refresh_folder_spinner`, the only places `syncing`/`prefetching`
    /// above should be mutated from.
    pub(crate) folder_row_spinners: HashMap<MailboxId, Vec<glib::WeakRef<gtk::Spinner>>>,
    /// Accounts with an `AccountCommand::Refresh` outstanding - sent but not
    /// yet answered by a `FoldersUpdated` (`Refresh`'s own first step is a
    /// folder relist). Dedupes `request_account_refresh` the same way
    /// `syncing` dedupes `request_mailbox_sync`: the message list's Sync
    /// button fires both together on every click, so without this a
    /// double-click (or clicking again before the first refresh lands) would
    /// queue a redundant full relist behind the one already in flight.
    /// Cleared on the answering `FoldersUpdated`, or on reconnect/removal
    /// (also a `FoldersUpdated`/account-removal event), so a request that
    /// dies with a dropped connection can't suppress a later one.
    pub(crate) refreshing: HashSet<AccountId>,
    /// Microsoft 365 property pulls in flight, and the newest Graph delta
    /// cursor per account (see `GraphPropertySyncs`). Forgotten on account
    /// removal.
    pub(crate) graph_property_syncs: super::graph_sync::GraphPropertySyncs,
    /// How the message list is ordered, set from the list header's sort
    /// controls. Applied in `repopulate_message_list` - the single choke point
    /// every list rebuild passes through - so the order is uniform no matter
    /// which event produced the rebuild.
    pub(crate) sort_key: SortKey,
    /// True for newest/Z-A first, the order the list was hardcoded to before
    /// the sort controls existed.
    pub(crate) sort_descending: bool,
    /// Mailboxes the user has starred in the message-list header, rendered as
    /// a "Favorites" section pinned to the top of the folder tree. Persisted
    /// via the `mail-favorites` GSettings key, loaded at startup and written
    /// through on every star toggle (see `settings`). Ordered: once the user
    /// has moved a favourite up or down (`mail-favorites-custom-order`) this
    /// is the Favorites section's display order; before that the section
    /// sorts alphabetically and new favourites are simply appended.
    pub(crate) favorites: Vec<MailboxId>,
    /// The Favorites section's rows as last drawn, in order - what the "⋯"
    /// menu's Move up/down act on, and how a row knows its place there.
    /// Written by `rebuild_folder_tree`.
    pub(crate) favorite_display_order: Vec<MailboxId>,
    /// Each folder's local display preferences (icon colour, count mode),
    /// from the folder pane's "⋯" menu. Persisted in the UI-state database.
    pub(crate) folder_prefs: HashMap<MailboxId, crate::ui_state_db::FolderPrefs>,
    /// Each account's recently used message move/copy targets, most recent
    /// first - the message context menu's Move ▸ / Copy ▸ shortcuts (item
    /// 1.17). Persisted in the UI-state database.
    pub(crate) recent_folders: HashMap<AccountId, Vec<MailboxId>>,
    /// The recent search queries (M-18, roadmap 2.3), newest first - shown in
    /// the search box's history popover and persisted in the UI-state database.
    pub(crate) recent_searches: Vec<String>,
    /// Which folders a title-bar search sweeps (M-18, roadmap 2.3), persisted
    /// via the `mail-search-scope` GSettings key.
    pub(crate) search_scope: SearchScope,
    /// A message-menu action waiting for a message's body (Reply, Forward,
    /// Print on a message not read yet): run by the `BodyFetched` handler when
    /// that body lands. One at a time - a newer request replaces it.
    pub(crate) body_waiter: Option<BodyWaiter>,
    /// Config → Mail → "Load images from the web": whether the reading pane's
    /// WebView may load remote `image/*` subresources. Consulted by the
    /// load-policy handler on every resource decision. Persisted via the
    /// `mail-load-remote-images` GSettings key.
    pub(crate) load_remote_images: bool,
    /// Config → Mail → "Rich text": the default body mode for new compose
    /// sessions, read when the composer opens. Persisted via the
    /// `mail-rich-text-default` GSettings key.
    pub(crate) rich_text_default: bool,
    /// Trusted-sender entries (`name@example.com` or `@example.com`,
    /// normalized lowercase) with their trust level, keyed by the receiving
    /// account - the persisted shape of the external-content trust flow. The
    /// reading pane's load-policy handler consults this for the message
    /// currently on screen. Persisted in the UI-state database
    /// (`ui_state_db`), loaded at startup and written through on every
    /// trust/revoke action, like `starred_contacts`.
    pub(crate) trusted_senders: HashMap<(AccountId, String), lookout_core::TrustLevel>,
    /// Blocked-sender entries (item 2.9 / M-17): normalized lowercase
    /// addresses (`spammer@example.com`) or `@domain` patterns, keyed by the
    /// receiving account. New mail whose `From` matches an entry is moved to
    /// Junk with the RFC 5788 `$Junk` keyword; entered from the message menu's
    /// Block ▸. Persisted in the UI-state database (`ui_state_db`), like
    /// `trusted_senders`. The safe list is `trusted_senders` (S-01).
    pub(crate) blocked_senders: HashSet<(AccountId, String)>,
    /// Ignored conversations (item 2.8 / M-20), keyed by the receiving account
    /// and the message's `ThreadKey` string. New mail in an ignored thread is
    /// moved to Deleted Items on arrival; entered from the message menu's
    /// Ignore. Persisted in the UI-state database (`ui_state_db`), like
    /// `blocked_senders`.
    pub(crate) ignored_threads: HashSet<(AccountId, String)>,
    /// Rebuilds the folder pane from current state; registered by the window
    /// once the tree exists, so a config change that alters it (adding or
    /// removing a saved search, item 2.4 / M-19) can repaint the pane at once
    /// rather than waiting for the next `FoldersUpdated`. `None` until then.
    pub(crate) folder_tree_refresh: Option<Rc<dyn Fn()>>,
    /// The `(account, normalized sender address)` of the message currently on
    /// the reading pane, re-stashed by `render_body` from the rendered
    /// summary's From address. The load-policy handler resolves this against
    /// `trusted_senders` on every resource decision; `None` when the pane
    /// shows nothing or the message has no usable sender (the debug `.eml`
    /// viewer, which also has no account to key trust on).
    pub(crate) rendered_trust_sender: Option<(AccountId, String)>,
    /// The impersonated address from the rendered message's `From` display
    /// name when it hides a *different* address (`"support@paypal.com"
    /// <attacker@evil.example>`), re-stashed by `render_body`. Drives the
    /// sender-authentication banner (S-03). `None` when there is no display
    /// name, no embedded address, or the domains match.
    pub(crate) rendered_from_mismatch: Option<String>,
    /// The `href`s of the rendered message's links whose visible text names a
    /// different host (item 6.3 / S-08), computed once per render by
    /// `render_body`. The reading pane's link-click handler confirms before
    /// opening one of these. Empty for messages with no body or no risky link.
    pub(crate) rendered_link_warnings: Vec<String>,
    /// Session-only "load remote images just this once" override for the
    /// message currently on the reading pane - the external-content banner's
    /// transient action. Cleared on every navigation.
    pub(crate) load_once_images: bool,
    /// The header's "Switch message theme" toggle for the message currently
    /// on the reading pane: when true, the message body's own background
    /// colour is stripped and its colours inverted (see
    /// `MESSAGE_THEME_OVERRIDE_CSS`) so the app theme's background shows
    /// through with the content still readable against it. A per-email
    /// override - reset to the Config → Appearance "Dark message theme"
    /// default on every navigation (alongside the other per-message state,
    /// and always in lockstep with the physical `set_message_theme_armed`),
    /// and `render_body` syncs the header button from it so the next message
    /// opens in the configured default.
    pub(crate) message_theme_override: bool,
    /// The `(mailbox, uid)` of the message whose external-content banner the
    /// user acted on (trusted the sender or loaded once); while that message
    /// stays on screen the banner must not come back. Cleared on every
    /// navigation, like `unsubscribe_dismissed`.
    pub(crate) trust_banner_dismissed: Option<(MailboxId, Uid)>,
    /// The last `html_remote_content_scan` result, keyed by the message it
    /// was computed for. The scan re-parses the rendered message's whole HTML
    /// (`window.rs`'s trust banner), which is unchanged across re-renders of
    /// the same message, so the banner block reuses the cached scan instead
    /// of rescanning on every render. Only the current message is retained -
    /// a single-slot cache, sized to what `render_body` actually re-visits.
    pub(crate) rendered_remote_scan: Option<(MailboxId, Uid, lookout_core::RemoteContentScan)>,
    /// Relay to the currently-open composer for its draft-autosave
    /// confirmations: the account event loops forward `DraftSaved`
    /// Message-Ids here, and the composer flips its "Saving draft…" label to
    /// "Draft saved" when its own id arrives. `None` while no composer is
    /// open; replaced whenever a new composer opens (dropping the previous
    /// sender lets the old composer's consumer exit).
    pub(crate) draft_saved_tx: Option<async_channel::Sender<String>>,
    /// Refresh hook for the currently-open composer's From dropdown: the
    /// Config → Mail accounts manage-identities dialog fires it after every
    /// change, so an identity added/edited while a composer is open shows up
    /// in its From list immediately. `None` while no composer is open.
    pub(crate) composer_identities_refresh: Option<Rc<dyn Fn()>>,
    /// Opens File ▸ Options on the Accounts → Signatures section - the
    /// composer's signature menu "Manage Signatures…" item. Late-bound:
    /// `build_window` fills it in once the File page's open logic exists
    /// (same pattern as `composer_identities_refresh`).
    pub(crate) manage_signatures: Option<Rc<dyn Fn()>>,
    /// Opens File ▸ Options on the Mail → My Templates section - the
    /// composer's template menu "Manage Templates…" item (item 2.10 / C-17).
    /// Late-bound, like `manage_signatures`.
    pub(crate) manage_templates: Option<Rc<dyn Fn()>>,
    /// Opens the "Import from Evolution…" dialog (EDS_TODO.md E3.4),
    /// anchored to the widget that was clicked. Late-bound: filled where the
    /// Config row is wired; the File page's Open & Export offers the entry
    /// while it is set.
    pub(crate) import_evolution: Option<ImportEvolutionFn>,
    /// The More menu's "Create task from message…": opens the task editor
    /// seeded from a message (`show_create_task_for_email`). Late-bound: the
    /// menu is built before `calendar_state`/`tasks_view` exist, so this hook
    /// is filled in once they do. `None` until then.
    pub(crate) create_task_from_message: Option<MessageActionFn>,
    /// The message menu's "Reply with meeting" (item 3.19 / C-20): opens a
    /// new event prefilled with the conversation's participants and subject.
    /// Late-bound like `create_task_from_message`; `None` until then.
    pub(crate) reply_with_meeting: Option<ReplyWithMeetingFn>,
    /// Repaints every surface listing flagged mail (the Tasks view's
    /// "Flagged email" section, the dashboard) from `flagged_mail` (item
    /// 1.16). Late-bound like `create_task_from_message`; `None` until then.
    pub(crate) flagged_mail_refresh: Option<Rc<dyn Fn()>>,
    /// Which compose session's relays (`draft_saved_tx` and
    /// `composer_identities_refresh`) are currently installed. Every composer
    /// bumps this when it opens and remembers its own value; a finishing
    /// composer clears the relays only if its generation still owns them, so
    /// a popped-out composer that finishes after a newer inline composer
    /// opened can't strip the newer composer's relays.
    pub(crate) composer_relay_generation: u64,
    /// The pop-out window hosting the composer, while one exists (the
    /// composer header's pop-out button moves the still-alive composer into
    /// its own window; `None` otherwise), paired with the composer
    /// generation that created it (its captured `composer_relay_generation`
    /// value) - so a finishing composer closes only its own window, never a
    /// newer composer's. Kept here so the window survives after the button
    /// handler returns - GTK only keeps a presented window alive for as long
    /// as it's on screen. The window's close handler pops the composer back
    /// into the reading pane, and a finishing composer destroys the window
    /// outright; both clear this.
    pub(crate) compose_popout_window: Option<(u64, adw::Window)>,
    /// The main window's action bar (`view_toolbar_stack`'s "mail-home"
    /// slot) plus the message-list command toolbar that normally fills it,
    /// available to any code path that opens a composer. While a composer is
    /// embedded in the reading pane, the action bar hosts the composer's
    /// editor toolbar instead (see `ComposeActionBar`); `None` only until
    /// `build_window` populates it (the toolbar row is built late, after the
    /// message-row factories that open composers), so composers fall back to
    /// their in-window toolbar layout if it's somehow missing.
    pub(crate) compose_action_bar: Option<Rc<ComposeActionBar>>,
    /// What the folder sidebar currently has rendered (see
    /// `folder_tree_signature`). `FoldersUpdated` now arrives repeatedly per
    /// account as the unread counts fill in, and almost all of those carry
    /// nothing the tree draws; comparing against this skips the rebuild
    /// entirely for them. `None` until the first tree is built.
    pub(crate) folder_tree: Option<FolderTreeSignature>,
    /// The folder pane's subfolders the user left expanded - the persisted
    /// counterpart of the session capture `rebuild_folder_tree` takes, so the
    /// first build of a fresh session can re-seed from it instead of the
    /// empty pre-model capture. Written back on every rebuild and on window
    /// close. Persisted in the UI-state database (`ui_state_db`), loaded at
    /// startup; survives "Clear all caches" like `starred_contacts`.
    pub(crate) expanded_folders: HashSet<MailboxId>,
    /// The folder pane's account groups (and the Favorites section, under its
    /// reserved key) the user left collapsed - the other half of the pane's
    /// expansion memory, defaulting every other group to expanded exactly as
    /// the session restore does. Persisted alongside `expanded_folders`.
    pub(crate) collapsed_groups: HashSet<AccountId>,
    /// Set while `rebuild_folder_tree` is putting the selection back after
    /// swapping the model, so the `selected-item` handler doesn't mistake the
    /// restore for the user clicking a folder. Without it, re-selecting the
    /// open mailbox would re-issue its sync, and the momentary landing on row
    /// 0 that `GtkSingleSelection`'s autoselect does on every `set_model`
    /// would enter the unified view.
    pub(crate) suppress_folder_selection: bool,
    /// Whether the message list is showing full-text search results. True only
    /// with a non-empty `search_query`; `MailView::Search` (see
    /// `MailView`) is the list's view mode while active. Exiting restores the
    /// pre-search view: `Single` when `current_mailbox` survived the search,
    /// else the unified "All Inboxes" view.
    pub(crate) search_active: bool,
    /// The query being searched for, matching the search entry's text. The
    /// list shows results as `(mailbox, uid)` sets that match it.
    pub(crate) search_query: String,
    /// The accumulated search results - the instant FTS cache pass over every
    /// account merged with each live IMAP `SEARCH` answer as it arrives,
    /// deduplicated by `(mailbox, uid)`. Repopulated into the message list on
    /// every change.
    pub(crate) search_results: Vec<EmailSummary>,
    /// The `(account, mailbox)` pairs the live IMAP pass has asked for and not
    /// yet been answered. `SearchResults` fires once per requested folder
    /// (always, even for an empty match set - see the session docs), removing
    /// its entry; an empty set means the live pass is done.
    pub(crate) search_pending: HashSet<(AccountId, MailboxId)>,
    /// The title-bar search entry (M-18, roadmap 2.3), so search helpers can
    /// rewrite its text (chip removal, the Refine form) and rebuild the
    /// refiner-chip row. `None` only before `build_window` sets it.
    pub(crate) search_entry: Option<gtk::SearchEntry>,
    /// The ribbon stack, so an active search can reveal its contextual
    /// "Search" row and restore the previous row on exit.
    pub(crate) search_ribbon: Option<gtk::Stack>,
    /// The ribbon stack's visible child before the search row was shown, kept
    /// so `exit_search` restores exactly what the user was looking at.
    pub(crate) search_ribbon_prev: Option<String>,
    /// The shared ribbon layout state (`ribbon.rs`): the mode in force and
    /// every live band, so one Simplified/Classic switch reaches the main
    /// window's ribbon and any open composer's toolbar alike. `None` only
    /// until `build_window` creates it.
    pub(crate) ribbon_style: Option<crate::ribbon::RibbonStyle>,
}

impl UiState {
    /// Whether a GOA account is enabled: everything is enabled unless its id
    /// sits in the `accounts-disabled` preference (Config → Accounts).
    /// `pub(crate)` for `contacts_view`'s discovery filtering.
    pub(crate) fn account_enabled(&self, id: &AccountId) -> bool {
        !self.settings.get_strv(crate::settings::ACCOUNTS_DISABLED).iter().any(|disabled| disabled == &id.0)
    }

    /// Marks a GOA account enabled/disabled, persisting the whole disabled
    /// set through the `accounts-disabled` preference.
    pub fn set_account_enabled(&self, id: &AccountId, enabled: bool) {
        let mut disabled = self.settings.get_strv(crate::settings::ACCOUNTS_DISABLED);
        disabled.retain(|existing| existing != &id.0);
        if !enabled {
            disabled.push(id.0.clone());
        }
        self.settings.set_strv(crate::settings::ACCOUNTS_DISABLED, disabled);
    }

    /// A recently-viewed message body from the in-memory LRU, if any - the
    /// pop-out message window (item 1.9) and its reply buttons read this
    /// instead of the reading pane's single-message fetch state. `&mut`
    /// because `BodyCache::get` promotes the hit (LRU).
    pub(crate) fn cached_body(&mut self, mailbox: &MailboxId, uid: Uid) -> Option<Rc<EmailBody>> {
        self.body_cache.get(mailbox, &uid)
    }

    /// Config → Mail → "Rich text": the body-mode default new composers open
    /// with. Read by the pop-out window's reply/forward actions, which open
    /// the composer from outside `build_window`'s scope.
    pub(crate) fn rich_text_default(&self) -> bool {
        self.rich_text_default
    }

    /// Config → Appearance → "Dark message theme": the state the per-message
    /// theme override opens in. Read by the pop-out message window (item
    /// 1.9), which has no reading-pane navigation to reset it from.
    pub(crate) fn message_theme_dark_default(&self) -> bool {
        self.settings.get_bool(crate::settings::MAIL_MESSAGE_THEME_DARK)
    }
}

/// Per-calendar-account state, kept separate from `UiState`/`AccountHandle`
/// (Mail's equivalents) matching the crate's existing per-domain-type
/// separation - Calendar is a wholly independent account set from Mail.
pub(crate) struct CalendarAccountHandle {
    pub(crate) cmd_tx: async_channel::Sender<CalendarCommand>,
    pub(crate) display_name: String,
    /// The account's CalDAV base URL, kept for the Config view's account
    /// overview.
    pub(crate) uri: String,
    /// Whether this account is served by the Microsoft Graph backend. Graph
    /// delivers iTIP itself, so invitation responses go through Graph's own
    /// RSVP endpoints rather than an iMIP REPLY (item 13).
    pub(crate) graph: bool,
    pub(crate) calendars: Vec<CalendarInfo>,
    /// Latest reported session state, rendered in the sidebar's "My
    /// calendars" checklist while the account hasn't delivered any
    /// calendars yet (see `refresh_calendar_checklist`).
    pub(crate) connection_state: CalConnectionState,
    /// Latest occurrences for whatever month this account last synced,
    /// keyed by month so a stale resync from one account can't clobber
    /// another account's occurrences for the currently-displayed month -
    /// same "only apply if it matches what's on screen" principle as Mail's
    /// `MessagesUpdated` handling.
    pub(crate) last_occurrences: Vec<EventOccurrence>,
    pub(crate) last_synced_month: Option<chrono::NaiveDate>,
    /// Occurrences keyed by the month they were synced in, pruned to the
    /// upcoming window (current month through three months ahead). The
    /// upcoming-events surfaces - the Lookout dashboard card and the mail
    /// overview panel's 90-day section - read this union: a single month's
    /// window would drain as events pass and the session's poll never
    /// advances past its one polled month. The calendar view and reminders
    /// keep using `last_occurrences`, so this map stays upcoming-only.
    pub(crate) occurrences_by_month: HashMap<chrono::NaiveDate, Vec<EventOccurrence>>,
    /// Latest full task list from the account's last `TasksUpdated` - tasks
    /// have no month window, so a whole-set snapshot is the natural unit.
    pub(crate) last_tasks: Vec<CalendarTask>,
    /// RFC 6638 scheduling capability discovered on the account principal:
    /// an advertised `schedule-outbox-URL` means the server delivers iTIP
    /// itself, so organizer saves skip the client-side iMIP sender (and later
    /// CAL-19 free/busy has its POST target). Defaults to "client schedules".
    pub(crate) scheduling: SchedulingInfo,
}

/// The original-message details a read receipt (RFC 8098 MDN) is built
/// from, stashed by `render_body` alongside the parsed
/// `Disposition-Notification-To` request. The builder (`lookout_mail`'s
/// `ReadReceipt`) needs the original's Message-ID/From/Subject/Date and its
/// headers (the report's third part); keeping them here means the banner's
/// button handler - and the automatic policy - never touch the
/// LRU-evictable body cache.
#[derive(Clone)]
pub(crate) struct ReadReceiptContext {
    pub(crate) message_id: String,
    pub(crate) original_from: String,
    pub(crate) subject: String,
    pub(crate) date: Option<String>,
    pub(crate) headers: Vec<(String, String)>,
}

/// Per-subscription state for one webcal feed - the fetch-only cousin of
/// `CalendarAccountHandle`. One handle per configured subscription, aligned
/// with `CalendarUiState::webcal_subscriptions` by subscription id; events
/// carry the synthetic calendar id `"webcal:<id>"` so the shared
/// calendar-id machinery (checklist toggles, checked set, colors, view
/// merge) works unchanged.
pub(crate) struct WebcalHandle {
    /// The synthetic calendar id its events carry (`"webcal:<id>"`) - how
    /// the handle's occurrences are filtered/colored/keyed everywhere.
    pub(crate) calendar_id: CalendarId,
    pub(crate) display_name: String,
    /// Latest occurrences for whatever month last synced - keyed the same
    /// "only apply if it matches what's on screen" way as
    /// `CalendarAccountHandle::last_occurrences`.
    pub(crate) last_occurrences: Vec<EventOccurrence>,
    pub(crate) last_synced_month: Option<chrono::NaiveDate>,
    /// Per-month occurrences for the Lookout dashboard, pruned like
    /// `CalendarAccountHandle::occurrences_by_month`.
    pub(crate) occurrences_by_month: HashMap<chrono::NaiveDate, Vec<EventOccurrence>>,
    /// The feed's latest fetch error, if any - the UI toasts on the
    /// transition into error, not on every 5-minute poll.
    pub(crate) error: Option<String>,
}

/// State for the synthesized "Birthdays" calendar - the calendar-id-keyed
/// cousin of `WebcalHandle`, but there is no fetch or poll to run: the
/// source is the `UiState::contacts_by_account` snapshots the CardDAV sync
/// already maintains, copied in on every contacts update, and the
/// per-month occurrences are recomputed in place (cheap: a few thousand
/// contacts at most) instead of cached. `None` until any contacts exist, so
/// the checklist's "Birthdays" row only appears when it has a data source.
pub(crate) struct BirthdaysHandle {
    /// The synthetic calendar id (`"birthdays"`) its events carry - the
    /// shared calendar-id machinery works unchanged, and read-only parity
    /// with webcal feeds falls out of the same id-based checks.
    pub(crate) calendar_id: CalendarId,
    pub(crate) display_name: String,
    /// The source contacts, one batch per account, replaced whenever a
    /// contacts snapshot lands (the signal that `BDAY` data changed).
    pub(crate) contacts: Vec<(AccountId, Vec<lookout_dav::ContactRecord>)>,
    /// Latest occurrences for whatever month last synced, computed by
    /// `sync_month` - keyed the same "only apply if it matches what's on
    /// screen" way as `CalendarAccountHandle::last_occurrences`.
    pub(crate) last_occurrences: Vec<EventOccurrence>,
    pub(crate) last_synced_month: Option<chrono::NaiveDate>,
    /// Per-month occurrences for the Lookout dashboard, computed by
    /// `sync_dashboard_window` and pruned like
    /// `CalendarAccountHandle::occurrences_by_month`.
    pub(crate) occurrences_by_month: HashMap<chrono::NaiveDate, Vec<EventOccurrence>>,
}

impl BirthdaysHandle {
    /// Replaces the source contact set (called when a contacts snapshot
    /// lands) - the only thing that can change birthday data besides the
    /// passage of time.
    pub(crate) fn set_contacts(&mut self, contacts: Vec<(AccountId, Vec<lookout_dav::ContactRecord>)>) {
        self.contacts = contacts;
    }

    /// (Re)computes the handle's occurrences for `month` - the scoped
    /// snapshot the calendar views, print path, and mini-calendar read.
    /// Deterministic (sorted by start then uid) so repeated calls are
    /// diff-friendly.
    pub(crate) fn sync_month(&mut self, month: chrono::NaiveDate) {
        self.last_occurrences = birthday_occurrences_batch(&self.contacts, &self.calendar_id, month);
        self.last_synced_month = Some(month);
    }

    /// (Re)computes the upcoming-window months (current through three
    /// months ahead) - the same window the account sessions' `FetchMonth`
    /// commands cover, so the upcoming-events surfaces and the reminder
    /// engine see far-ahead birthdays too.
    pub(crate) fn sync_dashboard_window(&mut self) {
        for month in dashboard_month_window() {
            let occurrences = birthday_occurrences_batch(&self.contacts, &self.calendar_id, month);
            insert_dashboard_occurrences(&mut self.occurrences_by_month, month, occurrences);
        }
    }
}

/// The synthetic calendar id every birthday occurrence carries.
pub(crate) fn birthdays_calendar_id() -> CalendarId {
    CalendarId("birthdays".to_string())
}

/// Runs `lookout_dav::birthday_occurrences` across every account's contact
/// batch and merges the results into one sorted set - the batch-level mirror
/// of the per-account function, kept here since `ContactRecord`'s account
/// grouping is an app-side concern.
pub(crate) fn birthday_occurrences_batch(contacts: &[(AccountId, Vec<lookout_dav::ContactRecord>)], calendar_id: &CalendarId, month: chrono::NaiveDate) -> Vec<EventOccurrence> {
    let mut occurrences: Vec<EventOccurrence> = contacts
        .iter()
        .flat_map(|(account_id, contacts)| lookout_dav::birthday_occurrences(account_id, calendar_id, contacts, month))
        .collect();
    occurrences.sort_by(|a, b| a.start.cmp(&b.start).then_with(|| a.uid.0.cmp(&b.uid.0)));
    occurrences
}

/// Per-Google-account state for the Google Tasks integration (keyed by the
/// account's email). One `run_google_tasks_session` actor per connected
/// account; its task lists map to synthetic `googletasks:<list id>`
/// calendar ids so the Tasks view's colour/merge machinery treats them like
/// calendars.
pub(crate) struct GoogleTasksHandle {
    pub(crate) cmd_tx: async_channel::Sender<GoogleTasksCommand>,
    pub(crate) email: String,
    /// The account's task lists, from the last `ListsUpdated` - the task
    /// editor's picker entries and the save-routing lookup.
    pub(crate) task_lists: Vec<TaskList>,
    /// Latest full task snapshot from the account's last `TasksUpdated`.
    pub(crate) last_tasks: Vec<CalendarTask>,
    /// The latest reported error, if any (revoked token, failed write, ...).
    pub(crate) error: Option<String>,
}

/// Per-Microsoft-365-account state for the Graph To Do integration (keyed by
/// the GOA account id, like `accounts`). One
/// `lookout_graph::run_todo_session` actor per connected account; its lists map
/// to synthetic `todo:<list id>` calendar ids so the Tasks view's
/// colour/merge machinery treats them like calendars (item 5.3 / A-05, T-12).
pub(crate) struct GraphTodoHandle {
    pub(crate) cmd_tx: async_channel::Sender<lookout_graph::TodoCommand>,
    /// The account's display name, so the task editor's picker can label its
    /// lists the same way CalDAV/Google sources are labelled.
    pub(crate) display_name: String,
    /// The account's To Do lists, from the last `ListsUpdated` - the task
    /// editor's picker entries and the save-routing lookup.
    pub(crate) task_lists: Vec<lookout_graph::TodoList>,
    /// Latest full task snapshot from the account's last `TasksUpdated`.
    pub(crate) last_tasks: Vec<CalendarTask>,
    /// The latest reported error, if any (revoked consent, failed write, ...).
    pub(crate) error: Option<String>,
}

/// The window's task-reminder sink: every merged `(account, task)` snapshot,
/// handed to the `ReminderEngine`'s `sync_tasks` on each Tasks-view repaint.
pub(crate) type TaskRemindersRefresh = Rc<dyn Fn(&[(AccountId, CalendarTask)])>;

pub(crate) struct CalendarUiState {
    pub(crate) accounts: HashMap<AccountId, CalendarAccountHandle>,
    pub(crate) displayed_month: chrono::NaiveDate,
    /// Which calendars (by id) are currently shown - unioned in
    /// `refresh_displayed_calendar_view`, toggled from the sidebar's "My
    /// calendars" checklist. Newly-discovered calendars default to checked
    /// (shown) - see `refresh_calendar_checklist`.
    pub(crate) checked_calendar_ids: HashSet<CalendarId>,
    /// Each calendar's assigned colour, persisted between sessions (see
    /// `calendar_colors`). Kept alongside `checked_calendar_ids` so the
    /// checklist can colour its checkboxes; new calendars are assigned here in
    /// `refresh_calendar_checklist`.
    pub(crate) calendar_colors: calendar_colors::CalendarColorMap,
    /// Command channel to the single webcal feed session (all subscriptions
    /// are polled by one actor), or `None` before `spawn_webcal_session` runs.
    /// Subscriptions are added/removed by sending the session the full new
    /// list via `SubscriptionCommand::Reload`.
    pub(crate) webcal_cmd_tx: Option<async_channel::Sender<SubscriptionCommand>>,
    /// Working copy of `AppConfig::webcal_subscriptions`, kept here so the
    /// checklist (and the session) can be driven without reaching into the
    /// config store on every event. Mutated only by the add/manage dialog,
    /// which persists the authoritative list back to `settings.json`.
    pub(crate) webcal_subscriptions: Vec<WebcalSubscription>,
    /// Per-subscription feed state, keyed by subscription id.
    pub(crate) webcal_handles: HashMap<String, WebcalHandle>,
    /// The synthesized "Birthdays" calendar, `None` until any contacts exist
    /// (the checklist row appears only when it has a data source - mirroring
    /// how the webcal group appears only when subscriptions exist).
    pub(crate) birthdays: Option<BirthdaysHandle>,
    /// Connected Google Tasks accounts, keyed by email.
    pub(crate) google_tasks: HashMap<String, GoogleTasksHandle>,
    /// Connected Microsoft 365 To Do accounts, keyed by GOA account id.
    pub(crate) graph_todo: HashMap<AccountId, GraphTodoHandle>,
    /// Every Google GOA account's email, discovered at startup - the
    /// "Connect Google Tasks" toolbar button's targets.
    pub(crate) google_account_emails: Vec<String>,
    /// Locally-stored tasks (`CalendarId("local")`) - the fallback store
    /// used when no connected source supports tasks. Survives "Clear all
    /// caches" (it lives in the UI-state database, not a cache).
    pub(crate) local_tasks: Vec<CalendarTask>,
    /// The task-reminder hook, registered by the window once the
    /// `ReminderEngine` exists (T-05): it receives every source's merged
    /// `(account, task)` snapshot on each `refresh_tasks_view`. `None` before
    /// then, so an early repaint is a no-op.
    pub(crate) task_reminders_refresh: Option<TaskRemindersRefresh>,
    /// Best-effort handle on the UI-state database for local-task writes;
    /// `None` when it couldn't open (local tasks then live in memory only).
    pub(crate) local_tasks_db: Option<Rc<RefCell<UiStateDb>>>,
    /// The local on-device task lists (item 4.8 / T-01), mirrored from
    /// `AppConfig::local_task_lists` so the Tasks navigation pane and the task
    /// editor (which only see this state) can offer and route them. The
    /// built-in default is always present.
    pub(crate) local_task_lists: Vec<crate::app_config::LocalTaskList>,
    /// Which Tasks navigation row is selected (item 4.8 / T-01). Ephemeral UI
    /// state - not persisted, defaulting to the "Tasks" (all) row.
    pub(crate) tasks_filter: crate::tasks_view::TasksFilter,
    /// Event search (CAL-22 / item 3.14): whether the calendar is showing
    /// results, the query, the merged/deduped rows, and the view to restore
    /// when the search is cleared.
    pub(crate) calendar_search_active: bool,
    pub(crate) calendar_search_query: String,
    pub(crate) calendar_search_results: Vec<crate::calendar_view::CalendarSearchRow>,
    pub(crate) calendar_search_previous_view: &'static str,
    /// Monotonic token for the async cache queries; replies for an older
    /// query are discarded.
    pub(crate) calendar_search_generation: u64,
    /// Number of per-account cache queries still in flight.
    pub(crate) calendar_search_pending: usize,
    /// Search only the calendars currently checked in the sidebar.
    pub(crate) calendar_search_checked_only: bool,
    /// Whether the one-shot cache backfill for search coverage has run this
    /// session (CAL-22). The backfill fetches a bounded month window with
    /// `FetchMonth` so cold-start search isn't limited to browsed months.
    pub(crate) calendar_search_backfilled: bool,
    /// Late-bound repaint hook so the calendar event loop can refresh an active
    /// search when a month syncs or an event changes (the loop has no
    /// `Worker`/search-entry references of its own).
    pub(crate) calendar_search_refresh: Option<Rc<dyn Fn()>>,
    /// The EDS mirror sync's pacing state (E2.3): whether it's enabled, the
    /// debounce timer for a calendar refresh, and the in-flight/coalesced
    /// flags. E2.4 wires the settings to it.
    pub(crate) eds_mirror: crate::eds_mirror::MirrorSyncState,
    /// The current day's My Day pin set (item 4.9 / T-02), mirrored from the
    /// UI-state database so the Tasks view can label its rows Add/Remove and
    /// resolve the pinned tasks without a database read per repaint. Today's
    /// `day` is kept alongside so a date rollover reloads.
    pub(crate) my_day_day: chrono::NaiveDate,
    pub(crate) my_day_pins: Vec<crate::my_day::MyDayKey>,
    /// The Lookout dashboard's repaint hook, registered by the window once
    /// `calendar_state` exists. `refresh_tasks_view` and the calendar event
    /// loops call it so the dashboard stays live; `None` until then (a
    /// no-op, never an error).
    pub(crate) dashboard_refresh: Option<Rc<dyn Fn()>>,
    /// Monotonic counter for the Lookout dashboard's async repaint. Each
    /// `refresh_lookout_view` bump starts a fresh generation; a reply whose
    /// generation no longer matches belongs to a refresh a newer one already
    /// superseded and is discarded (the `suggestion_generation` idiom).
    pub(crate) dashboard_generation: u64,
    /// Every account's flagged mail (item 1.16), mirrored from
    /// `UiState::flagged_mail` by its refresh hook so the dashboard and the
    /// Mail overview pane, which only see this state, can list it.
    pub(crate) flagged_mail: Vec<EmailSummary>,
    /// Occurrences with a drag-reschedule in flight, keyed by `(uid,
    /// recurrence_id)`, holding the occurrence with its new `start`/`end`
    /// already applied. Reapplied onto every `refresh_displayed_calendar_view`
    /// repaint until an incoming occurrence's own start/end already matches -
    /// meaning the server has confirmed it - at which point the entry is
    /// dropped. Rolled back (removed, no reapply) on `EventSaveFailed`.
    pub(crate) pending_calendar_moves: HashMap<(EventUid, Option<chrono::DateTime<chrono::Utc>>), EventOccurrence>,
    /// The Mail-screen overview pane's task rows' click-to-edit handler,
    /// registered by the window once `calendar_state` exists -
    /// `refresh_mail_overview_day_list` reads it to build rows that open the
    /// shared task editor (the overview's rows carry no completion checkbox).
    pub(crate) mail_overview_activate: Option<crate::tasks_view::ActivateHandler>,
    /// The Mail-screen overview pane's repaint hook, registered by the window
    /// once `calendar_state` exists - same pattern as `dashboard_refresh`.
    /// `refresh_tasks_view` calls it so the pane's task rows stay live as
    /// tasks are created, synced, toggled, or removed; `None` until then.
    pub(crate) mail_overview_refresh: Option<Rc<dyn Fn()>>,
    /// In-flight Scheduling Assistant free/busy lookups (item 3.8 / CAL-19),
    /// keyed by a window-unique request id: the open editor's delivery handle
    /// plus its own request generation. Filled by the editor's request
    /// callback and drained by the calendar event loop when the account
    /// answers (`FreeBusyUpdated`/`FreeBusyFailed`).
    pub(crate) scheduling_pending: HashMap<u64, (crate::event_editor::SchedulingHandle, u64)>,
    /// Monotonic id source for [`Self::scheduling_pending`].
    pub(crate) scheduling_next_id: u64,
    /// An event the context menu just created (Duplicate / Copy to calendar,
    /// item 3.12) whose editor should open once the server has confirmed it.
    /// The calendar event loop resolves it against the next `OccurrencesUpdated`
    /// that carries the new UID, so the editor opens on the real href/etag
    /// rather than re-creating the copy on save.
    pub(crate) pending_open_editor: Option<(CalendarId, EventUid)>,
    /// The open-event-editor hook, registered by the window once
    /// `calendar_state` exists (same late-binding pattern as
    /// `dashboard_refresh`), so the calendar event loop can open the editor
    /// for a just-created occurrence without holding the window's closures.
    pub(crate) open_event_editor: Option<Rc<dyn Fn(EventOccurrence)>>,
    /// The user-defined calendar groups (item 3.13 / CAL-33), mirrored from
    /// `AppConfig::calendar_groups` so the sidebar checklist can order its
    /// sections without reaching into the config store on every rebuild. The
    /// row/group action handler persists changes back to `settings.json`.
    pub(crate) calendar_groups: Vec<crate::app_config::CalendarGroup>,
    /// Calendars removed from the sidebar (item 3.13), mirrored from
    /// `AppConfig::hidden_calendars`; hidden calendars are filtered out of the
    /// checklist and every view until "Show all".
    pub(crate) hidden_calendars: Vec<CalendarId>,
    /// The sidebar checklist's row/group action handler (item 3.13),
    /// registered by the window once `calendar_state` exists - the same
    /// late-binding pattern as `open_event_editor`, so
    /// `refresh_calendar_checklist` can hand it to every rebuild from the many
    /// call sites that have no window reference.
    pub(crate) calendar_sidebar_action: Option<calendar_view::CalendarSidebarActionHandler>,
    /// In-flight event-attachment downloads (item 3.16 / CAL-21), keyed by a
    /// window-unique request id: the save callback the calendar event loop
    /// hands the decoded bytes (or the failure message) when the owning
    /// account answers a `FetchEventAttachment`.
    pub(crate) attachment_requests: HashMap<u64, AttachmentRequest>,
    /// Monotonic id source for [`Self::attachment_requests`].
    pub(crate) attachment_next_id: u64,
}

/// One pending `FetchEventAttachment` answer (item 3.16 / CAL-21): the save
/// dialog's success/failure callbacks, captured by the request site and run on
/// the main loop when the calendar session replies.
pub(crate) struct AttachmentRequest {
    pub(crate) on_success: Box<dyn Fn(String, Vec<u8>)>,
    pub(crate) on_error: Box<dyn Fn(String)>,
}
