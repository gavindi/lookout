use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};
use lookout_core::{AccountId, CalendarId, CalendarInfo, EmailBody, EmailSummary, EventOccurrence, Mailbox, MailboxId, MailboxRole, Uid};
use lookout_dav::session::{CalendarCommand, CalendarSessionEvent, ConnectionState as CalConnectionState};
use lookout_dav::CalendarAccountConfig;
use lookout_goa::{GoaCalendarAccount, GoaClient};
use lookout_mail::session::{AccountCommand, AccountEvent, ConnectionState};
use lookout_mail::{AccountConfig, EndpointConfig};
use webkit::prelude::*;

use crate::calendar_colors;
use crate::calendar_view::{self, CalendarMain};
use crate::folder_tree::{build_multi_account_tree_model, TreeItem};
use crate::goa_calendar_credentials::GoaCalendarCredentialProvider;
use crate::goa_credentials::GoaCredentialProvider;
use crate::last_view::{self, LastSelection};
use crate::worker::Worker;

/// Per-account state the UI needs once an `AccountSession` actor is running:
/// how to send it commands, its identity (for compose "From" and toast
/// labeling), and the last folder list it reported (kept here so the
/// multi-account folder tree can be rebuilt in full from all accounts'
/// latest snapshots whenever any one of them changes).
struct AccountHandle {
    cmd_tx: async_channel::Sender<AccountCommand>,
    email: String,
    display_name: String,
    /// Connection parameters, kept for the Config view's account overview
    /// (the Config view shows how each account is configured, not just that
    /// it exists).
    imap_host: String,
    imap_port: u16,
    smtp_host: String,
    smtp_port: u16,
    folders: Vec<Mailbox>,
}

/// What the message list is currently showing - either a single mailbox (the
/// classic folder-selection view) or the synthetic "All Inboxes" unified view
/// merging every connected account's Inbox.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MailView {
    Single,
    UnifiedInbox,
}

/// Which field the message list is ordered by. Paired with
/// `UiState::sort_descending`, this is the whole of the list's ordering
/// policy - see `sort_messages`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SortKey {
    Date,
    Sender,
    Subject,
}

impl SortKey {
    /// The sort dropdown's button label for this key.
    fn label(self) -> &'static str {
        match self {
            SortKey::Date => "By Date",
            SortKey::Sender => "By Sender",
            SortKey::Subject => "By Subject",
        }
    }

    /// The key's name in the `sort-key` `gio::SimpleAction`'s state, which is
    /// what the sort menu's radio items are bound to.
    fn action_state(self) -> &'static str {
        match self {
            SortKey::Date => "date",
            SortKey::Sender => "sender",
            SortKey::Subject => "subject",
        }
    }

    fn from_action_state(name: &str) -> Option<Self> {
        match name {
            "date" => Some(SortKey::Date),
            "sender" => Some(SortKey::Sender),
            "subject" => Some(SortKey::Subject),
            _ => None,
        }
    }
}

/// Mutable UI-thread state the various signal handlers close over. Plain
/// `Rc<RefCell<_>>` is fine here - GTK is single-threaded, so there's no
/// need for `Arc<Mutex<_>>` on this side of the worker-thread boundary.
struct UiState {
    accounts: HashMap<AccountId, AccountHandle>,
    /// Which account owns the currently-open mailbox - drives command
    /// routing (FetchBody, compose "From") and which account's
    /// `MessagesUpdated` events are allowed to update the message list (a
    /// background IDLE resync on some *other* account must not clobber
    /// whatever the user is currently looking at).
    current_account: Option<AccountId>,
    current_mailbox: Option<MailboxId>,
    /// Which of the two message-list views is active. `Single` keys the list
    /// off `current_mailbox` exactly as before; `UnifiedInbox` instead merges
    /// every connected account's Inbox (see `unified_snapshots`).
    mail_view: MailView,
    /// The latest per-mailbox message sets for the unified view, filled from
    /// each account's `MessagesUpdated` events while `mail_view` is
    /// `UnifiedInbox`. The visible list is the union of these, deduplicated
    /// by `(mailbox, uid)` and sorted newest-first.
    unified_snapshots: HashMap<MailboxId, Vec<EmailSummary>>,
    /// The most recently requested body fetch, used to ignore stale
    /// `BodyFetched` updates that arrive after the user has moved on to a
    /// different message.
    pending_body_request: Option<(MailboxId, Uid)>,
    /// A body is currently loading into the reading pane's WebView and
    /// should be revealed when its load finishes. Cleared on every selection
    /// change so a load started for a message the user has already navigated
    /// away from can never pop the pane back open (the persistent
    /// `load-changed` handler in the reading-pane build consults this before
    /// revealing).
    pending_html_reveal: bool,
    /// The summary of the message whose body is next to be revealed. The
    /// reading-pane header is updated from this only when the message page
    /// actually renders (`render_body`), never when the selection changes -
    /// so the previous message's header stays on screen for the whole
    /// fade-out instead of being swapped to the next email mid-fade.
    /// Cleared/overwritten on every selection change.
    pending_header: Option<EmailSummary>,
    /// The most recently fetched message body, kept alongside its
    /// mailbox/uid so Reply/Reply-All/Forward can find it again without a
    /// second fetch - see `selected_message_reply_context`. `None` until the
    /// first `BodyFetched` event lands, or if it's for a message other than
    /// what's currently selected.
    current_body: Option<(MailboxId, Uid, EmailBody)>,
    /// The folder pane's last-selected view, loaded from disk at startup and
    /// rewritten whenever the selection changes (`select_mailbox` /
    /// `enter_unified_inbox`), so the pane reopens where the user left off -
    /// see `last_view`. `None` on the very first run, when the pane defaults
    /// to the "All Inboxes" unified view.
    last_selection: Option<LastSelection>,
    /// True while a remembered selection still has to be restored on startup.
    /// The restore is retried on every `FoldersUpdated` until the remembered
    /// mailbox's account has connected (see `restore_or_default_initial_view`),
    /// so a slow account can't cause its folder to be skipped; the default
    /// "All Inboxes" fallback only applies once restore is done or abandoned.
    /// Stops being relevant the moment the user clicks anything.
    restore_pending: bool,
    /// The `(mailbox, uid)` of the message whose body is currently displayed
    /// on the reading pane's "message" page. Lets the message-selection
    /// handler tell a re-selection of the already-open email apart from a
    /// real navigation, so a list rebuild that keeps the same row selected
    /// doesn't route the pane through "empty" and crossfade the same email
    /// again. `None` while the pane shows nothing.
    rendered_message: Option<(MailboxId, Uid)>,
    /// Mailboxes with a `SyncMailbox` request outstanding - sent but not yet
    /// answered by a `MessagesUpdated`. The startup burst (the session's
    /// cache replay plus the app's on-demand syncs) would otherwise queue
    /// several identical inbox syncs; dedupe them in `request_mailbox_sync`.
    /// Entries are cleared when the mailbox delivers, or when its account
    /// reconnects and sends fresh folders, so a request that dies with a
    /// dropped connection can't suppress a later one.
    syncing: HashSet<MailboxId>,
    /// How the message list is ordered, set from the list header's sort
    /// controls. Applied in `repopulate_message_list` - the single choke point
    /// every list rebuild passes through - so the order is uniform no matter
    /// which event produced the rebuild.
    sort_key: SortKey,
    /// True for newest/Z-A first, the order the list was hardcoded to before
    /// the sort controls existed.
    sort_descending: bool,
    /// Mailboxes the user has starred in the message-list header, rendered as
    /// a "Favorites" section pinned to the top of the folder tree. Session-only
    /// until GSettings lands, matching the View tab's layout toggles.
    favorites: HashSet<MailboxId>,
}

/// Per-calendar-account state, kept separate from `UiState`/`AccountHandle`
/// (Mail's equivalents) matching the crate's existing per-domain-type
/// separation - Calendar is a wholly independent account set from Mail.
struct CalendarAccountHandle {
    cmd_tx: async_channel::Sender<CalendarCommand>,
    display_name: String,
    /// The account's CalDAV base URL, kept for the Config view's account
    /// overview.
    uri: String,
    calendars: Vec<CalendarInfo>,
    /// Latest reported session state, rendered in the sidebar's "My
    /// calendars" checklist while the account hasn't delivered any
    /// calendars yet (see `refresh_calendar_checklist`).
    connection_state: CalConnectionState,
    /// Latest occurrences for whatever month this account last synced,
    /// keyed by month so a stale resync from one account can't clobber
    /// another account's occurrences for the currently-displayed month -
    /// same "only apply if it matches what's on screen" principle as Mail's
    /// `MessagesUpdated` handling.
    last_occurrences: Vec<EventOccurrence>,
    last_synced_month: Option<chrono::NaiveDate>,
}

struct CalendarUiState {
    accounts: HashMap<AccountId, CalendarAccountHandle>,
    displayed_month: chrono::NaiveDate,
    /// Which calendars (by id) are currently shown - unioned in
    /// `refresh_displayed_calendar_view`, toggled from the sidebar's "My
    /// calendars" checklist. Newly-discovered calendars default to checked
    /// (shown) - see `refresh_calendar_checklist`.
    checked_calendar_ids: HashSet<CalendarId>,
    /// Each calendar's assigned colour, persisted between sessions (see
    /// `calendar_colors`). Kept alongside `checked_calendar_ids` so the
    /// checklist can colour its checkboxes; new calendars are assigned here in
    /// `refresh_calendar_checklist`.
    calendar_colors: calendar_colors::CalendarColorMap,
}

/// Strips `Gtk.Paned`'s default visible grey separator line - the card
/// margins already provide a visual gap between panes (see `card_section`),
/// so a painted handle on top of that just looks like a stray line. The
/// handle keeps a comfortable draggable hit-area (`min-width`/`min-height`);
/// only its painted background/border is removed.
///
/// Also gives the folder card (`.folder-pane`, see `card_section`'s caller)
/// a 50%-alpha black background instead of libadwaita's normal opaque
/// `.card` fill, so the window background image shows through it.
fn install_paned_css() {
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
            background-color: rgba(0, 0, 0, 0.5);
        }
        .folder-pane listview,
        .folder-pane scrolledwindow {
            background-color: transparent;
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
            background-color: black;
        }
        .window-icon-toolbar-background {
            background-color: #2e2e32;
            border-radius: 8px;
        }
        .message-header-subject {
            font-weight: bold;
            font-size: 1.2em;
        }
        .message-header-meta {
            opacity: 0.7;
        }
        .avatar-circle {
            border-radius: 9999px;
            color: white;
            font-weight: bold;
        }
        .avatar-color-0 { background-color: #e57373; }
        .avatar-color-1 { background-color: #64b5f6; }
        .avatar-color-2 { background-color: #81c784; }
        .avatar-color-3 { background-color: #ffb74d; }
        .avatar-color-4 { background-color: #ba68c8; }
        .avatar-color-5 { background-color: #4db6ac; }
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
            background-color: #2e2e32;
        }
        .ribbon-group-label {
            margin-right: 6px;
        }
        button.list-header-action {
            min-width: 24px;
            min-height: 24px;
            padding: 2px;
        }",
    );
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(&display, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    }
}

pub fn build_window(app: &adw::Application, worker: Rc<Worker>) -> adw::ApplicationWindow {
    install_paned_css();

    let quit_action = gio::SimpleAction::new("quit", None);
    quit_action.connect_activate({
        let app = app.clone();
        move |_, _| app.quit()
    });
    app.add_action(&quit_action);

    let about_action = gio::SimpleAction::new("about", None);
    about_action.connect_activate(|_, _| {
        adw::AboutDialog::builder()
            .application_name("Lookout")
            .version(env!("CARGO_PKG_VERSION"))
            .developer_name("Gavin Graham")
            .license_type(gtk::License::Gpl30)
            .comments("A native GNOME mail client for GNOME Online Accounts.")
            .build()
            .present(gtk::Window::NONE);
    });
    app.add_action(&about_action);

    let bg_bytes = include_bytes!("../../../Assets/backgrounds/background2.png");
    let bg_texture = gtk::gdk::Texture::from_bytes(&glib::Bytes::from_static(bg_bytes)).expect("bundled background image should decode");
    let background = gtk::Picture::for_paintable(&bg_texture);
    background.set_content_fit(gtk::ContentFit::Cover);
    background.set_can_shrink(true);
    background.set_hexpand(true);
    background.set_vexpand(true);

    let toast_overlay = adw::ToastOverlay::new();

    let status_page = adw::StatusPage::builder()
        .icon_name("mail-unread-symbolic")
        .title("No Mail Accounts")
        .description("Add an account with Mail enabled in GNOME Online Accounts to get started.")
        .build();
    let open_settings_button = gtk::Button::builder()
        .label("Open Online Accounts Settings")
        .halign(gtk::Align::Center)
        .css_classes(["suggested-action", "pill"])
        .build();
    open_settings_button.connect_clicked(|_| {
        let _ = std::process::Command::new("gnome-control-center").arg("online-accounts").spawn();
    });
    status_page.set_child(Some(&open_settings_button));

    // --- Folder sidebar: one expanded-by-default group per account ---
    let folder_selection = gtk::SingleSelection::new(None::<gio::ListModel>);
    let folder_factory = gtk::SignalListItemFactory::new();
    folder_factory.connect_setup(|_, list_item| {
        let expander = gtk::TreeExpander::new();
        let icon = gtk::Image::builder().icon_size(gtk::IconSize::Normal).build();
        let label = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .margin_top(6)
            .margin_bottom(6)
            .build();
        let row_box = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).build();
        row_box.append(&icon);
        row_box.append(&label);
        expander.set_child(Some(&row_box));
        list_item.downcast_ref::<gtk::ListItem>().unwrap().set_child(Some(&expander));
    });
    folder_factory.connect_bind(|_, list_item| {
        let list_item = list_item.downcast_ref::<gtk::ListItem>().unwrap();
        let Some(row) = list_item.item().and_downcast::<gtk::TreeListRow>() else { return };
        let Some(expander) = list_item.child().and_downcast::<gtk::TreeExpander>() else {
            return;
        };
        expander.set_list_row(Some(&row));
        let Some(boxed) = row.item().and_downcast::<glib::BoxedAnyObject>() else { return };
        let tree_item = boxed.borrow::<TreeItem>();
        let Some(row_box) = expander.child().and_downcast::<gtk::Box>() else { return };
        let Some(icon) = row_box.first_child().and_downcast::<gtk::Image>() else { return };
        let Some(label) = row_box.last_child().and_downcast::<gtk::Label>() else { return };
        match &*tree_item {
            TreeItem::Unified => {
                icon.set_visible(true);
                icon.set_icon_name(Some("mail-inbox-symbolic"));
                label.set_label("All Inboxes");
                label.set_css_classes(&["heading"]);
            }
            TreeItem::Favorites => {
                icon.set_visible(true);
                icon.set_icon_name(Some(themed_icon_name(&["starred-symbolic", "mail-mark-important-symbolic"])));
                label.set_label("Favorites");
                label.set_css_classes(&["heading"]);
            }
            TreeItem::Account(account) => {
                icon.set_visible(false);
                label.set_label(&account.label);
                label.set_css_classes(&["heading"]);
            }
            // A favorite renders exactly like the folder it duplicates.
            TreeItem::Folder(node) | TreeItem::Favorite(node) => {
                let unread = node.mailbox.unread;
                let text = if unread > 0 {
                    format!("{}  ({unread})", node.mailbox.name)
                } else {
                    node.mailbox.name.clone()
                };
                icon.set_visible(true);
                icon.set_icon_name(Some(folder_icon_name(node.mailbox.role)));
                label.set_label(&text);
                label.set_css_classes(&[]);
            }
        }
    });

    let folder_list_view = gtk::ListView::new(Some(folder_selection.clone()), Some(folder_factory));
    let folder_scroller = gtk::ScrolledWindow::builder().child(&folder_list_view).vexpand(true).build();
    let folder_card = card_section(&folder_scroller);
    folder_card.add_css_class("folder-pane");
    folder_card.add_css_class("card-flush-end");
    folder_card.set_margin_end(0);

    // --- Message list ---
    let message_store = gio::ListStore::new::<glib::BoxedAnyObject>();
    let message_selection = gtk::SingleSelection::new(Some(message_store.clone()));
    let last_selection = last_view::load();
    let state = Rc::new(RefCell::new(UiState {
        accounts: HashMap::new(),
        current_account: None,
        current_mailbox: None,
        mail_view: MailView::Single,
        unified_snapshots: HashMap::new(),
        pending_body_request: None,
        pending_html_reveal: false,
        pending_header: None,
        current_body: None,
        last_selection: last_selection.clone(),
        restore_pending: last_selection.is_some(),
        rendered_message: None,
        syncing: HashSet::new(),
        sort_key: SortKey::Date,
        sort_descending: true,
        favorites: HashSet::new(),
    }));
    let reading_stack = gtk::Stack::new();
    let state_clone = state.clone();
    let reading_stack_clone = reading_stack.clone();
    let message_factory = gtk::SignalListItemFactory::new();
    message_factory.connect_setup(move |_, list_item| {
        let list_item = list_item.downcast_ref::<gtk::ListItem>().unwrap();
        // Main vertical box for sender, date, subject
        let vbox = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(10)
            .margin_end(10)
            .build();
        // Top row: sender and date
        let hbox = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(8).build();
        let sender_label = gtk::Label::builder().xalign(0.0).hexpand(true).ellipsize(gtk::pango::EllipsizeMode::End).build();
        let date_label = gtk::Label::builder().xalign(1.0).css_classes(["dim-label", "caption"]).build();
        hbox.append(&sender_label);
        hbox.append(&date_label);
        // Subject label
        let subject_label = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["dim-label"])
            .build();
        vbox.append(&hbox);
        vbox.append(&subject_label);
        // Action box (initially hidden)
        let action_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(0)
            .halign(gtk::Align::End)
            .valign(gtk::Align::Center)
            .margin_end(8)
            .margin_top(4)
            .margin_bottom(4)
            .build();
        action_box.add_css_class("hover-quick-actions");
        let archive_btn = gtk::Button::from_icon_name("mail-archive-symbolic");
        archive_btn.add_css_class("hover-quick-action");
        archive_btn.set_tooltip_text(Some("Archive"));
        let delete_btn = gtk::Button::from_icon_name("user-trash-symbolic");
        delete_btn.add_css_class("hover-quick-action");
        delete_btn.set_tooltip_text(Some("Delete"));
        let reply_btn = gtk::Button::from_icon_name("mail-reply-sender-symbolic");
        reply_btn.add_css_class("hover-quick-action");
        reply_btn.set_tooltip_text(Some("Reply"));
        action_box.append(&archive_btn);
        action_box.append(&delete_btn);
        action_box.append(&reply_btn);
        // Initially hide actions
        action_box.set_visible(false);
        // Overlay to overlay actions on top of content
        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&vbox));
        overlay.add_overlay(&action_box);
        unsafe {
            list_item.set_data("action-box", action_box.clone());
            list_item.set_data("archive-button", archive_btn.clone());
            list_item.set_data("delete-button", delete_btn.clone());
            list_item.set_data("reply-button", reply_btn.clone());
        }
        list_item.set_child(Some(&overlay));
    });
    message_factory.connect_bind(move |_, list_item| {
        let list_item = list_item.downcast_ref::<gtk::ListItem>().unwrap();
        let Some(boxed) = list_item.item().and_downcast::<glib::BoxedAnyObject>() else { return };
        let summary = boxed.borrow::<EmailSummary>().clone();
        let overlay = list_item.child().and_downcast::<gtk::Overlay>().expect("overlay");
        let action_box = unsafe { list_item.data::<gtk::Box>("action-box").expect("action box").as_ref().clone() };
        let vbox = overlay.child().and_downcast::<gtk::Box>().expect("vbox");
        let hbox = vbox.first_child().and_downcast::<gtk::Box>().expect("hbox");
        let sender_label = hbox.first_child().and_downcast::<gtk::Label>().expect("sender label");
        let date_label = hbox.last_child().and_downcast::<gtk::Label>().expect("date label");
        let subject_label = vbox.last_child().and_downcast::<gtk::Label>().expect("subject label");
        let from = summary.from.first().map(|a| a.display_label().to_string()).unwrap_or_else(|| "(unknown)".into());
        // In the unified "All Inboxes" view, stamp each row with its owning
        // account so mail from mixed mailboxes stays readable.
        let account_label = {
            let st = state_clone.borrow();
            if matches!(st.mail_view, MailView::UnifiedInbox) {
                mailbox_account_id(&summary.mailbox)
                    .and_then(|id| st.accounts.get(&id))
                    .map(|h| if h.display_name.is_empty() { h.email.clone() } else { h.display_name.clone() })
            } else {
                None
            }
        };
        sender_label.set_label(&match account_label {
            Some(acc) => format!("{from} · {acc}"),
            None => from,
        });
        sender_label.set_css_classes(if summary.is_unread() { &["heading"] } else { &[] });
        let now = chrono::Utc::now();
        let recent = now.signed_duration_since(summary.date) < chrono::Duration::hours(24);
        let date = glib::DateTime::from_unix_local(summary.date.timestamp())
            .ok()
            .and_then(|dt| dt.format(if recent { "%X" } else { "%x" }).ok())
            .unwrap_or_default();
        date_label.set_label(&date);
        subject_label.set_label(summary.subject.as_deref().unwrap_or("(no subject)"));
        let action_box_for_hover = action_box.clone();
        let hover_controller = gtk::EventControllerMotion::new();
        hover_controller.connect_enter(move |_, _, _| {
            action_box_for_hover.set_visible(true);
        });
        let action_box_for_leave = action_box.clone();
        hover_controller.connect_leave(move |_| {
            action_box_for_leave.set_visible(false);
        });
        overlay.add_controller(hover_controller);
        // Button click handlers
        let state_clone = state_clone.clone();
        let reading_stack_clone = reading_stack_clone.clone();
        let mailbox_for_archive = summary.mailbox.clone();
        let uid_for_archive = summary.uid;
        let mailbox_for_delete = summary.mailbox.clone();
        let uid_for_delete = summary.uid;
        let mailbox_for_reply = summary.mailbox.clone();
        let uid_for_reply = summary.uid;
        let summary_for_reply = summary.clone();
        // Archive button: first child of action_box
        {
            let archive_btn = unsafe { list_item.data::<gtk::Button>("archive-button").expect("archive button").as_ref().clone() };
            let state_for_archive = state_clone.clone();
            archive_btn.connect_clicked(move |_| {
                let Some(account_id) = mailbox_account_id(&mailbox_for_archive) else {
                    return;
                };
                let state = state_for_archive.borrow();
                if let Some(handle) = state.accounts.get(&account_id) {
                    let _ = handle.cmd_tx.send_blocking(AccountCommand::MoveMessage {
                        mailbox: mailbox_for_archive.clone(),
                        uid: uid_for_archive,
                        role: MailboxRole::Archive,
                    });
                }
            });
        }
        // Delete button: second child of action_box
        {
            let delete_btn = unsafe { list_item.data::<gtk::Button>("delete-button").expect("delete button").as_ref().clone() };
            let state_for_delete = state_clone.clone();
            delete_btn.connect_clicked(move |_| {
                let Some(account_id) = mailbox_account_id(&mailbox_for_delete) else {
                    return;
                };
                let state = state_for_delete.borrow();
                if let Some(handle) = state.accounts.get(&account_id) {
                    let _ = handle.cmd_tx.send_blocking(AccountCommand::MoveMessage {
                        mailbox: mailbox_for_delete.clone(),
                        uid: uid_for_delete,
                        role: MailboxRole::Trash,
                    });
                }
            });
        }
        // Reply button: third child of action_box
        {
            let reply_btn = unsafe { list_item.data::<gtk::Button>("reply-button").expect("reply button").as_ref().clone() };
            let state_for_reply = state_clone.clone();
            let reading_stack_for_reply = reading_stack_clone.clone();
            reply_btn.connect_clicked(move |_| {
                let state = state_for_reply.borrow();
                // Check if we have the body for this message
                if let Some((body_mailbox, body_uid, body)) = state.current_body.as_ref() {
                    if *body_mailbox == mailbox_for_reply && *body_uid == uid_for_reply {
                        // We have the body, can reply
                        let Some(account_id) = mailbox_account_id(&mailbox_for_reply) else {
                            return;
                        };
                        if let Some(handle) = state.accounts.get(&account_id) {
                            let from_email = handle.email.clone();
                            let prefill = crate::compose::build_reply_prefill(&summary_for_reply, body, &from_email, crate::compose::ReplyMode::Reply);
                            // Show composer in reading pane
                            let on_done = Rc::new(|| {});
                            let composer = crate::compose::build_compose_view("Reply", from_email, handle.cmd_tx.clone(), prefill, on_done);
                            reading_stack_for_reply.add_named(&composer, Some("compose"));
                            reading_stack_for_reply.set_visible_child_name("compose");
                        }
                    }
                }
            });
        }
    });
    message_factory.connect_unbind(move |_, _| {});
    let message_list_view = gtk::ListView::new(Some(message_selection.clone()), Some(message_factory));
    let message_scroller = gtk::ScrolledWindow::builder().child(&message_list_view).vexpand(true).build();
    // Header row atop the message list: what's being shown on the left (the
    // folder's name over its account, plus the favorite star), and the list's
    // own controls on the right - sync, filter, sort direction, sort key.
    // `title_column`'s hexpand is what pushes the control cluster to the
    // trailing edge, so no spacer widget is needed.
    let folder_title_label = gtk::Label::builder()
        .halign(gtk::Align::Start)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .css_classes(["heading"])
        .label("Inbox")
        .build();
    let account_title_label = gtk::Label::builder()
        .halign(gtk::Align::Start)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .css_classes(["caption", "dim-label"])
        .build();
    let title_column = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .halign(gtk::Align::Start)
        .valign(gtk::Align::Center)
        .hexpand(true)
        .build();
    title_column.append(&folder_title_label);
    title_column.append(&account_title_label);

    let favorite_button = gtk::ToggleButton::builder()
        .icon_name(themed_icon_name(&["non-starred-symbolic", "starred-symbolic", "mail-mark-important-symbolic"]))
        .tooltip_text("Add to Favorites")
        .css_classes(["flat", "list-header-action"])
        .valign(gtk::Align::Center)
        .build();
    // `refresh_list_header` sets the star's state programmatically on every
    // view change; without this guard that would fire `toggled` and re-write
    // the favorites set from under the user.
    let favorite_suppress = Rc::new(Cell::new(false));

    let sync_button = gtk::Button::builder()
        .icon_name(themed_icon_name(&["view-refresh-symbolic", "emblem-synchronizing-symbolic"]))
        .tooltip_text("Sync")
        .css_classes(["flat", "list-header-action"])
        .valign(gtk::Align::Center)
        .build();
    // Honest-disabled, like the Flag button in the command toolbar: a real
    // filter needs unread/flagged plumbing that doesn't exist yet.
    let list_filter_button = gtk::Button::builder()
        .icon_name(themed_icon_name(&["funnel-symbolic", "view-filter-symbolic", "edit-find-symbolic"]))
        .tooltip_text("Filter")
        .css_classes(["flat", "list-header-action"])
        .valign(gtk::Align::Center)
        .sensitive(false)
        .build();
    let sort_direction_button = gtk::ToggleButton::builder()
        .icon_name(sort_direction_icon(true))
        .tooltip_text("Newest first")
        .css_classes(["flat", "list-header-action"])
        .valign(gtk::Align::Center)
        .active(true)
        .build();
    let sort_key_menu = gio::Menu::new();
    sort_key_menu.append(Some("By Date"), Some("win.sort-key('date')"));
    sort_key_menu.append(Some("By Sender"), Some("win.sort-key('sender')"));
    sort_key_menu.append(Some("By Subject"), Some("win.sort-key('subject')"));
    let sort_key_button = gtk::MenuButton::builder()
        .label(SortKey::Date.label())
        .css_classes(["flat"])
        .valign(gtk::Align::Center)
        .menu_model(&sort_key_menu)
        .build();

    let message_header_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .margin_start(10)
        .margin_end(10)
        .margin_top(6)
        .margin_bottom(4)
        .build();
    message_header_row.append(&title_column);
    message_header_row.append(&favorite_button);
    message_header_row.append(&sync_button);
    message_header_row.append(&list_filter_button);
    message_header_row.append(&sort_direction_button);
    message_header_row.append(&sort_key_button);

    let list_header = ListHeader {
        folder_label: folder_title_label,
        account_label: account_title_label,
        favorite_button: favorite_button.clone(),
        favorite_suppress: favorite_suppress.clone(),
    };
    refresh_list_header(&state, &list_header);

    let message_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
    message_box.append(&message_header_row);
    message_box.append(&message_scroller);
    let message_card = card_section(&message_box);
    message_card.add_css_class("card-flush-start");
    message_card.set_margin_start(0);

    // --- Sync button -> re-sync whatever the list is showing. ---
    {
        let state = state.clone();
        sync_button.connect_clicked(move |_| resync_current_view(&state));
    }

    // --- Sort direction toggle -> flip the order and re-sort in place. The
    // re-sort reads the visible list back out of the store rather than
    // re-fetching, so it costs nothing and keeps the selection (see
    // `snapshot_message_store`). ---
    {
        let state = state.clone();
        let message_store = message_store.clone();
        let message_selection = message_selection.clone();
        sort_direction_button.connect_toggled(move |button| {
            let descending = button.is_active();
            button.set_icon_name(sort_direction_icon(descending));
            button.set_tooltip_text(Some(if descending { "Newest first" } else { "Oldest first" }));
            state.borrow_mut().sort_descending = descending;
            resort_message_list(&state, &message_store, &message_selection);
        });
    }

    // --- Sort key -> a stateful action so the menu renders radio checks. The
    // action is added to the window once it exists (see `sort_key_action`
    // below); menu actions resolve through the widget hierarchy at activation
    // time, so registering it after the menu is built is fine. ---
    let sort_key_action = gio::SimpleAction::new_stateful("sort-key", Some(glib::VariantTy::STRING), &SortKey::Date.action_state().to_variant());
    {
        let state = state.clone();
        let message_store = message_store.clone();
        let message_selection = message_selection.clone();
        let sort_key_button = sort_key_button.clone();
        sort_key_action.connect_activate(move |action, parameter| {
            let Some(key) = parameter.and_then(|p| p.str()).and_then(SortKey::from_action_state) else {
                return;
            };
            action.set_state(&key.action_state().to_variant());
            sort_key_button.set_label(key.label());
            state.borrow_mut().sort_key = key;
            resort_message_list(&state, &message_store, &message_selection);
        });
    }

    // --- Favorite star -> add/remove the open folder from the tree's
    // Favorites section. Session-only (see `UiState::favorites`). ---
    {
        let state = state.clone();
        let folder_selection = folder_selection.clone();
        let suppress = favorite_suppress.clone();
        favorite_button.connect_toggled(move |button| {
            if suppress.get() {
                return;
            }
            let Some(mailbox) = state.borrow().current_mailbox.clone() else { return };
            {
                let mut st = state.borrow_mut();
                if button.is_active() {
                    st.favorites.insert(mailbox.clone());
                } else {
                    st.favorites.remove(&mailbox);
                }
            }
            apply_favorite_visual(button, button.is_active());
            // The tree grows/loses a whole section, so it has to be rebuilt -
            // which swaps the model and drops the highlight. Put it back on the
            // folder the user is still looking at.
            rebuild_folder_tree(&state, &folder_selection);
            if let Some(model) = folder_selection.model().and_downcast::<gtk::TreeListModel>() {
                if let Some(index) = find_mailbox_index(&model, &mailbox) {
                    folder_selection.set_selected(index);
                }
            }
        });
    }

    let compose_button = gtk::Button::from_icon_name("mail-message-new-symbolic");
    compose_button.set_tooltip_text(Some("New Message"));

    // Dev-only: load a raw .eml fixture straight into the reading pane,
    // bypassing IMAP entirely - for manually exercising render_body()
    // against test-fixtures/. Compiled out of release builds.
    #[cfg(debug_assertions)]
    let open_eml_button = gtk::Button::from_icon_name("document-open-symbolic");
    #[cfg(debug_assertions)]
    open_eml_button.set_tooltip_text(Some("Open .eml (debug)"));

    // --- Reading pane: WebKit for HTML, GtkTextView for plain text ---
    let _reading_stack = reading_stack.clone();
    let webkit_settings = webkit::Settings::new();
    webkit_settings.set_enable_javascript(false);
    webkit_settings.set_enable_developer_extras(false);
    let web_view = webkit::WebView::builder().settings(&webkit_settings).hexpand(true).vexpand(true).build();
    // Block navigation *away* from the loaded message body (e.g. clicking a
    // link) - but NOT the initial programmatic `load_html()` call itself,
    // which also fires a NavigationAction decision. Distinguish the two via
    // `is_user_gesture()`: a real click is a user gesture, `load_html()` is
    // not. Getting this wrong (blocking unconditionally) silently vetoes
    // every load, which is exactly the "reading pane always blank" bug this
    // fixes - the WebView was never rendering anything because its own
    // initial content load was being cancelled before it started. External
    // links should ideally open in the system browser instead of just being
    // dropped - full "open externally" handling is a Phase 2 refinement.
    web_view.connect_decide_policy(|_view, decision, decision_type| {
        if decision_type == webkit::PolicyDecisionType::NavigationAction {
            let is_user_gesture = decision
                .downcast_ref::<webkit::NavigationPolicyDecision>()
                .and_then(|d| d.navigation_action())
                .map(|action| action.is_user_gesture())
                .unwrap_or(false);
            if is_user_gesture {
                decision.ignore();
                return true;
            }
        }
        false
    });

    let text_view = gtk::TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .wrap_mode(gtk::WrapMode::WordChar)
        .left_margin(12)
        .right_margin(12)
        .top_margin(12)
        .bottom_margin(12)
        .build();
    let text_scroller = gtk::ScrolledWindow::builder().child(&text_view).build();

    // --- Reading-pane header: subject/sender/avatar/To/date, plus a second
    // set of Reply/Reply-All/Forward buttons duplicating the top command
    // toolbar's (see below). Lives inside the "message" page below, so it's
    // visible only while an actual message is shown, not for the empty
    // placeholder or the in-place composer.
    let message_header = crate::message_header::build();

    // The reading pane crossfades between three top-level pages: a single
    // "message" page that groups the message header with the body - so both
    // fade out/in together instead of the header popping out of sync with
    // the crossfading content - plus the "empty" placeholder and the
    // in-place composer. Inside the message page, a small no-transition
    // content stack toggles between the HTML web view and the plain-text
    // view without disturbing the outer crossfade.
    let reading_stack = gtk::Stack::new();
    let content_stack = gtk::Stack::new();
    content_stack.set_transition_type(gtk::StackTransitionType::None);
    content_stack.add_named(&web_view, Some("html"));
    content_stack.add_named(&text_scroller, Some("text"));
    let message_page = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
    message_page.append(&message_header.widget);
    message_page.append(&content_stack);
    reading_stack.add_named(&message_page, Some("message"));
    let reading_empty = gtk::Box::new(gtk::Orientation::Vertical, 0);
    reading_stack.add_named(&reading_empty, Some("empty"));
    reading_stack.set_visible_child_name("empty");
    // Interpolated crossfade between the reading pane's pages so a
    // message's header + body fade out and the next fades in instead of
    // snapping. Message switches already pass through the "empty" page
    // (the selection handler flips there before the body arrives), so both
    // halves of the transition fire for free - `render_body` handles the
    // same-page re-render case by routing through "empty" explicitly.
    reading_stack.set_transition_type(gtk::StackTransitionType::Crossfade);
    reading_stack.set_transition_duration(100);
    // WebKit paints asynchronously - revealing the HTML page while a fresh
    // body is still loading would show a blank/white page before the message
    // appears. So the body is loaded while the pane holds on "empty", and a
    // single persistent `load-changed` handler reveals the page only once the
    // load completes. `render_body` arms this with `pending_html_reveal`; the
    // selection handler disarms it whenever the user moves on, so a load
    // started for a stale message can never yank the pane back open (the
    // older per-render one-shot handlers instead fired on *any* Finished and
    // caused double reveals / stale emails appearing).
    let state_for_reveal = state.clone();
    let content_stack_for_reveal = content_stack.clone();
    let reading_stack_for_reveal = reading_stack.clone();
    web_view.connect_load_changed(move |_, event| {
        if event != webkit::LoadEvent::Finished {
            return;
        }
        let armed = state_for_reveal.borrow_mut().pending_html_reveal;
        if armed {
            state_for_reveal.borrow_mut().pending_html_reveal = false;
            reveal_message_page(&reading_stack_for_reveal, &content_stack_for_reveal, "html");
        }
    });
    // Floor so the reading pane never gets squeezed down to something
    // unusably short if the window itself is resized very short - since
    // both Paneds here are horizontal, every pane always spans the full
    // window height, so this is really a minimum-window-height constraint
    // expressed on the pane that most needs it. Set directly on the Stack
    // (not on `text_scroller`'s child) because `Gtk.ScrolledWindow`
    // deliberately absorbs its child's size request instead of propagating
    // it - setting it here, on the Stack itself, is what actually reaches
    // the window's own size negotiation.
    reading_stack.set_size_request(-1, 300);

    let reading_pane_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
    reading_pane_box.append(&reading_stack);

    let reading_card = card_section(&reading_pane_box);

    // Keep the reading pane's card fully transparent while it's showing the
    // "no message selected" placeholder, so the window background image
    // shows straight through with no card tint - matching `.folder-pane`'s
    // translucency, but taken all the way to zero alpha since there's no
    // content to read against here. Driven off the stack's own signal
    // (rather than each `set_visible_child_name` call site) so every path
    // that flips back to "empty" - initial state above, `render_body`'s
    // fallback, and the reset on account disconnect - is covered for free.
    let update_reading_card_transparency = {
        let reading_card = reading_card.clone();
        move |stack: &gtk::Stack| {
            if stack.visible_child_name().as_deref() == Some("empty") {
                reading_card.add_css_class("reading-pane-transparent");
            } else {
                reading_card.remove_css_class("reading-pane-transparent");
            }
        }
    };
    update_reading_card_transparency(&reading_stack);
    reading_stack.connect_notify_local(Some("visible-child-name"), move |stack, _| {
        update_reading_card_transparency(stack);
    });

    // --- Resizable panes: folders | (messages | reading), each its own
    // rounded card. `resize_start_child(false)` keeps the sidebar-like
    // panes from silently growing when the window resizes; the reading
    // pane absorbs the extra space.
    let messages_reading_paned = gtk::Paned::builder()
        .orientation(gtk::Orientation::Horizontal)
        .start_child(&message_card)
        .end_child(&reading_card)
        .resize_start_child(false)
        .resize_end_child(true)
        .shrink_start_child(false)
        .shrink_end_child(false)
        .position(320)
        .build();
    let main_paned = gtk::Paned::builder()
        .orientation(gtk::Orientation::Horizontal)
        .start_child(&folder_card)
        .end_child(&messages_reading_paned)
        .resize_start_child(false)
        .resize_end_child(true)
        .shrink_start_child(false)
        .shrink_end_child(false)
        .position(220)
        .build();
    main_paned.add_css_class("seamless-paned");

    let status_page_as_widget: gtk::Widget = status_page.clone().upcast();
    let main_paned_as_widget: gtk::Widget = main_paned.clone().upcast();
    let root_stack = gtk::Stack::new();
    root_stack.add_named(&status_page_as_widget, Some("empty"));
    root_stack.add_named(&main_paned_as_widget, Some("mail"));
    root_stack.set_visible_child_name("empty");

    let calendar_status_page = adw::StatusPage::builder()
        .icon_name("x-office-calendar-symbolic")
        .title("No Calendar Accounts")
        .description("Add an account with Calendar enabled in GNOME Online Accounts to see events here.")
        .build();
    let calendar_main = Rc::new(calendar_view::build_main());
    let calendar_sidebar = calendar_view::build_sidebar();
    let calendar_sidebar_card = card_section(&calendar_sidebar.root);
    calendar_sidebar_card.add_css_class("folder-pane");
    let calendar_paned = gtk::Paned::builder()
        .orientation(gtk::Orientation::Horizontal)
        .start_child(&calendar_sidebar_card)
        .end_child(&calendar_main.root)
        .resize_start_child(false)
        .resize_end_child(true)
        .shrink_start_child(false)
        .shrink_end_child(false)
        .position(260)
        .build();
    calendar_paned.add_css_class("seamless-paned");
    root_stack.add_named(&calendar_status_page, Some("calendar-empty"));
    root_stack.add_named(&calendar_paned, Some("calendar"));

    // The one real title bar for the window - owns the actual
    // minimize/maximize/close buttons. The per-card header bars inside
    // `root_stack` are explicitly told not to show these (see
    // `card_section`), so there's exactly one set, not four.
    let window_header = adw::HeaderBar::new();
    window_header.set_title_widget(Some(&adw::WindowTitle::new("Lookout", "")));
    #[cfg(debug_assertions)]
    window_header.pack_end(&open_eml_button);

    // --- Menu bar row (File/Home/View/Help). File (Quit) and Help (About)
    // are `MenuButton`s with a real popover; Home/View are the ribbon's tab
    // strip - mutually-exclusive toggle buttons that switch the ribbon
    // content row below (see `view_toolbar_stack`). Home holds the command
    // toolbar; View holds the pane-visibility layout toggles. Mail-only:
    // they're disabled while a Calendar/Config module is active (see the
    // nav-rail handlers), matching the codebase's honest-disabled convention.
    let file_menu = gio::Menu::new();
    file_menu.append(Some("Quit"), Some("app.quit"));
    let help_menu = gio::Menu::new();
    help_menu.append(Some("About Lookout"), Some("app.about"));

    let file_button = gtk::MenuButton::builder().label("File").css_classes(["flat"]).menu_model(&file_menu).build();
    let home_button = gtk::ToggleButton::builder().label("Home").css_classes(["flat", "ribbon-tab"]).active(true).build();
    let view_button = gtk::ToggleButton::builder().label("View").css_classes(["flat", "ribbon-tab"]).build();
    view_button.set_group(Some(&home_button));
    let help_button = gtk::MenuButton::builder().label("Help").css_classes(["flat"]).menu_model(&help_menu).build();

    // Which ribbon tab is active ("home" | "view") and which nav-rail module
    // is selected ("mail" | "calendar" | "config") - together they decide the
    // `view_toolbar_stack`'s visible child (see `ribbon_stack_name`). The tab
    // state persists across module switches; the nav-rail handlers re-enable
    // Home/View on Mail and disable them elsewhere.
    let active_ribbon_tab: Rc<Cell<&'static str>> = Rc::new(Cell::new("home"));
    let current_module: Rc<Cell<&'static str>> = Rc::new(Cell::new("mail"));

    let menu_bar = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(2).css_classes(["toolbar"]).build();
    menu_bar.append(&file_button);
    menu_bar.append(&home_button);
    menu_bar.append(&view_button);
    menu_bar.append(&help_button);

    // --- Command toolbar row. `compose_button`, `reply_button`,
    // `reply_all_button`, `forward_button`, `delete_button`,
    // `archive_button`, `report_button`, and `snooze_button` are backed by
    // real functionality; `flag_button`/`more_button` mirror Outlook's row
    // visually but are disabled since Lookout doesn't implement
    // flag/unflag or the "More" menu yet.
    let reply_button = gtk::Button::from_icon_name("mail-reply-sender-symbolic");
    reply_button.set_tooltip_text(Some("Reply"));
    let reply_all_button = gtk::Button::from_icon_name("mail-reply-all-symbolic");
    reply_all_button.set_tooltip_text(Some("Reply All"));
    let forward_button = gtk::Button::from_icon_name("mail-forward-symbolic");
    forward_button.set_tooltip_text(Some("Forward"));
    let delete_button = gtk::Button::from_icon_name("user-trash-symbolic");
    delete_button.set_tooltip_text(Some("Delete"));
    let archive_button = gtk::Button::from_icon_name("mail-archive-symbolic");
    archive_button.set_tooltip_text(Some("Archive"));
    let report_button = gtk::Button::from_icon_name("mail-mark-junk-symbolic");
    report_button.set_tooltip_text(Some("Report"));
    let flag_button = gtk::Button::from_icon_name("mail-mark-important-symbolic");
    flag_button.set_tooltip_text(Some("Flag/Unflag"));
    flag_button.set_sensitive(false);
    let snooze_button = gtk::Button::from_icon_name("appointment-soon-symbolic");
    snooze_button.set_tooltip_text(Some("Snooze"));
    let more_button = gtk::Button::from_icon_name("view-more-symbolic");
    more_button.set_tooltip_text(Some("More"));
    more_button.set_sensitive(false);

    let command_toolbar = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).css_classes(["toolbar"]).build();
    command_toolbar.append(&compose_button);
    command_toolbar.append(&reply_button);
    command_toolbar.append(&reply_all_button);
    command_toolbar.append(&forward_button);
    command_toolbar.append(&delete_button);
    command_toolbar.append(&archive_button);
    command_toolbar.append(&report_button);
    command_toolbar.append(&flag_button);
    command_toolbar.append(&snooze_button);
    command_toolbar.append(&more_button);

    // --- Calendar's own command toolbar row, swapped in for the Mail one
    // (see `view_toolbar_stack` below) when the Calendar nav-rail button is
    // active. All five segmented options (Day/Work week/Week/Month/Split)
    // switch the main panel's stack; the rest of the toolbar mirrors the
    // Mail toolbar's disabled-placeholder convention.
    let new_event_button = gtk::Button::from_icon_name("appointment-new-symbolic");
    new_event_button.set_tooltip_text(Some("New Event"));
    new_event_button.set_sensitive(false);

    let day_view_button = gtk::ToggleButton::builder().label("Day").build();
    let work_week_view_button = gtk::ToggleButton::builder().label("Work week").build();
    let week_view_button = gtk::ToggleButton::builder().label("Week").build();
    let month_view_button = gtk::ToggleButton::builder().label("Month").active(true).build();
    let split_view_button = gtk::ToggleButton::builder().label("Split view").build();
    // One mutual-exclusion group so exactly one view is active at a time
    // (same `set_group` trick as the nav rail's Mail/Calendar buttons).
    split_view_button.set_group(Some(&month_view_button));
    week_view_button.set_group(Some(&split_view_button));
    work_week_view_button.set_group(Some(&week_view_button));
    day_view_button.set_group(Some(&work_week_view_button));
    let view_switch_box = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).css_classes(["linked"]).build();
    view_switch_box.append(&day_view_button);
    view_switch_box.append(&work_week_view_button);
    view_switch_box.append(&week_view_button);
    view_switch_box.append(&month_view_button);
    view_switch_box.append(&split_view_button);
    for (button, view) in [
        (&day_view_button, "day"),
        (&work_week_view_button, "workweek"),
        (&week_view_button, "week"),
        (&month_view_button, "month"),
        (&split_view_button, "split"),
    ] {
        let calendar_main = calendar_main.clone();
        button.connect_toggled(move |btn| {
            if btn.is_active() {
                calendar_view::set_view(&calendar_main, view);
            }
        });
    }

    let filter_button = gtk::Button::from_icon_name("edit-find-symbolic");
    filter_button.set_tooltip_text(Some("Filter"));
    filter_button.set_sensitive(false);
    let share_button = gtk::Button::from_icon_name("send-to-symbolic");
    share_button.set_tooltip_text(Some("Share"));
    share_button.set_sensitive(false);
    let print_button = gtk::Button::from_icon_name("printer-symbolic");
    print_button.set_tooltip_text(Some("Print"));
    print_button.set_sensitive(false);

    let calendar_command_toolbar = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).css_classes(["toolbar"]).build();
    calendar_command_toolbar.append(&new_event_button);
    calendar_command_toolbar.append(&view_switch_box);
    calendar_command_toolbar.append(&filter_button);
    calendar_command_toolbar.append(&share_button);
    calendar_command_toolbar.append(&print_button);

    // --- View tab's ribbon content (Mail module): a "Layout" group of
    // pane-visibility toggles - Folder pane / Reading pane / Calendar
    // overview. All three default on; their click handlers live in a later
    // block (after every pane widget exists) and are session-only toggles
    // until Phase 5's GSettings landing.
    let layout_label = gtk::Label::builder().label("Layout").css_classes(["ribbon-group-label", "dim-label"]).build();
    let folder_pane_toggle = gtk::ToggleButton::builder().icon_name("folder-symbolic").build();
    folder_pane_toggle.set_tooltip_text(Some("Folder pane"));
    folder_pane_toggle.set_active(true);
    let reading_pane_toggle = gtk::ToggleButton::builder().icon_name("document-preview-symbolic").build();
    reading_pane_toggle.set_tooltip_text(Some("Reading pane"));
    reading_pane_toggle.set_active(true);
    let overview_pane_toggle = gtk::ToggleButton::builder().icon_name("x-office-calendar-symbolic").build();
    overview_pane_toggle.set_tooltip_text(Some("Calendar overview"));
    overview_pane_toggle.set_active(true);
    let view_toolbar = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).css_classes(["toolbar"]).build();
    view_toolbar.append(&layout_label);
    view_toolbar.append(&folder_pane_toggle);
    view_toolbar.append(&reading_pane_toggle);
    view_toolbar.append(&overview_pane_toggle);

    let view_toolbar_stack = gtk::Stack::new();
    view_toolbar_stack.add_named(&command_toolbar, Some("mail-home"));
    view_toolbar_stack.add_named(&view_toolbar, Some("mail-view"));
    view_toolbar_stack.add_named(&calendar_command_toolbar, Some("calendar"));

    // --- View-switcher rail: a narrow, deliberately unstyled (no `.card`,
    // no background) strip along the window's left edge so the background
    // image shows straight through it. Two views today (Mail/Calendar),
    // joined into one toggle group for mutual-exclusive selection.
    let mail_icon_bytes = include_bytes!("../../../data/icons/hicolor/scalable/apps/io.github.gavindi.Lookout.svg");
    let mail_icon_texture = gtk::gdk::Texture::from_bytes(&glib::Bytes::from_static(mail_icon_bytes)).expect("bundled app icon should decode");
    let mail_icon_image = gtk::Image::from_paintable(Some(&mail_icon_texture));
    mail_icon_image.set_pixel_size(28);
    let mail_view_button = gtk::ToggleButton::builder()
        .child(&mail_icon_image)
        .css_classes(["flat"])
        .tooltip_text("Mail")
        .active(true)
        .build();
    let calendar_view_button = gtk::ToggleButton::builder()
        .icon_name("x-office-calendar-symbolic")
        .css_classes(["flat"])
        .tooltip_text("Calendar")
        .build();
    calendar_view_button.set_group(Some(&mail_view_button));

    // `vexpand(true)` so the rail stretches the window's full height (it
    // sits beside `outer_toolbar_view` - header bar, menu bar, and command
    // toolbar included - rather than below those top bars).
    let nav_rail = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .width_request(56)
        .margin_top(6)
        .margin_start(6)
        .spacing(6)
        .vexpand(true)
        .build();
    nav_rail.append(&mail_view_button);
    nav_rail.append(&calendar_view_button);

    // --- Mail-screen calendar overview pane: a mini month-picker + a list
    // of the clicked day's events, docked to the far right of the window,
    // spanning the same full height as `nav_rail` (it's a sibling in
    // `window_body`, not nested inside `root_stack`). Mail-only - the
    // Calendar view already has its own full sidebar with a mini-calendar.
    let mail_calendar_overview = calendar_view::build_mini();
    let mail_overview_day_list = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).margin_top(8).build();
    // Matches `build_sidebar()`'s own width_request - without an explicit
    // cap here, the mini-calendar's day-button grid requests its natural
    // (much wider) size instead of a compact peek-pane width.
    let mail_overview_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).width_request(140).build();
    mail_overview_box.append(&mail_calendar_overview.root);
    mail_overview_box.append(&mail_overview_day_list);

    let mail_calendar_overview_card = card_section(&mail_overview_box);
    mail_calendar_overview_card.add_css_class("folder-pane");
    mail_calendar_overview_card.set_vexpand(true);

    // --- Ribbon tab strip + View-tab pane toggles. Home/View swap the
    // ribbon content row (`view_toolbar_stack`) between Mail's command
    // toolbar and the Layout toggles; the three toggles show/hide their
    // pane. Home/View only make sense on the Mail module - the Calendar/
    // Config nav handlers disable them below, so the tab buttons can't be
    // clicked there (and the `if btn.is_active()` guard makes re-entrant
    // toggles no-ops anyway).
    {
        let view_toolbar_stack = view_toolbar_stack.clone();
        let active_ribbon_tab = active_ribbon_tab.clone();
        let current_module = current_module.clone();
        home_button.connect_toggled(move |btn| {
            if btn.is_active() {
                active_ribbon_tab.set("home");
                view_toolbar_stack.set_visible_child_name(ribbon_stack_name(current_module.get(), "home"));
            }
        });
    }
    {
        let view_toolbar_stack = view_toolbar_stack.clone();
        let active_ribbon_tab = active_ribbon_tab.clone();
        let current_module = current_module.clone();
        view_button.connect_toggled(move |btn| {
            if btn.is_active() {
                active_ribbon_tab.set("view");
                view_toolbar_stack.set_visible_child_name(ribbon_stack_name(current_module.get(), "view"));
            }
        });
    }
    {
        let folder_card = folder_card.clone();
        folder_pane_toggle.connect_toggled(move |btn| {
            folder_card.set_visible(btn.is_active());
        });
    }
    {
        let reading_card = reading_card.clone();
        reading_pane_toggle.connect_toggled(move |btn| {
            reading_card.set_visible(btn.is_active());
        });
    }
    {
        let mail_calendar_overview_card = mail_calendar_overview_card.clone();
        overview_pane_toggle.connect_toggled(move |btn| {
            mail_calendar_overview_card.set_visible(btn.is_active());
        });
    }

    // Which sub-page each view should show when its nav-rail button becomes
    // active - kept up to date by the discovery/event handlers below (which
    // only actually flip `root_stack`'s visible child if their own button is
    // the one currently active, so a background sync on the other view
    // never yanks the screen out from under whichever one the user is
    // looking at).
    let current_mail_page: Rc<Cell<&'static str>> = Rc::new(Cell::new("empty"));
    let current_calendar_page: Rc<Cell<&'static str>> = Rc::new(Cell::new("calendar-empty"));
    {
        let root_stack = root_stack.clone();
        let current_mail_page = current_mail_page.clone();
        let view_toolbar_stack = view_toolbar_stack.clone();
        let mail_calendar_overview_card = mail_calendar_overview_card.clone();
        let active_ribbon_tab = active_ribbon_tab.clone();
        let current_module = current_module.clone();
        let overview_pane_toggle = overview_pane_toggle.clone();
        let home_button = home_button.clone();
        let view_button = view_button.clone();
        mail_view_button.connect_toggled(move |btn| {
            if btn.is_active() {
                current_module.set("mail");
                root_stack.set_visible_child_name(current_mail_page.get());
                view_toolbar_stack.set_visible_child_name(ribbon_stack_name("mail", active_ribbon_tab.get()));
                // Respect the View tab's toggle rather than forcing the
                // overview pane back on after a Calendar/Config round-trip.
                mail_calendar_overview_card.set_visible(overview_pane_toggle.is_active());
                home_button.set_sensitive(true);
                view_button.set_sensitive(true);
            }
        });
    }
    {
        let root_stack = root_stack.clone();
        let current_calendar_page = current_calendar_page.clone();
        let view_toolbar_stack = view_toolbar_stack.clone();
        let mail_calendar_overview_card = mail_calendar_overview_card.clone();
        let current_module = current_module.clone();
        let home_button = home_button.clone();
        let view_button = view_button.clone();
        calendar_view_button.connect_toggled(move |btn| {
            if btn.is_active() {
                current_module.set("calendar");
                root_stack.set_visible_child_name(current_calendar_page.get());
                view_toolbar_stack.set_visible_child_name("calendar");
                mail_calendar_overview_card.set_visible(false);
                home_button.set_sensitive(false);
                view_button.set_sensitive(false);
            }
        });
    }

    // The nav rail runs the full height *below the title bar* - it sits
    // beside the menu bar/command toolbar/mail-or-calendar content, but not
    // beside `window_header` itself, which stays the one real title bar
    // spanning the full window width at the very top.
    root_stack.set_hexpand(true);
    root_stack.set_vexpand(true);

    // The icon command toolbar (Mail's/Calendar's) gets its own dark grey
    // subgroup, distinct from the menu bar's black background.
    let icon_toolbar_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .css_classes(["window-icon-toolbar-background"])
        .overflow(gtk::Overflow::Hidden)
        .margin_start(6)
        .margin_end(6)
        .margin_top(6)
        .margin_bottom(6)
        .build();
    icon_toolbar_box.append(&view_toolbar_stack);

    // Menu bar + icon toolbar grouped onto their own shared black
    // background, rather than each row painting (or not painting) its own.
    let toolbars_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .css_classes(["window-toolbars-background"])
        .build();
    toolbars_box.append(&menu_bar);
    toolbars_box.append(&icon_toolbar_box);

    let inner_content_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).hexpand(true).vexpand(true).build();
    inner_content_box.append(&toolbars_box);
    inner_content_box.append(&root_stack);

    // Resizable split between the main content and the overview pane -
    // `nav_rail` stays a fixed-width sibling outside the split (it isn't
    // meant to be resizable), but the overview pane's width is user-
    // draggable like every other pane split in this app.
    let content_and_overview_paned = gtk::Paned::builder()
        .orientation(gtk::Orientation::Horizontal)
        .start_child(&inner_content_box)
        .end_child(&mail_calendar_overview_card)
        .resize_start_child(true)
        .resize_end_child(false)
        .shrink_start_child(false)
        .shrink_end_child(false)
        .position(1340)
        .build();
    content_and_overview_paned.add_css_class("seamless-paned");

    let window_body = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).build();
    window_body.append(&nav_rail);
    window_body.append(&content_and_overview_paned);

    let outer_toolbar_view = adw::ToolbarView::new();
    outer_toolbar_view.add_top_bar(&window_header);
    outer_toolbar_view.set_content(Some(&window_body));

    toast_overlay.set_child(Some(&outer_toolbar_view));
    toast_overlay.set_hexpand(true);
    toast_overlay.set_vexpand(true);

    let window_overlay = gtk::Overlay::new();
    window_overlay.set_child(Some(&background));
    window_overlay.add_overlay(&toast_overlay);

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Lookout")
        .default_width(1600)
        .default_height(900)
        .content(&window_overlay)
        .build();
    // The message list's sort menu is built long before the window exists;
    // `win.`-scoped actions resolve through the widget hierarchy when the menu
    // item is activated, so registering it here is in time.
    window.add_action(&sort_key_action);

    let state = state.clone();
    let calendar_state = Rc::new(RefCell::new(CalendarUiState {
        accounts: HashMap::new(),
        displayed_month: current_month_start(),
        checked_calendar_ids: HashSet::new(),
        calendar_colors: calendar_colors::load(),
    }));
    // Which single day the Mail-screen overview pane's event list is
    // currently showing - separate from `calendar_state.displayed_month`
    // (that's the main Calendar view's own concern).
    let mail_overview_day: Rc<Cell<chrono::NaiveDate>> = Rc::new(Cell::new(chrono::Utc::now().date_naive()));
    refresh_mail_overview_day_list(&calendar_state, mail_overview_day.get(), &mail_overview_day_list);

    // --- Config view: the third nav-rail view, a read-only overview of the
    // connected Mail/Calendar accounts (endpoints included, so it shows how
    // each account is configured) plus the Phase 5 placeholder sections, and
    // an "Add account" entry that opens GOA settings - same invocation as the
    // empty-state page's button. The account groups are repopulated by
    // `refresh_config` on every activation and again whenever either
    // discovery lands (`spawn_*_discovery` below).
    let config_view = Rc::new(crate::config_view::build());
    let config_card = card_section(&config_view.root);
    config_card.add_css_class("folder-pane");
    root_stack.add_named(&config_card, Some("config"));

    // Config → Appearance → "Animate transitions": flips the reading pane's
    // crossfade on/off live. Session-only state until Phase 5's GSettings
    // lands; off sets the transition type to `None`, which also makes
    // `render_body` skip its fade-specific dance (see below).
    {
        let reading_stack = reading_stack.clone();
        config_view.animations_row.connect_active_notify(move |row| {
            let transition = if row.is_active() {
                gtk::StackTransitionType::Crossfade
            } else {
                gtk::StackTransitionType::None
            };
            reading_stack.set_transition_type(transition);
        });
    }

    {
        let add_account_row = config_view.add_account_row.clone();
        add_account_row.connect_activated(|_| {
            let _ = std::process::Command::new("gnome-control-center").arg("online-accounts").spawn();
        });
    }

    // --- Config's own command-toolbar row, swapped in via `view_toolbar_stack`
    // like Mail's and Calendar's when the Config nav-rail button is active.
    let config_add_account_button = gtk::Button::from_icon_name("contact-new-symbolic");
    config_add_account_button.set_tooltip_text(Some("Add account"));
    config_add_account_button.connect_clicked(|_| {
        let _ = std::process::Command::new("gnome-control-center").arg("online-accounts").spawn();
    });
    let config_command_toolbar = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).css_classes(["toolbar"]).build();
    config_command_toolbar.append(&config_add_account_button);
    view_toolbar_stack.add_named(&config_command_toolbar, Some("config"));

    let config_view_button = gtk::ToggleButton::builder()
        .icon_name("preferences-system-symbolic")
        .css_classes(["flat"])
        .tooltip_text("Config")
        .build();
    config_view_button.set_group(Some(&calendar_view_button));
    // Anchored to the bottom of the rail: Mail/Calendar stay at the top, a
    // `vexpand(true)` spacer fills the middle, Config sits below it.
    let nav_rail_spacer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    nav_rail_spacer.set_vexpand(true);
    nav_rail.append(&nav_rail_spacer);
    nav_rail.append(&config_view_button);

    let refresh_config: Rc<dyn Fn()> = Rc::new({
        let state = state.clone();
        let calendar_state = calendar_state.clone();
        let config_view = config_view.clone();
        move || {
            let mut mail: Vec<crate::config_view::MailAccountInfo> = state
                .borrow()
                .accounts
                .values()
                .map(|h| crate::config_view::MailAccountInfo {
                    display_name: h.display_name.clone(),
                    email: h.email.clone(),
                    imap: format!("{}:{}", h.imap_host, h.imap_port),
                    smtp: format!("{}:{}", h.smtp_host, h.smtp_port),
                })
                .collect();
            mail.sort_by_key(|a| a.email.to_lowercase());
            let mut calendar: Vec<crate::config_view::CalendarAccountInfo> = calendar_state
                .borrow()
                .accounts
                .values()
                .map(|h| crate::config_view::CalendarAccountInfo {
                    display_name: h.display_name.clone(),
                    uri: h.uri.clone(),
                })
                .collect();
            calendar.sort_by_key(|a| a.display_name.to_lowercase());
            crate::config_view::refresh(&config_view, &mail, &calendar);
        }
    });
    // Populate the placeholder rows now (both groups are empty at startup).
    refresh_config();

    {
        let root_stack = root_stack.clone();
        let view_toolbar_stack = view_toolbar_stack.clone();
        let mail_calendar_overview_card = mail_calendar_overview_card.clone();
        let refresh_config = refresh_config.clone();
        let current_module = current_module.clone();
        let home_button = home_button.clone();
        let view_button = view_button.clone();
        config_view_button.connect_toggled(move |btn| {
            if btn.is_active() {
                current_module.set("config");
                root_stack.set_visible_child_name("config");
                view_toolbar_stack.set_visible_child_name("config");
                mail_calendar_overview_card.set_visible(false);
                home_button.set_sensitive(false);
                view_button.set_sensitive(false);
                refresh_config();
            }
        });
    }

    // --- "Clear all caches" (Config → Advanced): deletes the on-disk mail
    // cache, drops the in-memory calendar occurrences, and asks every
    // connected account to resync so the caches rebuild from the servers
    // right away rather than on next launch ---
    {
        let state = state.clone();
        let calendar_state = calendar_state.clone();
        let calendar_main = calendar_main.clone();
        let mail_overview_day = mail_overview_day.clone();
        let mail_overview_day_list = mail_overview_day_list.clone();
        let toast_overlay = toast_overlay.clone();
        config_view.clear_cache_row.connect_activated(move |_| {
            match (lookout_mail::clear_all_caches(), lookout_dav::clear_all_caches()) {
                (Ok(()), Ok(())) => toast_overlay.add_toast(adw::Toast::new("Cleared email and calendar caches")),
                (Err(e), _) => toast_overlay.add_toast(adw::Toast::new(&format!("Couldn't clear caches: {e}"))),
                (_, Err(e)) => toast_overlay.add_toast(adw::Toast::new(&format!("Couldn't clear caches: {e}"))),
            }
            let month = calendar_state.borrow().displayed_month;
            for handle in calendar_state.borrow_mut().accounts.values_mut() {
                handle.last_occurrences.clear();
                handle.last_synced_month = None;
                let _ = handle.cmd_tx.send_blocking(CalendarCommand::SyncMonth(month));
            }
            refresh_displayed_calendar_view(&calendar_state, &calendar_main);
            refresh_mail_overview_day_list(&calendar_state, mail_overview_day.get(), &mail_overview_day_list);
            for handle in state.borrow().accounts.values() {
                let _ = handle.cmd_tx.send_blocking(AccountCommand::Refresh);
            }
        });
    }

    // --- Compose button -> new-message composer in the reading pane,
    // "From" = the account owning the selected message (falling back to the
    // currently-open mailbox's account, then any connected account) ---
    {
        let state = state.clone();
        let reading_stack = reading_stack.clone();
        let message_selection = message_selection.clone();
        compose_button.connect_clicked(move |_| {
            let st = state.borrow();
            let account_id = message_selection
                .selected_item()
                .and_downcast::<glib::BoxedAnyObject>()
                .and_then(|b| {
                    let summary = b.borrow::<EmailSummary>();
                    mailbox_account_id(&summary.mailbox)
                })
                .or_else(|| st.current_account.clone())
                .or_else(|| st.accounts.keys().next().cloned());
            let Some(handle) = account_id.and_then(|id| st.accounts.get(&id)) else { return };
            let cmd_tx = handle.cmd_tx.clone();
            let from_email = handle.email.clone();
            drop(st);
            show_composer_in_reading_pane(&reading_stack, "New Message", from_email, cmd_tx, crate::compose::ComposePrefill::default());
        });
    }

    // --- Reply/Reply-All/Forward -> opens the composer in the reading pane
    // pre-filled from whatever message is currently selected and has a body
    // loaded. Silent no-op if nothing's selected or the body hasn't arrived
    // yet (same convention as the Delete/Archive/Report/Snooze buttons below).
    // Wired twice - once for the top command toolbar's buttons, once for the
    // reading-pane header's own copies - by design, see the plan this
    // shipped under.
    for (button, mode, title) in [
        (&reply_button, crate::compose::ReplyMode::Reply, "Reply"),
        (&message_header.reply_button, crate::compose::ReplyMode::Reply, "Reply"),
        (&reply_all_button, crate::compose::ReplyMode::ReplyAll, "Reply All"),
        (&message_header.reply_all_button, crate::compose::ReplyMode::ReplyAll, "Reply All"),
    ] {
        let message_selection = message_selection.clone();
        let state = state.clone();
        let reading_stack = reading_stack.clone();
        button.connect_clicked(move |_| {
            if let Some((summary, body, from_email, cmd_tx)) = selected_message_reply_context(&message_selection, &state) {
                let prefill = crate::compose::build_reply_prefill(&summary, &body, &from_email, mode);
                show_composer_in_reading_pane(&reading_stack, title, from_email, cmd_tx, prefill);
            }
        });
    }
    for button in [&forward_button, &message_header.forward_button] {
        let message_selection = message_selection.clone();
        let state = state.clone();
        let reading_stack = reading_stack.clone();
        button.connect_clicked(move |_| {
            if let Some((summary, body, from_email, cmd_tx)) = selected_message_reply_context(&message_selection, &state) {
                let prefill = crate::compose::build_forward_prefill(&summary, &body);
                show_composer_in_reading_pane(&reading_stack, "Forward", from_email, cmd_tx, prefill);
            }
        });
    }

    // --- Debug: open a raw .eml fixture straight into the reading pane ---
    #[cfg(debug_assertions)]
    {
        let window = window.clone();
        let state = state.clone();
        let message_header = message_header.clone();
        let reading_stack = reading_stack.clone();
        open_eml_button.connect_clicked(move |_| {
            let window = window.clone();
            let state = state.clone();
            let message_header = message_header.clone();
            let reading_stack = reading_stack.clone();
            glib::spawn_future_local(async move {
                let filter = gtk::FileFilter::new();
                filter.add_suffix("eml");
                filter.set_name(Some("Email messages (*.eml)"));
                let filters = gio::ListStore::new::<gtk::FileFilter>();
                filters.append(&filter);

                let dialog = gtk::FileDialog::builder().title("Open .eml (debug)").filters(&filters).build();

                let Ok(file) = dialog.open_future(Some(&window)).await else { return };
                let Some(path) = file.path() else { return };
                let Ok(raw) = std::fs::read(&path) else { return };
                if let Some(body) = lookout_mail::body::parse_body(lookout_core::Uid(0), &raw) {
                    render_body(&state, &reading_stack, &message_header, MailboxId("debug:eml".into()), body.uid, body);
                }
            });
        });
    }

    // --- Network connectivity -> nudge every backed-off account to retry now ---
    {
        let state = state.clone();
        let monitor = gio::NetworkMonitor::default();
        monitor.connect_network_changed(move |_monitor, available| {
            if !available {
                return;
            }
            for handle in state.borrow().accounts.values() {
                let _ = handle.cmd_tx.send_blocking(AccountCommand::Reconnect);
            }
        });
    }

    // --- Folder selection -> AccountCommand::SyncMailbox on that folder's
    // own account (selecting an account-group row, or the Favorites section
    // header, is a no-op - it just expands/collapses). The synthetic "All
    // Inboxes" row instead enters the unified view. A `Favorite` row is the
    // same mailbox as its `Folder` row, so it takes the identical path. Every
    // live path re-renders the list header, which is the one place both view
    // changes funnel through. ---
    {
        let state = state.clone();
        let message_store = message_store.clone();
        let message_selection = message_selection.clone();
        let list_header = list_header.clone();
        folder_selection.connect_selected_item_notify(move |sel| {
            let Some(row) = sel.selected_item().and_downcast::<gtk::TreeListRow>() else { return };
            let Some(boxed) = row.item().and_downcast::<glib::BoxedAnyObject>() else { return };
            let tree_item = boxed.borrow::<TreeItem>();
            match &*tree_item {
                TreeItem::Unified => {
                    drop(tree_item);
                    enter_unified_inbox(&state, &message_store, &message_selection);
                    refresh_list_header(&state, &list_header);
                }
                TreeItem::Folder(node) | TreeItem::Favorite(node) => {
                    let mailbox_id = node.mailbox.id.clone();
                    let account_id = node.mailbox.account_id.clone();
                    drop(tree_item);
                    select_mailbox(&state, account_id, mailbox_id);
                    refresh_list_header(&state, &list_header);
                }
                TreeItem::Account(_) | TreeItem::Favorites => {}
            }
        });
    }

    // --- Message selection -> AccountCommand::FetchBody on the current account ---
    {
        let state = state.clone();
        let reading_stack = reading_stack.clone();
        let message_header = message_header.clone();
        message_selection.connect_selected_item_notify(move |sel| {
            let Some(boxed) = sel.selected_item().and_downcast::<glib::BoxedAnyObject>() else {
                let mut st = state.borrow_mut();
                st.pending_body_request = None;
                st.pending_html_reveal = false;
                st.pending_header = None;
                st.rendered_message = None;
                drop(st);
                reading_stack.set_visible_child_name("empty");
                return;
            };
            let summary = boxed.borrow::<EmailSummary>();
            let uid = summary.uid;
            let mailbox = summary.mailbox.clone();
            // Re-selecting the message that's already on the reading pane -
            // which `GtkSingleSelection`'s autoselect does on every list
            // rebuild that keeps the same row first - must be a no-op, not a
            // fresh fetch/render. Routing it through "empty" and crossfading
            // the same email back in is exactly the startup flicker bug; the
            // body is already on screen, so there's nothing to re-render.
            let already_shown = {
                let st = state.borrow();
                st.rendered_message.as_ref() == Some(&(mailbox.clone(), uid)) && reading_stack.visible_child_name().as_deref() == Some("message")
            };
            if already_shown {
                return;
            }
            let request = (mailbox.clone(), uid);
            // Deferred to the reveal: updating the header here would swap it
            // to the next email while the previous message is still fading
            // out. `render_body` applies it once the new body is about to be
            // shown (the pane is on "empty" by then, so it can't flash).
            let pending_header = summary.clone();
            drop(summary);
            state.borrow_mut().pending_header = Some(pending_header);

            let body_is_cached = {
                let st = state.borrow();
                st.current_body
                    .as_ref()
                    .is_some_and(|(body_mailbox, body_uid, _)| body_mailbox == &mailbox && body_uid == &uid)
            };
            let should_request = {
                let mut st = state.borrow_mut();
                // Disarm any body load still in flight for the previously
                // selected message, so its `Finished` can't reveal a stale
                // email once the user has moved on.
                st.pending_html_reveal = false;
                let should_request = !body_is_cached && st.pending_body_request.as_ref() != Some(&request);
                if should_request || st.pending_body_request.as_ref() != Some(&request) {
                    st.pending_body_request = Some(request.clone());
                }
                should_request
            };
            if should_request {
                let st = state.borrow();
                if let Some(account_id) = mailbox_account_id(&mailbox) {
                    if let Some(handle) = st.accounts.get(&account_id) {
                        let _ = handle.cmd_tx.send_blocking(AccountCommand::FetchBody { mailbox: mailbox.clone(), uid });
                    }
                }
            }
            // Also silently abandons an in-progress composer in the reading
            // pane, if one's open - no "discard draft?" prompt, consistent
            // with this app's existing no-confirmation-dialog convention.
            reading_stack.set_visible_child_name("empty");
            if body_is_cached {
                let body = state.borrow().current_body.as_ref().map(|(_, _, body)| body.clone());
                if let Some(body) = body {
                    render_body(&state, &reading_stack, &message_header, mailbox, uid, body);
                }
            }
        });
    }

    // --- Delete/Archive/Report -> AccountCommand::MoveMessage against the
    // account's Trash/Archive/Junk mailbox; Snooze -> AccountCommand::
    // SnoozeMessage with a single fixed "tomorrow 9:00 AM local time"
    // default. All four are silent no-ops with nothing selected.
    for (button, role) in [
        (&delete_button, MailboxRole::Trash),
        (&archive_button, MailboxRole::Archive),
        (&report_button, MailboxRole::Junk),
    ] {
        let message_selection = message_selection.clone();
        let state = state.clone();
        button.connect_clicked(move |_| {
            if let Some((mailbox, uid, cmd_tx)) = selected_message_command_target(&message_selection, &state) {
                let _ = cmd_tx.send_blocking(AccountCommand::MoveMessage { mailbox, uid, role });
            }
        });
    }
    {
        let message_selection = message_selection.clone();
        let state = state.clone();
        snooze_button.connect_clicked(move |_| {
            if let Some((mailbox, uid, cmd_tx)) = selected_message_command_target(&message_selection, &state) {
                let tomorrow_9am = chrono::Local::now()
                    .date_naive()
                    .succ_opt()
                    .and_then(|d| d.and_hms_opt(9, 0, 0))
                    .and_then(|dt| dt.and_local_timezone(chrono::Local).single())
                    .map(|dt| dt.with_timezone(&chrono::Utc))
                    .unwrap_or_else(chrono::Utc::now);
                let _ = cmd_tx.send_blocking(AccountCommand::SnoozeMessage {
                    mailbox,
                    uid,
                    until: tomorrow_9am,
                });
            }
        });
    }

    // --- Main calendar navigation: prev/next/Today move the anchor by the
    // active view's natural unit (day/week/month - see `calendar_view::step`)
    // and redraw every view immediately, then ask every connected calendar
    // account to resync the newly-displayed month.
    {
        let calendar_state = calendar_state.clone();
        let calendar_main = calendar_main.clone();
        let mini_calendar = calendar_sidebar.mini_calendar.clone();
        let prev_button = calendar_main.prev_button.clone();
        prev_button.connect_clicked(move |_| {
            calendar_view::step(&calendar_main, -1);
            show_anchor(&calendar_state, &calendar_main, &mini_calendar);
        });
    }
    {
        let calendar_state = calendar_state.clone();
        let calendar_main = calendar_main.clone();
        let mini_calendar = calendar_sidebar.mini_calendar.clone();
        let next_button = calendar_main.next_button.clone();
        next_button.connect_clicked(move |_| {
            calendar_view::step(&calendar_main, 1);
            show_anchor(&calendar_state, &calendar_main, &mini_calendar);
        });
    }
    {
        let calendar_state = calendar_state.clone();
        let calendar_main = calendar_main.clone();
        let mini_calendar = calendar_sidebar.mini_calendar.clone();
        let today_button = calendar_main.today_button.clone();
        today_button.connect_clicked(move |_| {
            calendar_view::go_today(&calendar_main);
            show_anchor(&calendar_state, &calendar_main, &mini_calendar);
        });
    }
    // --- Sidebar mini-calendar -> re-anchor the main panel to the clicked
    // date (whatever view is active) and resync that date's month.
    {
        let calendar_state = calendar_state.clone();
        let calendar_main = calendar_main.clone();
        let mini_calendar = calendar_sidebar.mini_calendar.clone();
        calendar_view::connect_day_selected(&calendar_sidebar.mini_calendar, move |date| {
            calendar_view::set_anchor(&calendar_main, date);
            show_anchor(&calendar_state, &calendar_main, &mini_calendar);
        });
    }
    // --- Mail-screen overview mini-calendar -> re-show that day's events
    // (from whatever's already cached) and ask every connected calendar
    // account to resync that month in the background, without touching the
    // main Calendar view's own `displayed_month`.
    {
        let calendar_state = calendar_state.clone();
        let mail_overview_day = mail_overview_day.clone();
        let mail_overview_day_list = mail_overview_day_list.clone();
        calendar_view::connect_day_selected(&mail_calendar_overview, move |date| {
            mail_overview_day.set(date);
            refresh_mail_overview_day_list(&calendar_state, date, &mail_overview_day_list);
            let month = first_of_month(date);
            for handle in calendar_state.borrow().accounts.values() {
                let _ = handle.cmd_tx.send_blocking(CalendarCommand::SyncMonth(month));
            }
        });
    }

    spawn_account_discovery(
        worker.clone(),
        state,
        root_stack.clone(),
        toast_overlay.clone(),
        folder_selection,
        message_selection,
        message_store,
        message_header,
        reading_stack,
        current_mail_page,
        mail_view_button,
        list_header,
        refresh_config.clone(),
    );
    spawn_calendar_discovery(
        worker,
        calendar_state,
        root_stack,
        toast_overlay,
        calendar_main,
        calendar_sidebar.calendar_list_box,
        calendar_sidebar.mini_calendar,
        mail_overview_day,
        mail_overview_day_list,
        current_calendar_page,
        calendar_view_button,
        refresh_config,
    );

    window
}

/// Re-anchors the main calendar panel's already-redrawn views, keeps the
/// sidebar's mini-calendar showing the anchor's month (with event-day
/// markers), records the displayed month, and asks every connected calendar
/// account to resync it.
fn show_anchor(calendar_state: &Rc<RefCell<CalendarUiState>>, calendar_main: &CalendarMain, mini_calendar: &calendar_view::MiniCalendar) {
    let day = calendar_view::anchor(calendar_main);
    let month = first_of_month(day);
    let event_days = calendar_event_days(calendar_state, month);
    calendar_view::set_mini_month(mini_calendar, day, &event_days);
    let mut st = calendar_state.borrow_mut();
    st.displayed_month = month;
    for handle in st.accounts.values() {
        let _ = handle.cmd_tx.send_blocking(CalendarCommand::SyncMonth(month));
    }
}

/// The local dates within `month` that have at least one occurrence from a
/// currently-checked calendar, unioned across every account that has synced
/// that month - drives the mini-calendar's bold event-day numerals.
fn calendar_event_days(calendar_state: &Rc<RefCell<CalendarUiState>>, month: chrono::NaiveDate) -> HashSet<chrono::NaiveDate> {
    let st = calendar_state.borrow();
    let mut days = HashSet::new();
    for handle in st.accounts.values() {
        if handle.last_synced_month != Some(month) {
            continue;
        }
        for occ in &handle.last_occurrences {
            if st.checked_calendar_ids.contains(&occ.calendar_id) {
                days.insert(occ.start.with_timezone(&chrono::Local).date_naive());
            }
        }
    }
    days
}

fn current_month_start() -> chrono::NaiveDate {
    first_of_month(chrono::Utc::now().date_naive())
}

fn first_of_month(date: chrono::NaiveDate) -> chrono::NaiveDate {
    use chrono::Datelike;
    date.with_day(1).unwrap_or(date)
}

/// Maps a (nav-rail module, ribbon tab) pair to the `view_toolbar_stack`
/// child to show. Mail is tabbed - Home shows the command toolbar, View the
/// layout toggles; Calendar/Config each have a single non-tabbed toolbar of
/// their own, so they ignore the tab. Unknown combos fall back to Mail-Home.
fn ribbon_stack_name(module: &str, tab: &str) -> &'static str {
    match (module, tab) {
        ("mail", "view") => "mail-view",
        ("mail", _) => "mail-home",
        ("calendar", _) => "calendar",
        ("config", _) => "config",
        _ => "mail-home",
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_account_discovery(
    worker: Rc<Worker>,
    state: Rc<RefCell<UiState>>,
    root_stack: gtk::Stack,
    toast_overlay: adw::ToastOverlay,
    folder_selection: gtk::SingleSelection,
    message_selection: gtk::SingleSelection,
    message_store: gio::ListStore,
    message_header: crate::message_header::MessageHeader,
    reading_stack: gtk::Stack,
    current_mail_page: Rc<Cell<&'static str>>,
    mail_view_button: gtk::ToggleButton,
    list_header: ListHeader,
    refresh_config: Rc<dyn Fn()>,
) {
    let (goa_tx, goa_rx) = async_channel::bounded(1);
    worker.spawn(async move {
        let result = async {
            let client = GoaClient::connect().await?;
            let accounts = client.list_mail_accounts().await?;
            Ok::<_, lookout_goa::Error>((client, accounts))
        }
        .await;
        let _ = goa_tx.send(result).await;
    });

    glib::spawn_future_local(async move {
        let Ok(result) = goa_rx.recv().await else { return };
        let show_page = |name: &'static str| {
            current_mail_page.set(name);
            if mail_view_button.is_active() {
                root_stack.set_visible_child_name(name);
            }
        };
        match result {
            Ok((client, accounts)) if !accounts.is_empty() => {
                show_page("mail");
                // One AccountSession actor per connected account, all
                // running concurrently on the worker thread. `GoaClient` is
                // a cheap Arc-backed handle (see its doc comment), so
                // cloning it per account reuses the one D-Bus connection
                // rather than opening a redundant one each time.
                for account in accounts {
                    connect_account(
                        worker.clone(),
                        state.clone(),
                        folder_selection.clone(),
                        message_selection.clone(),
                        message_store.clone(),
                        message_header.clone(),
                        reading_stack.clone(),
                        toast_overlay.clone(),
                        client.clone(),
                        list_header.clone(),
                        account,
                    );
                }
                refresh_config();
            }
            Ok(_) => {
                show_page("empty");
            }
            Err(e) => {
                show_page("empty");
                toast_overlay.add_toast(adw::Toast::new(&format!("Couldn't reach GNOME Online Accounts: {e}")));
            }
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn connect_account(
    worker: Rc<Worker>,
    state: Rc<RefCell<UiState>>,
    folder_selection: gtk::SingleSelection,
    message_selection: gtk::SingleSelection,
    message_store: gio::ListStore,
    message_header: crate::message_header::MessageHeader,
    reading_stack: gtk::Stack,
    toast_overlay: adw::ToastOverlay,
    goa_client: GoaClient,
    list_header: ListHeader,
    account: lookout_goa::GoaMailAccount,
) {
    let account_id = AccountId(account.account_id.0.clone());
    let display_name = account.display_name.clone();
    let config = AccountConfig {
        account_id: account_id.clone(),
        display_name: account.display_name.clone(),
        email: account.email.clone(),
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
    let credentials: Rc<dyn lookout_mail::session::CredentialProvider> = Rc::new(GoaCredentialProvider::new(goa_client, account));
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

    let (cmd_tx, cmd_rx) = async_channel::unbounded();
    let (evt_tx, evt_rx) = async_channel::unbounded();
    state.borrow_mut().accounts.insert(
        account_id.clone(),
        AccountHandle {
            cmd_tx,
            email: config.email.clone(),
            display_name,
            imap_host: config.imap.host.clone(),
            imap_port: config.imap.port,
            smtp_host: config.smtp.host.clone(),
            smtp_port: config.smtp.port,
            folders: Vec::new(),
        },
    );

    worker.spawn(lookout_mail::session::run_account_session(config, credentials, cmd_rx, evt_tx));

    glib::spawn_future_local(async move {
        while let Ok(event) = evt_rx.recv().await {
            match event {
                AccountEvent::ConnectionStateChanged(ConnectionState::Error { message, .. }) => {
                    toast_overlay.add_toast(adw::Toast::new(&format!("{}: {message}", account_label(&state, &account_id))));
                }
                AccountEvent::ConnectionStateChanged(_) => {}
                AccountEvent::FoldersUpdated(folders) => {
                    {
                        let mut st = state.borrow_mut();
                        if let Some(handle) = st.accounts.get_mut(&account_id) {
                            handle.folders = folders;
                        }
                        // Fresh folder list means the account just (re)connected;
                        // any sync requests it left unanswered are dead - clear
                        // them so a later request for the same mailbox isn't
                        // wrongly suppressed by an entry that'll never resolve.
                        st.syncing.retain(|mailbox| mailbox_account_id(mailbox).as_ref() != Some(&account_id));
                    }
                    rebuild_folder_tree(&state, &folder_selection);
                    // Folder names and account labels only exist once this
                    // event lands, so a view restored before it (or adopted by
                    // the race below) gets its header filled in here.
                    refresh_list_header(&state, &list_header);
                }
                AccountEvent::MessagesUpdated { mailbox, messages } => {
                    // The sync this mailbox was asked for (if any) has landed.
                    state.borrow_mut().syncing.remove(&mailbox);
                    // Decide whether this mailbox belongs to the view on
                    // screen, folding its payload into the unified snapshot
                    // when in "All Inboxes" mode. On fresh startup (nothing
                    // selected yet) the first inbox sync is still adopted as
                    // the default single-mailbox view, matching the old
                    // race-first behavior.
                    let (display, single_messages, adopted) = {
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
                            if in_unified_set {
                                st.unified_snapshots.insert(mailbox, messages);
                            }
                            (in_unified_set, None, false)
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
                            (is_current, is_current.then_some(messages), adopted)
                        }
                    };
                    // The adopt-first path picked a default view; name it in
                    // the list header.
                    if adopted {
                        refresh_list_header(&state, &list_header);
                    }
                    if display {
                        let all = match single_messages {
                            Some(messages) => messages,
                            None => merge_unified_snapshots(&state.borrow().unified_snapshots),
                        };
                        let (key, descending) = current_sort(&state);
                        repopulate_message_list(&message_store, &message_selection, all, key, descending);
                    }
                }
                AccountEvent::BodyFetched { mailbox, uid, body } => {
                    let should_render = {
                        let mut st = state.borrow_mut();
                        let is_current = body_request_matches(&mailbox, &uid, st.pending_body_request.as_ref());
                        if is_current {
                            st.current_body = Some((mailbox.clone(), uid, body.clone()));
                        }
                        is_current
                    };
                    if should_render {
                        render_body(&state, &reading_stack, &message_header, mailbox, uid, body);
                    }
                }
                AccountEvent::SendCompleted => {
                    toast_overlay.add_toast(adw::Toast::new("Message sent"));
                }
                AccountEvent::MessageMoved { role } => {
                    let label = match role {
                        MailboxRole::Trash => "Deleted",
                        MailboxRole::Archive => "Archived",
                        MailboxRole::Junk => "Reported as junk",
                        _ => "Moved",
                    };
                    toast_overlay.add_toast(adw::Toast::new(label));
                }
                AccountEvent::MessageSnoozed => {
                    toast_overlay.add_toast(adw::Toast::new("Snoozed until tomorrow 9:00 AM"));
                }
                AccountEvent::Error(message) => {
                    toast_overlay.add_toast(adw::Toast::new(&format!("{}: {message}", account_label(&state, &account_id))));
                }
            }
        }
    });
}

/// Mirrors `spawn_account_discovery` 1:1 for Calendar - a fully independent
/// GOA account set, discovered and connected the same worker-spawn +
/// `glib::spawn_future_local` way.
#[allow(clippy::too_many_arguments)]
fn spawn_calendar_discovery(
    worker: Rc<Worker>,
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
    refresh_config: Rc<dyn Fn()>,
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
        match result {
            Ok((client, accounts)) if !accounts.is_empty() => {
                show_page("calendar");
                for account in accounts {
                    connect_calendar_account(
                        worker.clone(),
                        calendar_state.clone(),
                        calendar_main.clone(),
                        calendar_list_box.clone(),
                        mini_calendar.clone(),
                        mail_overview_day.clone(),
                        mail_overview_day_list.clone(),
                        toast_overlay.clone(),
                        client.clone(),
                        account,
                    );
                }
                refresh_config();
            }
            Ok(_) => {
                show_page("calendar-empty");
            }
            Err(e) => {
                show_page("calendar-empty");
                toast_overlay.add_toast(adw::Toast::new(&format!("Couldn't reach GNOME Online Accounts: {e}")));
            }
        }
    });
}

fn ensure_checked_calendars(checked: &mut HashSet<CalendarId>, calendars: &[CalendarInfo]) {
    for calendar in calendars {
        checked.insert(calendar.id.clone());
    }
}

/// Recomputes which calendars actually exist across every connected account,
/// defaults any newly-seen id to checked (shown), and re-renders the
/// sidebar's "My calendars" checklist against that - the checklist's own
/// `on_toggle` closure flips membership in `checked_calendar_ids` and calls
/// `refresh_displayed_calendar_view` to redraw the grid accordingly.
fn refresh_calendar_checklist(calendar_state: &Rc<RefCell<CalendarUiState>>, calendar_list_box: &gtk::Box, calendar_main: &Rc<CalendarMain>) {
    let mut groups: Vec<calendar_view::CalendarAccountGroup> = calendar_state
        .borrow()
        .accounts
        .values()
        .map(|h| calendar_view::CalendarAccountGroup {
            display_name: h.display_name.clone(),
            calendars: h.calendars.clone(),
            status: calendar_view::calendar_account_status_text(&h.connection_state, !h.calendars.is_empty()),
        })
        .collect();
    groups.sort_by_key(|g| g.display_name.to_lowercase());
    let all_calendars: Vec<CalendarInfo> = groups.iter().flat_map(|g| g.calendars.iter().cloned()).collect();
    {
        let mut st = calendar_state.borrow_mut();
        ensure_checked_calendars(&mut st.checked_calendar_ids, &all_calendars);
        calendar_colors::assign_missing(&mut st.calendar_colors, &all_calendars);
    }
    let checked = calendar_state.borrow().checked_calendar_ids.clone();
    let colors = calendar_state.borrow().calendar_colors.clone();
    let on_toggle = {
        let calendar_state = calendar_state.clone();
        let calendar_main = calendar_main.clone();
        move |id: CalendarId, is_checked: bool| {
            {
                let mut st = calendar_state.borrow_mut();
                if is_checked {
                    st.checked_calendar_ids.insert(id);
                } else {
                    st.checked_calendar_ids.remove(&id);
                }
            }
            refresh_displayed_calendar_view(&calendar_state, &calendar_main);
        }
    };
    calendar_view::rebuild_calendar_checklist(calendar_list_box, &groups, &checked, &colors, &calendar_main.check_colors, on_toggle);
    calendar_view::set_calendar_colors(calendar_main, &colors);
    refresh_displayed_calendar_view(calendar_state, calendar_main);
}

/// Unions every connected calendar account's latest occurrences for
/// whatever month is currently displayed - filtered to only the calendars
/// currently checked in the sidebar - and redraws the whole main panel (every
/// view in the stack gets the merged set; each derives its own window from
/// the anchor). Same "only apply if it matches what's on screen" + "merge all
/// accounts' latest snapshot" approach as Mail's
/// `MessagesUpdated`/`rebuild_folder_tree`.
fn refresh_displayed_calendar_view(calendar_state: &Rc<RefCell<CalendarUiState>>, calendar_main: &CalendarMain) {
    let st = calendar_state.borrow();
    let month = st.displayed_month;
    let merged: Vec<EventOccurrence> = st
        .accounts
        .values()
        .filter(|h| h.last_synced_month == Some(month))
        .flat_map(|h| h.last_occurrences.iter().filter(|occ| st.checked_calendar_ids.contains(&occ.calendar_id)).cloned())
        .collect();
    drop(st);
    calendar_view::set_occurrences(calendar_main, &merged);
}

/// Fills the Mail-screen overview pane's event list with every checked
/// calendar's occurrences (from whatever's currently cached - no new fetch
/// here) whose local date matches `day`, sorted by start time. Shows a
/// "No events" placeholder when there are none. Unlike
/// `refresh_displayed_calendar_view`, this filters by exact day rather than
/// by the main Calendar view's own displayed month - the overview pane can
/// be showing a day from a different month entirely.
fn refresh_mail_overview_day_list(calendar_state: &Rc<RefCell<CalendarUiState>>, day: chrono::NaiveDate, day_list_box: &gtk::Box) {
    while let Some(child) = day_list_box.first_child() {
        day_list_box.remove(&child);
    }

    let st = calendar_state.borrow();
    let mut occurrences: Vec<&EventOccurrence> = st
        .accounts
        .values()
        .flat_map(|h| h.last_occurrences.iter())
        .filter(|occ| st.checked_calendar_ids.contains(&occ.calendar_id))
        .filter(|occ| occ.start.with_timezone(&chrono::Local).date_naive() == day)
        .collect();
    occurrences.sort_by_key(|occ| occ.start);

    if occurrences.is_empty() {
        let placeholder = gtk::Label::builder().label("No events").css_classes(["dim-label", "caption"]).xalign(0.0).build();
        day_list_box.append(&placeholder);
    } else {
        for occ in occurrences {
            let text = if occ.all_day {
                occ.summary.clone().unwrap_or_else(|| "(untitled)".to_string())
            } else {
                format!(
                    "{} {}",
                    occ.start.with_timezone(&chrono::Local).format("%H:%M"),
                    occ.summary.as_deref().unwrap_or("(untitled)")
                )
            };
            let label = gtk::Label::builder()
                .label(&text)
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .css_classes(["caption"])
                .build();
            day_list_box.append(&label);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn connect_calendar_account(
    worker: Rc<Worker>,
    calendar_state: Rc<RefCell<CalendarUiState>>,
    calendar_main: Rc<CalendarMain>,
    calendar_list_box: gtk::Box,
    mini_calendar: calendar_view::MiniCalendar,
    mail_overview_day: Rc<Cell<chrono::NaiveDate>>,
    mail_overview_day_list: gtk::Box,
    toast_overlay: adw::ToastOverlay,
    goa_client: GoaClient,
    account: GoaCalendarAccount,
) {
    let account_id = account.account_id.clone();
    let display_name = account.display_name.clone();
    let config = CalendarAccountConfig {
        account_id: account_id.clone(),
        display_name: display_name.clone(),
        base_url: account.uri.clone(),
        accept_ssl_errors: account.accept_ssl_errors,
        username: account.display_name.clone(),
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

    let (cmd_tx, cmd_rx) = async_channel::unbounded();
    let (evt_tx, evt_rx) = async_channel::unbounded();
    calendar_state.borrow_mut().accounts.insert(
        account_id.clone(),
        CalendarAccountHandle {
            cmd_tx,
            display_name,
            uri: config.base_url.clone(),
            calendars: Vec::new(),
            connection_state: CalConnectionState::Connecting,
            last_occurrences: Vec::new(),
            last_synced_month: None,
        },
    );

    worker.spawn(lookout_dav::session::run_calendar_session(config, credentials, cmd_rx, evt_tx));

    glib::spawn_future_local(async move {
        while let Ok(event) = evt_rx.recv().await {
            match event {
                CalendarSessionEvent::ConnectionStateChanged(state) => {
                    if let Some(handle) = calendar_state.borrow_mut().accounts.get_mut(&account_id) {
                        handle.connection_state = state.clone();
                    }
                    if let CalConnectionState::Error { message, .. } = &state {
                        toast_overlay.add_toast(adw::Toast::new(&format!("{}: {message}", calendar_account_label(&calendar_state, &account_id))));
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
                    if let Some(handle) = calendar_state.borrow_mut().accounts.get_mut(&account_id) {
                        handle.last_occurrences = occurrences;
                        handle.last_synced_month = Some(month);
                    }
                    refresh_displayed_calendar_view(&calendar_state, &calendar_main);
                    refresh_mail_overview_day_list(&calendar_state, mail_overview_day.get(), &mail_overview_day_list);
                    // The sidebar mini-calendar's bold event-day numerals track
                    // the currently-displayed month - refresh them when a fetch
                    // for that month lands (navigating to an uncached month
                    // marks its days bold the moment the sync completes).
                    if calendar_view::mini_month(&mini_calendar) == month {
                        let event_days = calendar_event_days(&calendar_state, month);
                        calendar_view::set_mini_event_days(&mini_calendar, &event_days);
                    }
                }
                CalendarSessionEvent::Error(message) => {
                    toast_overlay.add_toast(adw::Toast::new(&format!("{}: {message}", calendar_account_label(&calendar_state, &account_id))));
                }
            }
        }
    });
}

fn calendar_account_label(state: &Rc<RefCell<CalendarUiState>>, account_id: &AccountId) -> String {
    state
        .borrow()
        .accounts
        .get(account_id)
        .map(|h| h.display_name.clone())
        .unwrap_or_else(|| account_id.0.clone())
}

fn mailbox_account_id(mailbox: &MailboxId) -> Option<AccountId> {
    mailbox.0.split_once(':').map(|(account_id, _)| AccountId(account_id.to_string()))
}

/// The message-list header's mutable parts: the two title lines and the
/// favorite star. Bundled so the handful of places that change the active view
/// can refresh the header without each threading four widgets through its own
/// signature.
#[derive(Clone)]
struct ListHeader {
    folder_label: gtk::Label,
    account_label: gtk::Label,
    favorite_button: gtk::ToggleButton,
    /// Set while the star's state is being written programmatically, so the
    /// `toggled` handler can tell a refresh apart from a real user click.
    favorite_suppress: Rc<Cell<bool>>,
}

/// The header's two title lines for the active view: the open folder's display
/// name over its owning account. The unified view has no single mailbox behind
/// it, so it names itself and the whole account set.
fn current_view_title(state: &Rc<RefCell<UiState>>) -> (String, String) {
    let st = state.borrow();
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
        .map(|m| m.name.clone())
        .unwrap_or_else(|| mailbox_id.0.split_once(':').map(|(_, path)| path.to_string()).unwrap_or_else(|| mailbox_id.0.clone()));
    let account = handle
        .map(|h| if h.display_name.is_empty() { h.email.clone() } else { h.display_name.clone() })
        .unwrap_or_default();
    (folder, account)
}

/// Renders the header for whatever the message list is currently showing:
/// title lines, plus the star's pressed state and whether it's actionable at
/// all (the unified view spans every account, so there's no single folder to
/// favorite).
fn refresh_list_header(state: &Rc<RefCell<UiState>>, header: &ListHeader) {
    let (folder, account) = current_view_title(state);
    header.folder_label.set_label(&folder);
    header.account_label.set_label(&account);
    header.account_label.set_visible(!account.is_empty());

    let (favorable, starred) = {
        let st = state.borrow();
        match st.current_mailbox.as_ref() {
            Some(mailbox) => (true, st.favorites.contains(mailbox)),
            None => (false, false),
        }
    };
    header.favorite_suppress.set(true);
    header.favorite_button.set_sensitive(favorable);
    header.favorite_button.set_active(starred);
    apply_favorite_visual(&header.favorite_button, starred);
    header.favorite_suppress.set(false);
}

/// Keeps the favorite star's icon and tooltip in step with its pressed state.
fn apply_favorite_visual(button: &gtk::ToggleButton, starred: bool) {
    let icon = if starred {
        themed_icon_name(&["starred-symbolic", "mail-mark-important-symbolic"])
    } else {
        themed_icon_name(&["non-starred-symbolic", "starred-symbolic", "mail-mark-important-symbolic"])
    };
    button.set_icon_name(icon);
    button.set_tooltip_text(Some(if starred { "Remove from Favorites" } else { "Add to Favorites" }));
}

/// The first of `candidates` the current icon theme actually has. Icon names
/// in this app resolve against the machine's theme rather than a bundled set
/// (see `folder_icon_name`), and the header's sort/filter/star icons are names
/// this codebase hasn't used before - so fall back to one that's already
/// proven in-tree instead of rendering a "missing image" box. The last
/// candidate is used unconditionally if none match.
fn themed_icon_name(candidates: &[&'static str]) -> &'static str {
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
fn sort_direction_icon(descending: bool) -> &'static str {
    if descending {
        themed_icon_name(&["view-sort-descending-symbolic", "go-down-symbolic"])
    } else {
        themed_icon_name(&["view-sort-ascending-symbolic", "go-up-symbolic"])
    }
}

/// Switches the message list to a single mailbox and asks its owning account
/// to sync it. Shared by the folder-selection handler and the account
/// switcher's fallback path.
fn select_mailbox(state: &Rc<RefCell<UiState>>, account_id: AccountId, mailbox_id: MailboxId) {
    {
        let mut st = state.borrow_mut();
        st.mail_view = MailView::Single;
        st.current_account = Some(account_id.clone());
        st.current_mailbox = Some(mailbox_id.clone());
        st.restore_pending = false;
        last_view::save(&LastSelection {
            unified: false,
            mailbox: Some(mailbox_id.0.clone()),
        });
    }
    request_mailbox_sync(state, &account_id, &mailbox_id);
}

/// Requests a mailbox sync from its account, deduplicating against
/// in-flight requests: a `SyncMailbox` that's already outstanding (sent but
/// not yet answered by a `MessagesUpdated`) suppresses a duplicate. Without
/// this the startup burst - the session's cache replay plus the app's
/// on-demand syncs for the same inbox - queues several identical syncs.
/// Returns whether a request was actually sent. Pending entries are cleared
/// by the `MessagesUpdated` that answers them and by a `FoldersUpdated`
/// (every reconnect re-lists folders), so a request that dies with a dropped
/// connection can never permanently suppress a later one.
fn request_mailbox_sync(state: &Rc<RefCell<UiState>>, account_id: &AccountId, mailbox_id: &MailboxId) -> bool {
    let mut st = state.borrow_mut();
    if st.syncing.contains(mailbox_id) {
        return false;
    }
    st.syncing.insert(mailbox_id.clone());
    if let Some(handle) = st.accounts.get(account_id) {
        let _ = handle.cmd_tx.send_blocking(AccountCommand::SyncMailbox(mailbox_id.clone()));
    }
    true
}

/// Every connected account's Inbox - the mailbox set the unified view is
/// built from.
fn account_inboxes(state: &Rc<RefCell<UiState>>) -> Vec<(AccountId, MailboxId)> {
    state
        .borrow()
        .accounts
        .iter()
        .filter_map(|(id, handle)| handle.folders.iter().find(|m| matches!(m.role, MailboxRole::Inbox)).map(|m| (id.clone(), m.id.clone())))
        .collect()
}

/// Re-syncs whatever the message list is currently showing: the open mailbox,
/// or every account's Inbox in the unified view. `request_mailbox_sync`
/// dedupes against in-flight requests, so pressing Sync while a sync is
/// already outstanding is deliberately a no-op rather than a second round
/// trip.
fn resync_current_view(state: &Rc<RefCell<UiState>>) {
    if matches!(state.borrow().mail_view, MailView::UnifiedInbox) {
        for (account_id, inbox_id) in account_inboxes(state) {
            request_mailbox_sync(state, &account_id, &inbox_id);
        }
        return;
    }
    let current = {
        let st = state.borrow();
        st.current_account.clone().zip(st.current_mailbox.clone())
    };
    if let Some((account_id, mailbox_id)) = current {
        request_mailbox_sync(state, &account_id, &mailbox_id);
    }
}

/// Enters the "All Inboxes" view: asks every connected account that has an
/// Inbox to sync it, and immediately repopulates the list from whatever the
/// per-mailbox snapshots already hold.
fn enter_unified_inbox(state: &Rc<RefCell<UiState>>, message_store: &gio::ListStore, message_selection: &gtk::SingleSelection) {
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
        last_view::save(&LastSelection { unified: true, mailbox: None });
    }
    for (account_id, inbox_id) in inboxes {
        request_mailbox_sync(state, &account_id, &inbox_id);
    }
    let all = merge_unified_snapshots(&state.borrow().unified_snapshots);
    let (key, descending) = current_sort(state);
    repopulate_message_list(message_store, message_selection, all, key, descending);
}

/// Merges the unified view's per-mailbox snapshots into a single newest-first
/// list, deduplicating by `(mailbox, uid)` - a message lives in exactly one
/// mailbox, but a mailbox can be re-synced more than once, so the same
/// `(mailbox, uid)` must never appear twice.
fn merge_unified_snapshots(snapshots: &HashMap<MailboxId, Vec<EmailSummary>>) -> Vec<EmailSummary> {
    let mut seen: HashSet<(MailboxId, Uid)> = HashSet::new();
    let mut merged: Vec<EmailSummary> = Vec::new();
    for messages in snapshots.values() {
        for m in messages {
            if seen.insert((m.mailbox.clone(), m.uid)) {
                merged.push(m.clone());
            }
        }
    }
    merged.sort_by_key(|m| std::cmp::Reverse(m.date));
    merged
}

/// The active sort, snapshotted so callers don't hold a `state` borrow across
/// a `repopulate_message_list` (whose splice re-enters the selection handler).
fn current_sort(state: &Rc<RefCell<UiState>>) -> (SortKey, bool) {
    let st = state.borrow();
    (st.sort_key, st.sort_descending)
}

/// Orders the message list by the header's chosen key. Every key tie-breaks on
/// date, newest first, so the order is total - two messages with the same
/// sender or subject can't shuffle between otherwise-identical rebuilds and
/// defeat `store_matches`.
fn sort_messages(messages: &mut [EmailSummary], key: SortKey, descending: bool) {
    fn sender_of(m: &EmailSummary) -> String {
        m.from.first().map(|a| a.display_label().to_lowercase()).unwrap_or_default()
    }
    match key {
        SortKey::Date => messages.sort_by(|a, b| b.date.cmp(&a.date)),
        SortKey::Sender => messages.sort_by(|a, b| sender_of(a).cmp(&sender_of(b)).then_with(|| b.date.cmp(&a.date))),
        SortKey::Subject => {
            let subject_of = |m: &EmailSummary| m.subject.clone().unwrap_or_default().to_lowercase();
            messages.sort_by(|a, b| subject_of(a).cmp(&subject_of(b)).then_with(|| b.date.cmp(&a.date)));
        }
    }
    // Ascending is the descending order reversed rather than a second set of
    // comparators - which also flips the date tie-break, so "oldest first"
    // really is the exact mirror of what was on screen.
    if !descending {
        messages.reverse();
    }
}

/// Every summary currently in the message store, in display order. Lets a sort
/// change re-order the visible list without re-fetching: single-mailbox views
/// keep no snapshot in `UiState` (only the unified view does), so the store
/// itself is the only copy of what's on screen.
fn snapshot_message_store(message_store: &gio::ListStore) -> Vec<EmailSummary> {
    (0..message_store.n_items())
        .filter_map(|i| message_store.item(i).and_downcast::<glib::BoxedAnyObject>())
        .map(|boxed| boxed.borrow::<EmailSummary>().clone())
        .collect()
}

/// Re-orders the visible list under the current sort. The rebuild goes through
/// `repopulate_message_list`, so the selected message stays selected and the
/// reading pane doesn't re-render (and re-crossfade) the email it's already
/// showing.
fn resort_message_list(state: &Rc<RefCell<UiState>>, message_store: &gio::ListStore, message_selection: &gtk::SingleSelection) {
    let messages = snapshot_message_store(message_store);
    if messages.is_empty() {
        return;
    }
    let (key, descending) = current_sort(state);
    repopulate_message_list(message_store, message_selection, messages, key, descending);
}

/// Replaces the message store's contents with `messages` in the list's current
/// sort order, preserving the current highlight where possible - the
/// selection-restore dance the old inline `MessagesUpdated` rebuild did,
/// shared by the single-mailbox and unified paths.
///
/// A no-op when the incoming list is identical to what's already displayed:
/// the startup burst - each account's cache replay plus its live sync plus
/// the app's on-demand syncs - delivers the same envelope set several times
/// in a row, and rebuilding for each one would re-select the first row
/// (`GtkSingleSelection` autoselects), refiring the selection handler and
/// crossfading the same email every time.
///
/// The store is rebuilt with a single `splice` rather than `remove_all` +
/// `append`: the two-step version transiently empties the model, which
/// flips `GtkSingleSelection` to "nothing selected" and fires the selection
/// handler's empty branch - yanking the reading pane to "empty" and clearing
/// `rendered_message` so the re-selection of the same email no longer
/// matches the `already_shown` guard and re-renders (crossfading) it. A
/// splice is one `items-changed` signal in which the selection position
/// survives, so the guard keeps working across rebuilds.
fn repopulate_message_list(
    message_store: &gio::ListStore,
    message_selection: &gtk::SingleSelection,
    mut messages: Vec<EmailSummary>,
    sort_key: SortKey,
    sort_descending: bool,
) {
    // Sorting here, before the identity check, makes this the list's canonical
    // order no matter which event produced the rebuild - so the check compares
    // like with like, and a sort change is itself detected as a real change.
    sort_messages(&mut messages, sort_key, sort_descending);
    if store_matches(message_store, &messages) {
        return;
    }

    // Remember the current highlight before the rebuild wipes the store: the
    // splice drops the old row objects, which would otherwise lose the user's
    // selection every time a delete/archive/snooze lands a `MessagesUpdated`.
    let previous_selection = message_selection.selected_item().and_downcast::<glib::BoxedAnyObject>().map(|b| {
        let summary = b.borrow::<EmailSummary>();
        (summary.mailbox.clone(), summary.uid)
    });
    let previous_index = message_selection.selected();

    let additions: Vec<glib::Object> = messages.into_iter().map(|m| glib::BoxedAnyObject::new(m).upcast()).collect();
    message_store.splice(0, message_store.n_items(), &additions);

    // Restore the highlight: the same message if it's still present (the
    // rebuild wasn't a delete of the selected row), otherwise the message
    // that now occupies its old spot.
    let restored = previous_selection.map(|(mailbox, uid)| {
        let mut restored = None;
        for i in 0..message_store.n_items() {
            let Some(boxed) = message_store.item(i).and_downcast::<glib::BoxedAnyObject>() else {
                continue;
            };
            let summary = boxed.borrow::<EmailSummary>();
            if summary.mailbox == mailbox && summary.uid == uid {
                restored = Some(i);
                break;
            }
        }
        restored
    });
    if let Some(index) = restored.flatten() {
        message_selection.set_selected(index);
    } else if previous_index != u32::MAX && message_store.n_items() > 0 {
        message_selection.set_selected(previous_index.min(message_store.n_items() - 1));
    }
}

/// A compact fingerprint of the fields one message-list row displays
/// (sender, unread emphasis, date, subject), keyed to keep the row distinct
/// from any other. Used by `store_matches` to detect "nothing changed".
fn message_row_key(m: &EmailSummary) -> (MailboxId, Uid, bool, chrono::DateTime<chrono::Utc>, Option<String>, String) {
    let from = m.from.first().map(|a| a.display_label().to_string()).unwrap_or_else(|| "(unknown)".into());
    (m.mailbox.clone(), m.uid, m.is_unread(), m.date, m.subject.clone(), from)
}

/// True when the message store already holds exactly `messages`, in the same
/// order and with the same displayed content - so `repopulate_message_list`
/// can skip the rebuild (and the selection churn it causes) as a no-op.
fn store_matches(message_store: &gio::ListStore, messages: &[EmailSummary]) -> bool {
    if message_store.n_items() as usize != messages.len() {
        return false;
    }
    messages.iter().enumerate().all(|(i, m)| {
        message_store
            .item(i as u32)
            .and_downcast::<glib::BoxedAnyObject>()
            .map(|b| message_row_key(&b.borrow::<EmailSummary>()) == message_row_key(m))
            .unwrap_or(false)
    })
}

/// Resolves the currently-selected message in `message_selection` to its
/// mailbox/uid and its owning account's command channel, for the
/// Delete/Archive/Report/Snooze button handlers - mirrors the lookup already
/// done inline by the `FetchBody`-on-selection handler above. The account is
/// derived from the message's own `MailboxId` rather than the view's
/// `current_account`, so the unified "All Inboxes" list routes each message
/// to the right account. Returns `None` if nothing is selected or its
/// account has since disconnected, in which case the calling handler is a
/// silent no-op.
fn selected_message_command_target(message_selection: &gtk::SingleSelection, state: &Rc<RefCell<UiState>>) -> Option<(MailboxId, Uid, async_channel::Sender<AccountCommand>)> {
    let boxed = message_selection.selected_item().and_downcast::<glib::BoxedAnyObject>()?;
    let summary = boxed.borrow::<EmailSummary>();
    let uid = summary.uid;
    let mailbox = summary.mailbox.clone();
    drop(summary);

    let account_id = mailbox_account_id(&mailbox)?;
    let st = state.borrow();
    let cmd_tx = st.accounts.get(&account_id)?.cmd_tx.clone();
    Some((mailbox, uid, cmd_tx))
}

/// Resolves the currently-selected message plus its already-fetched body,
/// for the Reply/Reply-All/Forward button handlers. Returns `None` if
/// nothing is selected, its account has disconnected, or the cached
/// `current_body` doesn't match the selection (its body hasn't arrived yet,
/// or is stale from a previous selection) - the calling handler is then a
/// silent no-op, same convention as `selected_message_command_target`. The
/// "From" identity is the message's owning account, so replies composed from
/// the unified view go out from the right address.
fn selected_message_reply_context(
    message_selection: &gtk::SingleSelection,
    state: &Rc<RefCell<UiState>>,
) -> Option<(EmailSummary, EmailBody, String, async_channel::Sender<AccountCommand>)> {
    let boxed = message_selection.selected_item().and_downcast::<glib::BoxedAnyObject>()?;
    let summary = boxed.borrow::<EmailSummary>().clone();

    let st = state.borrow();
    let (body_mailbox, body_uid, body) = st.current_body.as_ref()?;
    if *body_mailbox != summary.mailbox || *body_uid != summary.uid {
        return None;
    }
    let account_id = mailbox_account_id(&summary.mailbox)?;
    let handle = st.accounts.get(&account_id)?;
    Some((summary, body.clone(), handle.email.clone(), handle.cmd_tx.clone()))
}

fn account_label(state: &Rc<RefCell<UiState>>, account_id: &AccountId) -> String {
    state
        .borrow()
        .accounts
        .get(account_id)
        .map(|h| h.display_name.clone())
        .unwrap_or_else(|| account_id.0.clone())
}

/// Rebuilds the folder sidebar's `Gtk.TreeListModel` from every connected
/// account's latest folder snapshot. Accounts are sorted by email for a
/// stable order across rebuilds (`HashMap` iteration order isn't stable,
/// and accounts visibly reshuffling on every resync would be jarring).
/// On startup - before any folder has been selected or message adopted -
/// the pane opens on the user's remembered view (see `last_view`), or the
/// "All Inboxes" unified row by default (see
/// `restore_or_default_initial_view`).
fn rebuild_folder_tree(state: &Rc<RefCell<UiState>>, folder_selection: &gtk::SingleSelection) {
    // Borrow only long enough to snapshot the account data. `set_selected`
    // inside `select_first_inbox` synchronously fires the `selected-item`
    // handler, which itself borrows `state` mutably - so no borrow may be
    // live across that call.
    let (auto_select_inbox, mut accounts, mut favorites): (bool, Vec<(AccountId, String, Vec<Mailbox>)>, Vec<Mailbox>) = {
        let st = state.borrow();
        let accounts = st
            .accounts
            .iter()
            .map(|(id, handle)| {
                let label = if handle.display_name.is_empty() {
                    handle.email.clone()
                } else {
                    handle.display_name.clone()
                };
                (id.clone(), label, handle.folders.clone())
            })
            .collect();
        // Starred mailboxes are held as bare ids; resolve each against its
        // account's current folder list, dropping any whose folder has since
        // disappeared (or whose account hasn't connected yet).
        let favorites = st
            .favorites
            .iter()
            .filter_map(|mailbox_id| {
                let account_id = mailbox_account_id(mailbox_id)?;
                let handle = st.accounts.get(&account_id)?;
                handle.folders.iter().find(|m| &m.id == mailbox_id).cloned()
            })
            .collect();
        // Don't yank a user already in the unified view back to the first
        // account's Inbox just because some account resynced its folders.
        let auto_select_inbox = st.current_mailbox.is_none() && !matches!(st.mail_view, MailView::UnifiedInbox);
        (auto_select_inbox, accounts, favorites)
    };
    accounts.sort_by_key(|a| a.1.to_lowercase());
    favorites.sort_by_key(|m| m.name.to_lowercase());

    // While the unified view is active, any account whose Inbox just appeared
    // (or reconnected) gets asked to sync so it populates the merged list.
    // Mailboxes with a sync already outstanding are skipped (`syncing`).
    if matches!(state.borrow().mail_view, MailView::UnifiedInbox) {
        let missing: Vec<(AccountId, MailboxId)> = {
            let st = state.borrow();
            st.accounts
                .iter()
                .filter_map(|(id, handle)| {
                    handle
                        .folders
                        .iter()
                        .find(|m| matches!(m.role, MailboxRole::Inbox))
                        .filter(|m| !st.unified_snapshots.contains_key(&m.id) && !st.syncing.contains(&m.id))
                        .map(|m| (id.clone(), m.id.clone()))
                })
                .collect()
        };
        for (account_id, inbox_id) in missing {
            request_mailbox_sync(state, &account_id, &inbox_id);
        }
    }

    let model = build_multi_account_tree_model(accounts, favorites);
    folder_selection.set_model(Some(&model));
    if auto_select_inbox {
        restore_or_default_initial_view(state, &model, folder_selection);
    }
}

/// Chooses the folder pane's starting selection on first load. A remembered
/// view (see `last_view`) is restored when its mailbox is present in the
/// rebuilt tree; a remembered "All Inboxes" restores the unified row. With
/// no memory the pane defaults to "All Inboxes" (row 0). If the remembered
/// mailbox's account hasn't connected yet, the restore is deferred - this
/// does nothing and `rebuild_folder_tree` runs it again on the next
/// `FoldersUpdated`, so a slow account can't lose the user's folder. Only
/// once that account has delivered folders and still doesn't list the
/// mailbox is the restore abandoned for the unified default.
fn restore_or_default_initial_view(state: &Rc<RefCell<UiState>>, model: &gtk::TreeListModel, folder_selection: &gtk::SingleSelection) {
    // Decide without holding a borrow: `set_selected` synchronously fires the
    // `selected-item` handler, which itself borrows `state` mutably.
    let (action, done) = {
        let mut st = state.borrow_mut();
        let action = match st.last_selection.clone() {
            None => Some(0),
            Some(sel) if sel.unified => Some(0),
            Some(sel) => match sel.mailbox {
                Some(mailbox) => {
                    let mailbox_id = MailboxId(mailbox);
                    if let Some(index) = find_mailbox_index(model, &mailbox_id) {
                        Some(index)
                    } else {
                        // Not visible yet. If the mailbox's account has
                        // connected and still doesn't list it, the folder is
                        // gone - fall back to the unified default rather than
                        // waiting forever. Otherwise keep waiting for the
                        // account to connect.
                        let account_gone = mailbox_account_id(&mailbox_id)
                            .and_then(|id| st.accounts.get(&id))
                            .map(|h| !h.folders.is_empty())
                            .unwrap_or(false);
                        account_gone.then_some(0)
                    }
                }
                None => Some(0),
            },
        };
        let done = action.is_some();
        if done {
            st.restore_pending = false;
        }
        (action, done)
    };
    if done {
        if let Some(index) = action {
            folder_selection.set_selected(index);
        }
    }
}

/// Finds the flattened index of a specific mailbox row in the folder tree's
/// `TreeListModel`, by exact `MailboxId`. On the flat `TreeListModel`,
/// iterating top-level indices reaches folder rows inside expanded account
/// groups. Used to restore the remembered view on startup and to put the
/// highlight back after a favorite toggle rebuilds the tree.
///
/// Deliberately matches only `TreeItem::Folder`: a starred mailbox also has a
/// `TreeItem::Favorite` row in the pinned Favorites section, which sorts
/// *above* its real row, so matching both would resolve every favorite to its
/// duplicate.
fn find_mailbox_index(model: &gtk::TreeListModel, mailbox_id: &MailboxId) -> Option<u32> {
    for i in 0..model.n_items() {
        let Some(row) = model.item(i).and_downcast::<gtk::TreeListRow>() else {
            continue;
        };
        let Some(boxed) = row.item().and_downcast::<glib::BoxedAnyObject>() else {
            continue;
        };
        let tree_item = boxed.borrow::<TreeItem>();
        if let TreeItem::Folder(node) = &*tree_item {
            if &node.mailbox.id == mailbox_id {
                return Some(i);
            }
        }
    }
    None
}

/// Wraps `content` directly in a `.card`-styled box - libadwaita's real,
/// built-in style class for a rounded-corner, softly-shaded panel. No
/// per-section header bar: `Gtk.Paned` (see `build_window`) provides the
/// resize handle and visual gap between cards, and the small margin here is
/// what makes each card's rounded corners actually visible against the
/// window background instead of touching the divider/edges.
fn card_section(content: &impl IsA<gtk::Widget>) -> gtk::Box {
    let card = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .css_classes(["card"])
        .overflow(gtk::Overflow::Hidden)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(6)
        .margin_end(6)
        .build();
    card.append(content);
    card
}

/// Maps a mailbox's special-use role to a folder-row icon name, mirroring the
/// role->icon mapping in the reference webmail app's `getIconForMailbox()`
/// (webmail/components/layout/sidebar.tsx). These specific names come from
/// this machine's Yaru icon theme, not the base Adwaita theme - fine for now
/// since there's no working Flatpak build yet, just a manifest spike.
fn folder_icon_name(role: MailboxRole) -> &'static str {
    match role {
        MailboxRole::Inbox => "mail-inbox-symbolic",
        MailboxRole::Sent => "mail-sent-symbolic",
        MailboxRole::Drafts => "mail-drafts-symbolic",
        MailboxRole::Trash => "user-trash-symbolic",
        MailboxRole::Junk => "mail-spam-symbolic",
        MailboxRole::Archive => "mail-archive-symbolic",
        MailboxRole::Custom => "folder-symbolic",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_request_matches_the_current_pending_selection() {
        let pending = Some((MailboxId("acc:inbox".into()), Uid(42)));
        assert!(body_request_matches(&MailboxId("acc:inbox".into()), &Uid(42), pending.as_ref()));
        assert!(!body_request_matches(&MailboxId("acc:inbox".into()), &Uid(43), pending.as_ref()));
    }

    #[test]
    fn body_request_is_rejected_when_no_selection_is_pending() {
        assert!(!body_request_matches(&MailboxId("acc:inbox".into()), &Uid(42), None));
    }

    #[test]
    fn mailbox_account_id_splits_the_account_prefix() {
        assert_eq!(
            mailbox_account_id(&MailboxId("/org/gnome/OnlineAccounts/Accounts/account_7:INBOX".into())),
            Some(AccountId("/org/gnome/OnlineAccounts/Accounts/account_7".into()))
        );
        // No account prefix (an id not shaped "account:path").
        assert_eq!(mailbox_account_id(&MailboxId("INBOX".into())), None);
    }

    #[test]
    fn unified_merge_dedupes_by_mailbox_and_uid_and_sorts_newest_first() {
        let snapshots = HashMap::from([
            (
                MailboxId("a:INBOX".into()),
                vec![summary(Uid(1), "a:INBOX", 2024, 1, 10, 9), summary(Uid(2), "a:INBOX", 2024, 1, 10, 8)],
            ),
            (
                MailboxId("b:INBOX".into()),
                // Duplicate of a:INBOX/2 (same uid, different mailbox) is kept;
                // the doubled a:INBOX/1 in this snapshot must collapse.
                vec![summary(Uid(2), "b:INBOX", 2024, 1, 10, 10), summary(Uid(1), "a:INBOX", 2024, 1, 10, 9)],
            ),
        ]);

        let merged = merge_unified_snapshots(&snapshots);
        let keys: Vec<(String, u32)> = merged.iter().map(|m| (m.mailbox.0.clone(), m.uid.0)).collect();
        // Newest first; a:INBOX/1 appears once despite being in both snapshots.
        assert_eq!(keys, vec![("b:INBOX".into(), 2), ("a:INBOX".into(), 1), ("a:INBOX".into(), 2)]);
    }

    #[test]
    fn identical_message_lists_are_skipped_but_any_displayed_change_is_not() {
        let a = vec![summary(Uid(2), "a:INBOX", 2024, 1, 10, 10), summary(Uid(1), "a:INBOX", 2024, 1, 10, 9)];
        // The startup burst: the same envelope set replayed by cache + live
        // sync + on-demand sync. Display-identical, so no repopulate needed.
        assert!(same_message_list(&a, &a));

        // A message arriving (extra row) is a real change.
        let with_new = vec![
            summary(Uid(3), "a:INBOX", 2024, 1, 10, 11),
            summary(Uid(2), "a:INBOX", 2024, 1, 10, 10),
            summary(Uid(1), "a:INBOX", 2024, 1, 10, 9),
        ];
        assert!(!same_message_list(&a, &with_new));

        // A re-order of the same messages is a real change.
        let reordered = vec![summary(Uid(1), "a:INBOX", 2024, 1, 10, 9), summary(Uid(2), "a:INBOX", 2024, 1, 10, 10)];
        assert!(!same_message_list(&a, &reordered));

        // An unread->read flip is a real change (the row's emphasis changes).
        let mut read_flip = a.clone();
        read_flip[0].flags = {
            use std::collections::BTreeSet;
            let mut flags = BTreeSet::new();
            flags.insert(lookout_core::SystemFlagBit::Seen);
            flags
        };
        assert!(!same_message_list(&a, &read_flip));
    }

    /// True when two ordered message lists are display-identical - the pure
    /// counterpart of `store_matches`, testable without a widget tree.
    fn same_message_list(a: &[EmailSummary], b: &[EmailSummary]) -> bool {
        a.len() == b.len() && a.iter().zip(b).all(|(x, y)| message_row_key(x) == message_row_key(y))
    }

    #[test]
    fn request_mailbox_sync_dedupes_until_answered_or_reconnected() {
        let account_id = AccountId("acc".into());
        let inbox = MailboxId("acc:INBOX".into());
        let (cmd_tx, _cmd_rx) = async_channel::unbounded();
        let state = Rc::new(RefCell::new(UiState {
            accounts: HashMap::from([(
                account_id.clone(),
                AccountHandle {
                    cmd_tx,
                    email: "a@b.c".into(),
                    display_name: String::new(),
                    imap_host: "imap".into(),
                    imap_port: 993,
                    smtp_host: "smtp".into(),
                    smtp_port: 465,
                    folders: Vec::new(),
                },
            )]),
            current_account: None,
            current_mailbox: None,
            mail_view: MailView::Single,
            unified_snapshots: HashMap::new(),
            pending_body_request: None,
            pending_html_reveal: false,
            pending_header: None,
            current_body: None,
            last_selection: None,
            restore_pending: false,
            rendered_message: None,
            syncing: HashSet::new(),
            sort_key: SortKey::Date,
            sort_descending: true,
            favorites: HashSet::new(),
        }));

        // First request goes out and is marked pending.
        assert!(request_mailbox_sync(&state, &account_id, &inbox));
        // A duplicate while the first is still in flight is dropped.
        assert!(!request_mailbox_sync(&state, &account_id, &inbox));

        // The sync landing clears the pending mark, so a later request - a
        // genuine re-sync - is allowed again.
        state.borrow_mut().syncing.remove(&inbox);
        assert!(request_mailbox_sync(&state, &account_id, &inbox));

        // A reconnect (fresh folders) also clears it, so a request that died
        // with a dropped connection can't suppress the next one.
        state.borrow_mut().syncing.insert(inbox.clone());
        state.borrow_mut().syncing.retain(|mailbox| mailbox_account_id(mailbox).as_ref() != Some(&account_id));
        assert!(!state.borrow().syncing.contains(&inbox));
    }

    #[test]
    fn sort_messages_orders_by_the_chosen_key_and_mirrors_exactly_when_ascending() {
        // Same date on the two "b"/"c" rows so the sender/subject keys are what
        // actually decides their order, not the date tie-break.
        let mut messages = vec![
            with_sender_and_subject(summary(Uid(1), "a:INBOX", 2024, 1, 10, 9), "Carol", "Zebra"),
            with_sender_and_subject(summary(Uid(2), "a:INBOX", 2024, 1, 10, 11), "alice", "middle"),
            with_sender_and_subject(summary(Uid(3), "a:INBOX", 2024, 1, 10, 10), "Bob", "Apple"),
        ];

        sort_messages(&mut messages, SortKey::Date, true);
        assert_eq!(uids(&messages), vec![2, 3, 1]);

        // Sender and subject are case-insensitive - "alice" must lead "Bob".
        sort_messages(&mut messages, SortKey::Sender, true);
        assert_eq!(uids(&messages), vec![2, 3, 1]);
        sort_messages(&mut messages, SortKey::Subject, true);
        assert_eq!(uids(&messages), vec![3, 2, 1]);

        // Ascending is the exact mirror of descending, tie-break included.
        sort_messages(&mut messages, SortKey::Sender, false);
        assert_eq!(uids(&messages), vec![1, 3, 2]);
        sort_messages(&mut messages, SortKey::Date, false);
        assert_eq!(uids(&messages), vec![1, 3, 2]);
    }

    #[test]
    fn sort_messages_tie_breaks_equal_keys_by_date_so_rebuilds_are_stable() {
        // Two messages the sender key can't tell apart: only the date decides,
        // and it must decide the same way every time - otherwise identical
        // rebuilds could shuffle and defeat `store_matches`.
        let older = with_sender_and_subject(summary(Uid(1), "a:INBOX", 2024, 1, 10, 9), "Dana", "one");
        let newer = with_sender_and_subject(summary(Uid(2), "a:INBOX", 2024, 1, 10, 12), "Dana", "two");

        let mut forwards = vec![older.clone(), newer.clone()];
        let mut backwards = vec![newer, older];
        sort_messages(&mut forwards, SortKey::Sender, true);
        sort_messages(&mut backwards, SortKey::Sender, true);
        assert_eq!(uids(&forwards), vec![2, 1]);
        assert_eq!(uids(&forwards), uids(&backwards));
    }

    fn uids(messages: &[EmailSummary]) -> Vec<u32> {
        messages.iter().map(|m| m.uid.0).collect()
    }

    fn with_sender_and_subject(mut summary: EmailSummary, sender: &str, subject: &str) -> EmailSummary {
        summary.from = vec![lookout_core::EmailAddress {
            name: Some(sender.to_string()),
            address: format!("{}@example.com", sender.to_lowercase()),
        }];
        summary.subject = Some(subject.to_string());
        summary
    }

    fn summary(uid: Uid, mailbox: &str, year: i32, month: u32, day: u32, hour: u32) -> EmailSummary {
        use chrono::TimeZone;
        use lookout_core::ThreadKey;
        use std::collections::BTreeSet;
        EmailSummary {
            uid,
            mailbox: MailboxId(mailbox.into()),
            message_id: None,
            in_reply_to: None,
            references: vec![],
            thread_key: ThreadKey("t".into()),
            subject: None,
            from: vec![],
            to: vec![],
            cc: vec![],
            date: chrono::Utc.with_ymd_and_hms(year, month, day, hour, 0, 0).unwrap(),
            flags: BTreeSet::new(),
            keywords: BTreeSet::new(),
            size: 0,
            has_attachment: false,
            preview: None,
        }
    }
}

fn body_request_matches(mailbox: &MailboxId, uid: &Uid, pending_request: Option<&(MailboxId, Uid)>) -> bool {
    pending_request.is_some_and(|(pending_mailbox, pending_uid)| pending_mailbox == mailbox && pending_uid == uid)
}

/// Reveals the reading pane's "message" page (switching `content_stack` to
/// `content_page` and the reading stack to "message"). If the stack is
/// mid-transition - e.g. a message → "empty" fade-out still settling after a
/// fast body load - the reveal is deferred until that transition completes,
/// so the next message never appears before the previous one has fully faded
/// away.
fn reveal_message_page(reading_stack: &gtk::Stack, content_stack: &gtk::Stack, content_page: &'static str) {
    if reading_stack.is_transition_running() {
        let reading_stack = reading_stack.clone();
        let content_stack = content_stack.clone();
        let settled = Rc::new(Cell::new(None::<glib::SignalHandlerId>));
        let settled_for_notify = settled.clone();
        let handler_id = reading_stack.connect_notify_local(Some("transition-running"), move |stack, _| {
            if !stack.is_transition_running() {
                if let Some(handler_id) = settled_for_notify.take() {
                    stack.disconnect(handler_id);
                }
                content_stack.set_visible_child_name(content_page);
                stack.set_visible_child_name("message");
            }
        });
        settled.set(Some(handler_id));
    } else {
        content_stack.set_visible_child_name(content_page);
        reading_stack.set_visible_child_name("message");
    }
}

fn render_body(
    state: &Rc<RefCell<UiState>>,
    reading_stack: &gtk::Stack,
    message_header: &crate::message_header::MessageHeader,
    mailbox: MailboxId,
    uid: Uid,
    body: lookout_core::EmailBody,
) {
    // Defense-in-depth for the direct `BodyFetched` caller: if this exact
    // message is already on screen, a duplicate body event must not route the
    // pane through "empty" and crossfade the same email again. The selection
    // handler's `already_shown` guard covers the normal path; this covers a
    // stray re-fetch racing a repopulate. (The selection handler's cached
    // path passes through "empty" before calling this, so a fresh render of
    // the newly-selected message still lands here.)
    let already_shown = {
        let st = state.borrow();
        st.rendered_message.as_ref() == Some(&(mailbox.clone(), uid)) && reading_stack.visible_child_name().as_deref() == Some("message")
    };
    if already_shown {
        return;
    }
    // Remember what's being rendered so a later re-selection of the same
    // message (see the selection handler's `already_shown` guard) can be
    // recognized as a no-op instead of another crossfade.
    state.borrow_mut().rendered_message = Some((mailbox, uid));
    // Apply the header for the message being rendered now. The selection
    // handler stores the summary here instead of updating the header
    // immediately, so the previous message's header stays on screen through
    // its whole fade-out; by the time we get here the pane is already on the
    // "empty" placeholder, so updating the (hidden) header can't flash.
    if let Some(summary) = state.borrow_mut().pending_header.take() {
        message_header.update(&summary);
    }
    // Config → Appearance → "Animate transitions" can switch the stack's
    // transition type to `None`; when it's off, skip the fade-specific paths
    // below (routing through "empty", waiting for the WebView to paint) and
    // swap content in directly, matching the pre-fade behavior.
    let animated = reading_stack.transition_type() != gtk::StackTransitionType::None;
    // The "message" page groups the header with the body's content stack
    // (web view / text view), so revealing it - vs. the "empty" page - is
    // what crossfades, carrying the whole header + body together.
    let Some(content_stack) = reading_stack
        .child_by_name("message")
        .and_downcast::<gtk::Box>()
        .and_then(|page| page.last_child())
        .and_then(|child| child.downcast::<gtk::Stack>().ok())
    else {
        return;
    };
    if let Some(html) = &body.html_body {
        if let Some(web_view) = content_stack.child_by_name("html").and_downcast::<webkit::WebView>() {
            if !animated {
                web_view.load_html(html, None);
                content_stack.set_visible_child_name("html");
                reading_stack.set_visible_child_name("message");
                return;
            }
            // Re-render of the same page: GTK only transitions when the
            // visible child actually changes, so drop back to "empty" first
            // and let the reveal crossfade the fresh body in.
            if reading_stack.visible_child_name().as_deref() == Some("message") {
                reading_stack.set_visible_child_name("empty");
            }
            // WebKit paints asynchronously - crossfading into the HTML page
            // while a fresh body is still loading would show a blank/white
            // page before the message appears. So arm the persistent
            // `load-changed` handler (`pending_html_reveal`) and let it
            // reveal the page once the load completes. The selection handler
            // disarms it on every selection change, so a load started for a
            // message the user has already moved on from can never reveal.
            state.borrow_mut().pending_html_reveal = true;
            web_view.load_html(html, None);
            return;
        }
    }
    if let Some(text) = &body.text_body {
        if let Some(scroller) = content_stack.child_by_name("text").and_downcast::<gtk::ScrolledWindow>() {
            if let Some(text_view) = scroller.child().and_downcast::<gtk::TextView>() {
                text_view.buffer().set_text(text);
                if animated && reading_stack.visible_child_name().as_deref() == Some("message") {
                    // Same-page re-render: route through "empty", wait out
                    // the fade-out, then reveal so both halves of the
                    // transition are visible.
                    let reading_stack_for_reveal = reading_stack.clone();
                    let content_stack_for_reveal = content_stack.clone();
                    let duration = reading_stack.transition_duration() as u64;
                    reading_stack.set_visible_child_name("empty");
                    glib::timeout_add_local_once(std::time::Duration::from_millis(duration), move || {
                        reveal_message_page(&reading_stack_for_reveal, &content_stack_for_reveal, "text");
                    });
                } else {
                    reveal_message_page(reading_stack, &content_stack, "text");
                }
                return;
            }
        }
    }
    reading_stack.set_visible_child_name("empty");
    state.borrow_mut().rendered_message = None;
}

/// Swaps a compose widget into the reading pane's `"compose"` stack page,
/// replacing whatever was showing there (a message, or the empty
/// placeholder). Removes any leftover `"compose"` page first so repeated
/// clicks don't accumulate stale pages, and restores whatever page was
/// visible beforehand once `on_done` fires (Cancel or Send) - so Reply's
/// Cancel lands back on the same message, and New Message's Cancel lands
/// back on the empty placeholder.
fn show_composer_in_reading_pane(
    reading_stack: &gtk::Stack,
    title: &str,
    from_email: String,
    cmd_tx: async_channel::Sender<AccountCommand>,
    prefill: crate::compose::ComposePrefill,
) {
    if let Some(existing) = reading_stack.child_by_name("compose") {
        reading_stack.remove(&existing);
    }
    let previous_page = reading_stack.visible_child_name().map(|s| s.to_string()).unwrap_or_else(|| "empty".to_string());
    let reading_stack_for_close = reading_stack.clone();
    let on_done: Rc<dyn Fn()> = Rc::new(move || {
        if let Some(existing) = reading_stack_for_close.child_by_name("compose") {
            reading_stack_for_close.remove(&existing);
        }
        reading_stack_for_close.set_visible_child_name(&previous_page);
    });
    let composer = crate::compose::build_compose_view(title, from_email, cmd_tx, prefill, on_done);
    reading_stack.add_named(&composer, Some("compose"));
    reading_stack.set_visible_child_name("compose");
}
