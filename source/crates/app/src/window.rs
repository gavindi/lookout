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

use crate::calendar_view::{self, MonthGrid};
use crate::folder_tree::{build_multi_account_tree_model, TreeItem};
use crate::goa_calendar_credentials::GoaCalendarCredentialProvider;
use crate::goa_credentials::GoaCredentialProvider;
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
    folders: Vec<Mailbox>,
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
    /// The most recently fetched message body, kept alongside its
    /// mailbox/uid so Reply/Reply-All/Forward can find it again without a
    /// second fetch - see `selected_message_reply_context`. `None` until the
    /// first `BodyFetched` event lands, or if it's for a message other than
    /// what's currently selected.
    current_body: Option<(MailboxId, Uid, EmailBody)>,
}

/// Per-calendar-account state, kept separate from `UiState`/`AccountHandle`
/// (Mail's equivalents) matching the crate's existing per-domain-type
/// separation - Calendar is a wholly independent account set from Mail.
struct CalendarAccountHandle {
    cmd_tx: async_channel::Sender<CalendarCommand>,
    display_name: String,
    calendars: Vec<CalendarInfo>,
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
    let bg_texture = gtk::gdk::Texture::from_bytes(&glib::Bytes::from_static(bg_bytes))
        .expect("bundled background image should decode");
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
            TreeItem::Account(account) => {
                icon.set_visible(false);
                label.set_label(&account.label);
                label.set_css_classes(&["heading"]);
            }
            TreeItem::Folder(node) => {
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
    let message_factory = gtk::SignalListItemFactory::new();
    message_factory.connect_setup(|_, list_item| {
        let box_ = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(10)
            .margin_end(10)
            .build();
        let top_row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(8).build();
        let sender = gtk::Label::builder().xalign(0.0).hexpand(true).ellipsize(gtk::pango::EllipsizeMode::End).build();
        let date = gtk::Label::builder().xalign(1.0).css_classes(["dim-label", "caption"]).build();
        top_row.append(&sender);
        top_row.append(&date);
        let subject = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["dim-label"])
            .build();
        box_.append(&top_row);
        box_.append(&subject);
        list_item.downcast_ref::<gtk::ListItem>().unwrap().set_child(Some(&box_));
    });
    message_factory.connect_bind(|_, list_item| {
        let list_item = list_item.downcast_ref::<gtk::ListItem>().unwrap();
        let Some(boxed) = list_item.item().and_downcast::<glib::BoxedAnyObject>() else { return };
        let summary = boxed.borrow::<EmailSummary>();
        let Some(box_) = list_item.child().and_downcast::<gtk::Box>() else { return };
        let Some(top_row) = box_.first_child().and_downcast::<gtk::Box>() else { return };
        let sender = top_row.first_child().and_downcast::<gtk::Label>().unwrap();
        let date_label = top_row.last_child().and_downcast::<gtk::Label>().unwrap();
        let subject = box_.last_child().and_downcast::<gtk::Label>().unwrap();

        let from = summary.from.first().map(|a| a.display_label().to_string()).unwrap_or_else(|| "(unknown)".into());
        sender.set_label(&from);
        sender.set_css_classes(if summary.is_unread() { &["heading"] } else { &[] });
        date_label.set_label(&summary.date.format("%Y-%m-%d %H:%M").to_string());
        subject.set_label(summary.subject.as_deref().unwrap_or("(no subject)"));
    });
    let message_list_view = gtk::ListView::new(Some(message_selection.clone()), Some(message_factory));
    let message_scroller = gtk::ScrolledWindow::builder().child(&message_list_view).vexpand(true).build();
    let message_card = card_section(&message_scroller);
    message_card.add_css_class("card-flush-start");
    message_card.set_margin_start(0);
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

    let reading_stack = gtk::Stack::new();
    reading_stack.add_named(&web_view, Some("html"));
    reading_stack.add_named(&text_scroller, Some("text"));
    let reading_empty = gtk::Box::new(gtk::Orientation::Vertical, 0);
    reading_stack.add_named(&reading_empty, Some("empty"));
    reading_stack.set_visible_child_name("empty");
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

    let reading_card = card_section(&reading_stack);

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
    let month_grid = Rc::new(calendar_view::build());
    let calendar_sidebar = calendar_view::build_sidebar();
    let calendar_sidebar_card = card_section(&calendar_sidebar.root);
    calendar_sidebar_card.add_css_class("folder-pane");
    let calendar_paned = gtk::Paned::builder()
        .orientation(gtk::Orientation::Horizontal)
        .start_child(&calendar_sidebar_card)
        .end_child(&month_grid.root)
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

    // --- Menu bar row (File/Home/View/Help). Only File (Quit) and Help
    // (About) have real actions behind them, so only those two are
    // `MenuButton`s with a real popover; Home/View have no ribbon-tab
    // content to show yet, so they're honestly disabled instead of opening
    // an empty popover.
    let file_menu = gio::Menu::new();
    file_menu.append(Some("Quit"), Some("app.quit"));
    let help_menu = gio::Menu::new();
    help_menu.append(Some("About Lookout"), Some("app.about"));

    let file_button = gtk::MenuButton::builder().label("File").css_classes(["flat"]).menu_model(&file_menu).build();
    let home_button = gtk::Button::builder().label("Home").css_classes(["flat"]).sensitive(false).build();
    let view_button = gtk::Button::builder().label("View").css_classes(["flat"]).sensitive(false).build();
    let help_button = gtk::MenuButton::builder().label("Help").css_classes(["flat"]).menu_model(&help_menu).build();

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
    // active. "Month" is the only view that actually exists, so it's the
    // only sensitive item in the Day/Work week/Week/Month/Split view
    // segmented control; everything else here mirrors the Mail toolbar's
    // disabled-placeholder convention.
    let new_event_button = gtk::Button::from_icon_name("appointment-new-symbolic");
    new_event_button.set_tooltip_text(Some("New Event"));
    new_event_button.set_sensitive(false);

    let day_view_button = gtk::ToggleButton::builder().label("Day").sensitive(false).build();
    let work_week_view_button = gtk::ToggleButton::builder().label("Work week").sensitive(false).build();
    let week_view_button = gtk::ToggleButton::builder().label("Week").sensitive(false).build();
    let month_view_button = gtk::ToggleButton::builder().label("Month").active(true).build();
    let split_view_button = gtk::ToggleButton::builder().label("Split view").sensitive(false).build();
    let view_switch_box = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).css_classes(["linked"]).build();
    view_switch_box.append(&day_view_button);
    view_switch_box.append(&work_week_view_button);
    view_switch_box.append(&week_view_button);
    view_switch_box.append(&month_view_button);
    view_switch_box.append(&split_view_button);

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

    let view_toolbar_stack = gtk::Stack::new();
    view_toolbar_stack.add_named(&command_toolbar, Some("mail"));
    view_toolbar_stack.add_named(&calendar_command_toolbar, Some("calendar"));

    // --- View-switcher rail: a narrow, deliberately unstyled (no `.card`,
    // no background) strip along the window's left edge so the background
    // image shows straight through it. Two views today (Mail/Calendar),
    // joined into one toggle group for mutual-exclusive selection.
    let mail_icon_bytes = include_bytes!("../../../data/icons/hicolor/scalable/apps/io.github.gavindi.Lookout.svg");
    let mail_icon_texture = gtk::gdk::Texture::from_bytes(&glib::Bytes::from_static(mail_icon_bytes))
        .expect("bundled app icon should decode");
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
    let mail_overview_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).width_request(240).build();
    mail_overview_box.append(&mail_calendar_overview.root);
    mail_overview_box.append(&mail_overview_day_list);

    let mail_calendar_overview_card = card_section(&mail_overview_box);
    mail_calendar_overview_card.add_css_class("folder-pane");
    mail_calendar_overview_card.set_vexpand(true);

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
        mail_view_button.connect_toggled(move |btn| {
            if btn.is_active() {
                root_stack.set_visible_child_name(current_mail_page.get());
                view_toolbar_stack.set_visible_child_name("mail");
                mail_calendar_overview_card.set_visible(true);
            }
        });
    }
    {
        let root_stack = root_stack.clone();
        let current_calendar_page = current_calendar_page.clone();
        let view_toolbar_stack = view_toolbar_stack.clone();
        let mail_calendar_overview_card = mail_calendar_overview_card.clone();
        calendar_view_button.connect_toggled(move |btn| {
            if btn.is_active() {
                root_stack.set_visible_child_name(current_calendar_page.get());
                view_toolbar_stack.set_visible_child_name("calendar");
                mail_calendar_overview_card.set_visible(false);
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
    let toolbars_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).css_classes(["window-toolbars-background"]).build();
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

    let state = Rc::new(RefCell::new(UiState {
        accounts: HashMap::new(),
        current_account: None,
        current_mailbox: None,
        current_body: None,
    }));
    let calendar_state = Rc::new(RefCell::new(CalendarUiState {
        accounts: HashMap::new(),
        displayed_month: current_month_start(),
        checked_calendar_ids: HashSet::new(),
    }));
    // Which single day the Mail-screen overview pane's event list is
    // currently showing - separate from `calendar_state.displayed_month`
    // (that's the main Calendar view's own concern).
    let mail_overview_day: Rc<Cell<chrono::NaiveDate>> = Rc::new(Cell::new(chrono::Utc::now().date_naive()));
    refresh_mail_overview_day_list(&calendar_state, mail_overview_day.get(), &mail_overview_day_list);

    // --- Compose button -> new-message composer in the reading pane,
    // "From" = the account owning the currently-open mailbox (falling back
    // to any connected account if nothing's been selected yet) ---
    {
        let state = state.clone();
        let reading_stack = reading_stack.clone();
        compose_button.connect_clicked(move |_| {
            let st = state.borrow();
            let account_id = st.current_account.clone().or_else(|| st.accounts.keys().next().cloned());
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
    for (button, mode, title) in [(&reply_button, crate::compose::ReplyMode::Reply, "Reply"), (&reply_all_button, crate::compose::ReplyMode::ReplyAll, "Reply All")] {
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
    {
        let message_selection = message_selection.clone();
        let state = state.clone();
        let reading_stack = reading_stack.clone();
        forward_button.connect_clicked(move |_| {
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
        let reading_stack = reading_stack.clone();
        open_eml_button.connect_clicked(move |_| {
            let window = window.clone();
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
                    render_body(&reading_stack, body);
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
    // own account (selecting an account-group row itself is a no-op - it
    // just expands/collapses) ---
    {
        let state = state.clone();
        folder_selection.connect_selected_item_notify(move |sel| {
            let Some(row) = sel.selected_item().and_downcast::<gtk::TreeListRow>() else { return };
            let Some(boxed) = row.item().and_downcast::<glib::BoxedAnyObject>() else { return };
            let tree_item = boxed.borrow::<TreeItem>();
            let TreeItem::Folder(node) = &*tree_item else { return };
            let mailbox_id = node.mailbox.id.clone();
            let account_id = node.mailbox.account_id.clone();
            drop(tree_item);

            let mut st = state.borrow_mut();
            st.current_account = Some(account_id.clone());
            st.current_mailbox = Some(mailbox_id.clone());
            if let Some(handle) = st.accounts.get(&account_id) {
                let _ = handle.cmd_tx.send_blocking(AccountCommand::SyncMailbox(mailbox_id));
            }
        });
    }

    // --- Message selection -> AccountCommand::FetchBody on the current account ---
    {
        let state = state.clone();
        let reading_stack = reading_stack.clone();
        message_selection.connect_selected_item_notify(move |sel| {
            let Some(boxed) = sel.selected_item().and_downcast::<glib::BoxedAnyObject>() else {
                return;
            };
            let summary = boxed.borrow::<EmailSummary>();
            let uid = summary.uid;
            let mailbox = summary.mailbox.clone();
            drop(summary);

            let st = state.borrow();
            if let Some(handle) = st.current_account.as_ref().and_then(|id| st.accounts.get(id)) {
                let _ = handle.cmd_tx.send_blocking(AccountCommand::FetchBody { mailbox, uid });
            }
            drop(st);
            // Also silently abandons an in-progress composer in the reading
            // pane, if one's open - no "discard draft?" prompt, consistent
            // with this app's existing no-confirmation-dialog convention.
            reading_stack.set_visible_child_name("empty");
        });
    }

    // --- Delete/Archive/Report -> AccountCommand::MoveMessage against the
    // account's Trash/Archive/Junk mailbox; Snooze -> AccountCommand::
    // SnoozeMessage with a single fixed "tomorrow 9:00 AM local time"
    // default. All four are silent no-ops with nothing selected.
    for (button, role) in [(&delete_button, MailboxRole::Trash), (&archive_button, MailboxRole::Archive), (&report_button, MailboxRole::Junk)] {
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
                let _ = cmd_tx.send_blocking(AccountCommand::SnoozeMessage { mailbox, uid, until: tomorrow_9am });
            }
        });
    }

    // --- Month grid navigation: prev/next/Today update the grid locally
    // (immediate redraw of the date labels) and ask every connected
    // calendar account to resync the newly-displayed month.
    {
        let calendar_state = calendar_state.clone();
        let month_grid = month_grid.clone();
        let mini_calendar = calendar_sidebar.mini_calendar.clone();
        let prev_button = month_grid.prev_button.clone();
        prev_button.connect_clicked(move |_| {
            let current = calendar_state.borrow().displayed_month;
            let new_month = current.checked_sub_months(chrono::Months::new(1)).unwrap_or(current);
            show_month(&calendar_state, &month_grid, &mini_calendar, new_month);
        });
    }
    {
        let calendar_state = calendar_state.clone();
        let month_grid = month_grid.clone();
        let mini_calendar = calendar_sidebar.mini_calendar.clone();
        let next_button = month_grid.next_button.clone();
        next_button.connect_clicked(move |_| {
            let current = calendar_state.borrow().displayed_month;
            let new_month = current.checked_add_months(chrono::Months::new(1)).unwrap_or(current);
            show_month(&calendar_state, &month_grid, &mini_calendar, new_month);
        });
    }
    {
        let calendar_state = calendar_state.clone();
        let month_grid = month_grid.clone();
        let mini_calendar = calendar_sidebar.mini_calendar.clone();
        let today_button = month_grid.today_button.clone();
        today_button.connect_clicked(move |_| {
            show_month(&calendar_state, &month_grid, &mini_calendar, current_month_start());
        });
    }
    // --- Sidebar mini-calendar -> jump the main grid to whatever month the
    // clicked date belongs to (same `show_month` helper the main grid's own
    // prev/next/Today buttons use).
    {
        let calendar_state = calendar_state.clone();
        let month_grid = month_grid.clone();
        let mini_calendar = calendar_sidebar.mini_calendar.clone();
        calendar_view::connect_day_selected(&calendar_sidebar.mini_calendar, move |date| {
            show_month(&calendar_state, &month_grid, &mini_calendar, date);
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
        message_store,
        reading_stack,
        current_mail_page,
        mail_view_button,
    );
    spawn_calendar_discovery(
        worker,
        calendar_state,
        root_stack,
        toast_overlay,
        month_grid,
        calendar_sidebar.calendar_list_box,
        mail_overview_day,
        mail_overview_day_list,
        current_calendar_page,
        calendar_view_button,
    );

    window
}

/// Updates `calendar_state.displayed_month`, redraws the main grid's date
/// labels immediately (`calendar_view::set_month` clears every cell's
/// events, so this shows an empty grid for the new month until the resync
/// below lands) and keeps the sidebar's mini-calendar showing the same
/// month, then asks every connected calendar account to resync it.
fn show_month(calendar_state: &Rc<RefCell<CalendarUiState>>, month_grid: &MonthGrid, mini_calendar: &calendar_view::MiniCalendar, new_month: chrono::NaiveDate) {
    calendar_view::set_month(month_grid, new_month);
    calendar_view::set_mini_month(mini_calendar, new_month);
    let mut st = calendar_state.borrow_mut();
    st.displayed_month = new_month;
    for handle in st.accounts.values() {
        let _ = handle.cmd_tx.send_blocking(CalendarCommand::SyncMonth(new_month));
    }
}

fn current_month_start() -> chrono::NaiveDate {
    first_of_month(chrono::Utc::now().date_naive())
}

fn first_of_month(date: chrono::NaiveDate) -> chrono::NaiveDate {
    use chrono::Datelike;
    date.with_day(1).unwrap_or(date)
}

#[allow(clippy::too_many_arguments)]
fn spawn_account_discovery(
    worker: Rc<Worker>,
    state: Rc<RefCell<UiState>>,
    root_stack: gtk::Stack,
    toast_overlay: adw::ToastOverlay,
    folder_selection: gtk::SingleSelection,
    message_store: gio::ListStore,
    reading_stack: gtk::Stack,
    current_mail_page: Rc<Cell<&'static str>>,
    mail_view_button: gtk::ToggleButton,
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
                        message_store.clone(),
                        reading_stack.clone(),
                        toast_overlay.clone(),
                        client.clone(),
                        account,
                    );
                }
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
    message_store: gio::ListStore,
    reading_stack: gtk::Stack,
    toast_overlay: adw::ToastOverlay,
    goa_client: GoaClient,
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
                    if let Some(handle) = state.borrow_mut().accounts.get_mut(&account_id) {
                        handle.folders = folders;
                    }
                    rebuild_folder_tree(&state.borrow(), &folder_selection);
                }
                AccountEvent::MessagesUpdated { mailbox, messages } => {
                    let should_display = {
                        let mut st = state.borrow_mut();
                        // Nothing selected yet (fresh startup): adopt
                        // whichever account's initial inbox sync lands
                        // first as the default view, rather than leaving
                        // the message list empty until the user clicks a
                        // folder. Whichever connected account happens to
                        // finish its first sync first wins this race -
                        // an acceptable, benign nondeterminism for Phase 1.
                        if st.current_mailbox.is_none() {
                            st.current_account = Some(account_id.clone());
                            st.current_mailbox = Some(mailbox.clone());
                        }
                        st.current_mailbox.as_ref() == Some(&mailbox)
                    };
                    if should_display {
                        message_store.remove_all();
                        // Newest first for the reading list.
                        let mut messages = messages;
                        messages.sort_by_key(|m| std::cmp::Reverse(m.date));
                        for m in messages {
                            message_store.append(&glib::BoxedAnyObject::new(m));
                        }
                    }
                }
                AccountEvent::BodyFetched { mailbox, uid, body } => {
                    state.borrow_mut().current_body = Some((mailbox, uid, body.clone()));
                    render_body(&reading_stack, body);
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
    month_grid: Rc<MonthGrid>,
    calendar_list_box: gtk::Box,
    mail_overview_day: Rc<Cell<chrono::NaiveDate>>,
    mail_overview_day_list: gtk::Box,
    current_calendar_page: Rc<Cell<&'static str>>,
    calendar_view_button: gtk::ToggleButton,
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
                        month_grid.clone(),
                        calendar_list_box.clone(),
                        mail_overview_day.clone(),
                        mail_overview_day_list.clone(),
                        toast_overlay.clone(),
                        client.clone(),
                        account,
                    );
                }
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

/// Recomputes which calendars actually exist across every connected account,
/// defaults any newly-seen id to checked (shown), and re-renders the
/// sidebar's "My calendars" checklist against that - the checklist's own
/// `on_toggle` closure flips membership in `checked_calendar_ids` and calls
/// `refresh_displayed_calendar_view` to redraw the grid accordingly.
fn refresh_calendar_checklist(calendar_state: &Rc<RefCell<CalendarUiState>>, calendar_list_box: &gtk::Box, month_grid: &Rc<MonthGrid>) {
    let all_calendars: Vec<CalendarInfo> = calendar_state.borrow().accounts.values().flat_map(|h| h.calendars.iter().cloned()).collect();
    {
        let mut st = calendar_state.borrow_mut();
        for cal in &all_calendars {
            if !st.checked_calendar_ids.contains(&cal.id) {
                st.checked_calendar_ids.insert(cal.id.clone());
            }
        }
    }
    let checked = calendar_state.borrow().checked_calendar_ids.clone();
    let on_toggle = {
        let calendar_state = calendar_state.clone();
        let month_grid = month_grid.clone();
        move |id: CalendarId, is_checked: bool| {
            {
                let mut st = calendar_state.borrow_mut();
                if is_checked {
                    st.checked_calendar_ids.insert(id);
                } else {
                    st.checked_calendar_ids.remove(&id);
                }
            }
            refresh_displayed_calendar_view(&calendar_state, &month_grid);
        }
    };
    calendar_view::rebuild_calendar_checklist(calendar_list_box, &all_calendars, &checked, on_toggle);
}

/// Unions every connected calendar account's latest occurrences for
/// whatever month is currently displayed - filtered to only the calendars
/// currently checked in the sidebar - and redraws the grid. Same
/// "only apply if it matches what's on screen" + "merge all accounts'
/// latest snapshot" approach as Mail's `MessagesUpdated`/`rebuild_folder_tree`.
fn refresh_displayed_calendar_view(calendar_state: &Rc<RefCell<CalendarUiState>>, month_grid: &MonthGrid) {
    let st = calendar_state.borrow();
    let month = st.displayed_month;
    let merged: Vec<EventOccurrence> = st
        .accounts
        .values()
        .filter(|h| h.last_synced_month == Some(month))
        .flat_map(|h| h.last_occurrences.iter().filter(|occ| st.checked_calendar_ids.contains(&occ.calendar_id)).cloned())
        .collect();
    drop(st);
    calendar_view::set_occurrences(month_grid, &merged);
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
                format!("{} {}", occ.start.with_timezone(&chrono::Local).format("%H:%M"), occ.summary.as_deref().unwrap_or("(untitled)"))
            };
            let label = gtk::Label::builder().label(&text).xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).css_classes(["caption"]).build();
            day_list_box.append(&label);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn connect_calendar_account(
    worker: Rc<Worker>,
    calendar_state: Rc<RefCell<CalendarUiState>>,
    month_grid: Rc<MonthGrid>,
    calendar_list_box: gtk::Box,
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
            calendars: Vec::new(),
            last_occurrences: Vec::new(),
            last_synced_month: None,
        },
    );

    worker.spawn(lookout_dav::session::run_calendar_session(config, credentials, cmd_rx, evt_tx));

    glib::spawn_future_local(async move {
        while let Ok(event) = evt_rx.recv().await {
            match event {
                CalendarSessionEvent::ConnectionStateChanged(CalConnectionState::Error { message, .. }) => {
                    toast_overlay.add_toast(adw::Toast::new(&format!("{}: {message}", calendar_account_label(&calendar_state, &account_id))));
                }
                CalendarSessionEvent::ConnectionStateChanged(_) => {}
                CalendarSessionEvent::CalendarsUpdated(calendars) => {
                    if let Some(handle) = calendar_state.borrow_mut().accounts.get_mut(&account_id) {
                        handle.calendars = calendars;
                    }
                    refresh_calendar_checklist(&calendar_state, &calendar_list_box, &month_grid);
                }
                CalendarSessionEvent::OccurrencesUpdated { month, occurrences } => {
                    if let Some(handle) = calendar_state.borrow_mut().accounts.get_mut(&account_id) {
                        handle.last_occurrences = occurrences;
                        handle.last_synced_month = Some(month);
                    }
                    refresh_displayed_calendar_view(&calendar_state, &month_grid);
                    refresh_mail_overview_day_list(&calendar_state, mail_overview_day.get(), &mail_overview_day_list);
                }
                CalendarSessionEvent::Error(message) => {
                    toast_overlay.add_toast(adw::Toast::new(&format!("{}: {message}", calendar_account_label(&calendar_state, &account_id))));
                }
            }
        }
    });
}

fn calendar_account_label(state: &Rc<RefCell<CalendarUiState>>, account_id: &AccountId) -> String {
    state.borrow().accounts.get(account_id).map(|h| h.display_name.clone()).unwrap_or_else(|| account_id.0.clone())
}

/// Resolves the currently-selected message in `message_selection` to its
/// mailbox/uid and its owning account's command channel, for the
/// Delete/Archive/Report/Snooze button handlers - mirrors the lookup already
/// done inline by the `FetchBody`-on-selection handler above. Returns `None`
/// if nothing is selected or its account has since disconnected, in which
/// case the calling handler is a silent no-op.
fn selected_message_command_target(message_selection: &gtk::SingleSelection, state: &Rc<RefCell<UiState>>) -> Option<(MailboxId, Uid, async_channel::Sender<AccountCommand>)> {
    let boxed = message_selection.selected_item().and_downcast::<glib::BoxedAnyObject>()?;
    let summary = boxed.borrow::<EmailSummary>();
    let uid = summary.uid;
    let mailbox = summary.mailbox.clone();
    drop(summary);

    let st = state.borrow();
    let cmd_tx = st.current_account.as_ref().and_then(|id| st.accounts.get(id)).map(|handle| handle.cmd_tx.clone())?;
    Some((mailbox, uid, cmd_tx))
}

/// Resolves the currently-selected message plus its already-fetched body,
/// for the Reply/Reply-All/Forward button handlers. Returns `None` if
/// nothing is selected, its account has disconnected, or the cached
/// `current_body` doesn't match the selection (its body hasn't arrived yet,
/// or is stale from a previous selection) - the calling handler is then a
/// silent no-op, same convention as `selected_message_command_target`.
fn selected_message_reply_context(message_selection: &gtk::SingleSelection, state: &Rc<RefCell<UiState>>) -> Option<(EmailSummary, EmailBody, String, async_channel::Sender<AccountCommand>)> {
    let boxed = message_selection.selected_item().and_downcast::<glib::BoxedAnyObject>()?;
    let summary = boxed.borrow::<EmailSummary>().clone();

    let st = state.borrow();
    let (body_mailbox, body_uid, body) = st.current_body.as_ref()?;
    if *body_mailbox != summary.mailbox || *body_uid != summary.uid {
        return None;
    }
    let handle = st.current_account.as_ref().and_then(|id| st.accounts.get(id))?;
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
fn rebuild_folder_tree(state: &UiState, folder_selection: &gtk::SingleSelection) {
    let mut accounts: Vec<(AccountId, String, Vec<Mailbox>)> = state
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
    accounts.sort_by_key(|a| a.1.to_lowercase());

    let model = build_multi_account_tree_model(accounts);
    folder_selection.set_model(Some(&model));
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

fn render_body(reading_stack: &gtk::Stack, body: lookout_core::EmailBody) {
    if let Some(html) = &body.html_body {
        if let Some(web_view) = reading_stack.child_by_name("html").and_downcast::<webkit::WebView>() {
            web_view.load_html(html, None);
            reading_stack.set_visible_child_name("html");
            return;
        }
    }
    if let Some(text) = &body.text_body {
        if let Some(scroller) = reading_stack.child_by_name("text").and_downcast::<gtk::ScrolledWindow>() {
            if let Some(text_view) = scroller.child().and_downcast::<gtk::TextView>() {
                text_view.buffer().set_text(text);
                reading_stack.set_visible_child_name("text");
                return;
            }
        }
    }
    reading_stack.set_visible_child_name("empty");
}

/// Swaps a compose widget into the reading pane's `"compose"` stack page,
/// replacing whatever was showing there (a message, or the empty
/// placeholder). Removes any leftover `"compose"` page first so repeated
/// clicks don't accumulate stale pages, and restores whatever page was
/// visible beforehand once `on_done` fires (Cancel or Send) - so Reply's
/// Cancel lands back on the same message, and New Message's Cancel lands
/// back on the empty placeholder.
fn show_composer_in_reading_pane(reading_stack: &gtk::Stack, title: &str, from_email: String, cmd_tx: async_channel::Sender<AccountCommand>, prefill: crate::compose::ComposePrefill) {
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
