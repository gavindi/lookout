//! Config view: a read-only overview of the Mail/Calendar accounts Lookout
//! is connected to, a live Appearance section (the smooth-transitions
//! preference, the window-background image picker and its dimming slider), live Mail and Calendar
//! preference groups (the remote-images/rich-text toggles and the event-alerts
//! toggle), a live Keyboard-shortcuts section (per-action
//! capture rows over the `shortcuts` GSettings key), the Appearance page
//! (theme, accent, window background, and the folder/message-list vertical-
//! spacing choice), disabled placeholder
//! sections mirroring the rest of the Phase 5 settings taxonomy (Privacy) and a live
//! Advanced section with a cache-clear action. Data-in/widget-out like
//! `calendar_view.rs` and `folder_tree.rs`: the caller (window.rs) owns the
//! session state and feeds plain display structs into `refresh`.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

/// The caller's hook for opening the manage-identities dialog: takes the
/// anchoring widget and the account id (opaque string).
pub type ManageIdentities = Rc<dyn Fn(&gtk::Widget, &str)>;

/// The caller's hook for one manual ("other") account's row actions: takes
/// the anchoring widget and the account id. Used for both editing (opens the
/// account dialog prefilled) and removing (confirms, then tears the account
/// down) - the caller distinguishes the two by which closure it registers.
pub type OtherAccountAction = Rc<dyn Fn(&gtk::Widget, &str)>;

/// The caller's hook for a GOA account row's switch: takes the account id
/// (opaque string) and the switch's new state. The caller owns the actual
/// connect/teardown and persistence.
pub type AccountToggle = Rc<dyn Fn(&str, bool)>;

/// Plain display data for one GOA account in the Accounts section's
/// enable/disable list - the union of every account the discoveries saw,
/// disabled ones included, so a disabled account stays toggleable.
pub struct GoaAccountInfo {
    pub display_name: String,
    pub email: String,
    /// The account's id, as an opaque string - handed back to the caller's
    /// toggle callback so it can route the change to the right account.
    pub account_id: String,
    /// Whether the account is currently enabled (all accounts are, unless
    /// the user disabled one).
    pub enabled: bool,
}

/// Plain display data for one mail account, decoupled from window.rs's
/// private `AccountHandle` so this module has no dependency on the session
/// types.
pub struct MailAccountInfo {
    pub display_name: String,
    pub email: String,
    /// The account's id, as an opaque string - handed back to the caller's
    /// manage-identities callback so it can route edits to the right
    /// account.
    pub account_id: String,
    /// True for accounts Lookout manages itself (manual IMAP/SMTP, ids
    /// prefixed `other:`); such rows get Edit/Remove actions since they're
    /// owned by the app, unlike GOA accounts which are managed in system
    /// settings.
    pub is_other: bool,
    /// Every identity this account can send as, display-ready.
    pub identity_labels: Vec<String>,
    /// Preformatted `host:port`.
    pub imap: String,
    /// Preformatted `host:port`.
    pub smtp: String,
}

/// Plain display data for one calendar account.
pub struct CalendarAccountInfo {
    pub display_name: String,
    /// The account's CalDAV base URL.
    pub uri: String,
}

/// Plain display data for one webcal subscription feed.
pub struct WebcalSubscriptionInfo {
    pub display_name: String,
    /// The feed URL as the user configured it.
    pub url: String,
}

/// One cached database file to display in the Advanced section.
pub struct CacheFile {
    pub name: String,
    pub size: String,
}

/// One editable keyboard-shortcut row: the action it edits and the label
/// showing its current accelerator. The caller refreshes the label from the
/// live shortcut table and wires the row's activation to the capture flow.
pub struct ShortcutRow {
    pub row: adw::ActionRow,
    /// Shows the canonical accelerator string (`<Primary>n`); "Press keys…"
    /// while a replacement is being recorded.
    pub label: gtk::Label,
    /// The action id this row edits (see `shortcuts`).
    pub action: &'static str,
}

/// The config screen's widget tree. `root` goes into the main `root_stack`;
/// the account groups are kept here so `refresh` can repopulate them in
/// place without rebuilding the whole page.
pub struct ConfigView {
    pub root: gtk::Widget,
    /// The sidebar/content split itself, exposed so the caller can wire its
    /// `position` to the same persist-width-as-a-percentage mechanism used
    /// by the Mail/Calendar/Contacts panes.
    pub paned: gtk::Paned,
    /// "Add account…" entry row, exposed so the caller can wire its
    /// `activated` signal to the same GOA-settings invocation the empty-state
    /// page uses.
    pub add_account_row: adw::ActionRow,
    /// "Add IMAP/SMTP account…" entry row - the entry point for the app's
    /// own manually-managed mail accounts (see `other_accounts.rs`), wired
    /// by the caller to the add-account dialog.
    pub add_imap_row: adw::ActionRow,
    /// The GOA accounts enable/disable list, rebuilt by `refresh` like the
    /// mail/calendar groups.
    goa_group: adw::PreferencesGroup,
    goa_rows: RefCell<Vec<adw::SwitchRow>>,
    /// "Clear all caches…" row, exposed so the caller can wire its
    /// `activated` signal to the actual cache-clearing (the mail cache lives
    /// in the `lookout-mail` crate, out of this module's reach).
    pub clear_cache_row: adw::ActionRow,
    /// "Animate transitions" switch, exposed so the caller (which owns the
    /// widgets that animate) can wire its `active` state to disable the
    /// reading pane's crossfades.
    pub animations_row: adw::SwitchRow,
    /// "Theme" dropdown (Config → Appearance), exposed so the caller can wire
    /// its selection into the `ThemeManager` and seed it from GSettings.
    pub theme_row: adw::ComboRow,
    /// "Dark message theme" switch (Config → Appearance), exposed so the
    /// caller can seed it from GSettings. The default (off) is light mode -
    /// messages keep their original background and text; when on,
    /// newly-opened HTML messages start with the header's "Switch message
    /// theme" override applied.
    pub message_theme_dark_row: adw::SwitchRow,
    /// "Vertical spacing" dropdown (Config → Appearance), exposed so the caller
    /// can seed it from the `layout-vertical-spacing` GSettings key and map
    /// its selection to the `spacing-*` CSS class on the folder and message
    /// list panes.
    pub spacing_row: adw::ComboRow,
    /// "Custom accent color" switch (Config → Appearance), exposed so the
    /// caller can arm the accent picker row and map its state to the
    /// `accent-color` GSettings key (off = follow the system accent).
    pub accent_switch_row: adw::SwitchRow,
    /// "Accent color" picker row, exposed so the caller can wire its colour
    /// button into the `ThemeManager`; disabled until the switch above is on.
    pub accent_color_row: adw::ActionRow,
    pub accent_color_button: gtk::ColorDialogButton,
    /// "Window background" row, exposed so the caller can wire its `activated`
    /// signal to a file chooser and update its subtitle to reflect the image
    /// currently in use.
    pub background_image_row: adw::ActionRow,
    /// The "Background dimming" slider, exposed so the caller can wire its
    /// `value-changed` signal into the window background's brightness and
    /// seed it with the stored value. Always enabled - dimming applies to
    /// the bundled artwork as well as a custom background - so the row
    /// itself needs no caller-side wiring beyond this.
    pub background_brightness_scale: gtk::Scale,
    /// "Restore default background" row, exposed so the caller can wire its
    /// `activated` signal back to the bundled artwork (and re-enable it only
    /// when a custom image is actually set).
    pub restore_background_row: adw::ActionRow,
    /// "Load images from the web" switch (Config → Mail), exposed so the
    /// caller can wire its `active` state into the reading pane's remote-image
    /// policy.
    pub remote_images_row: adw::SwitchRow,
    /// "Trusted senders…" row (Config → Mail), exposed so the caller can wire
    /// its `activated` signal to the external-content trust manage dialog
    /// (and refresh the subtitle after a change).
    pub trusted_senders_row: adw::ActionRow,
    /// "Rich text" switch (Config → Mail), exposed so the caller can wire its
    /// `active` state into the composer's default body mode.
    pub rich_text_row: adw::SwitchRow,
    /// "Send read receipts automatically" switch (Config → Mail), exposed so
    /// the caller can wire its `active` state into the reading pane's
    /// read-receipt policy (read per message at display time).
    pub read_receipts_row: adw::SwitchRow,
    /// "Mail notifications" switch (Config → Mail), exposed so the caller can
    /// wire its `active` state into the `Gio::Notification` new-mail/send-
    /// failure notifications.
    pub mail_notifications_row: adw::SwitchRow,
    /// "Event alerts" switch (Config → Calendar), exposed so the caller can
    /// wire its `active` state into the `Gio::Notification` reminder
    /// scheduling.
    pub calendar_alerts_row: adw::SwitchRow,
    /// "OAuth client id" row (Config → Google Tasks), exposed so the caller
    /// can wire its `activated` signal to the client-id dialog (and update
    /// the subtitle after a save).
    pub google_tasks_client_row: adw::ActionRow,
    /// The per-action keyboard-shortcut rows (Config → Keyboard shortcuts),
    /// exposed so the caller can seed their labels from the live shortcut
    /// table and wire each row's activation to the chord-capture flow.
    pub keyboard_rows: RefCell<Vec<ShortcutRow>>,
    /// "Restore default shortcuts" row at the bottom of the keyboard group,
    /// exposed so the caller can wire it to `ShortcutState::reset_all`.
    pub reset_shortcuts_row: adw::ActionRow,
    /// "Start Lookout at login" switch (Config → General), exposed so the
    /// caller can wire it to the background-portal / XDG-autostart
    /// registration.
    pub start_at_login_row: adw::SwitchRow,
    /// "Keep running when the window is closed" switch (Config → General),
    /// exposed so the caller can wire it into the main window's
    /// close-request handling.
    pub close_to_background_row: adw::SwitchRow,
    mail_group: adw::PreferencesGroup,
    calendar_group: adw::PreferencesGroup,
    webcal_group: adw::PreferencesGroup,
    mail_rows: RefCell<Vec<adw::ActionRow>>,
    calendar_rows: RefCell<Vec<adw::ActionRow>>,
    webcal_rows: RefCell<Vec<adw::ActionRow>>,
    /// The per-account "Identities" rows, rebuilt alongside `mail_rows`.
    identity_rows: RefCell<Vec<adw::ActionRow>>,
    mail_cache_group: adw::PreferencesGroup,
    calendar_cache_group: adw::PreferencesGroup,
    contacts_cache_group: adw::PreferencesGroup,
    mail_cache_rows: RefCell<Vec<adw::ActionRow>>,
    calendar_cache_rows: RefCell<Vec<adw::ActionRow>>,
    contacts_cache_rows: RefCell<Vec<adw::ActionRow>>,
}

/// Builds a section's screen: a scrolling `adw::PreferencesPage` holding
/// `groups` (in order), registered on `stack` under `name`. `sections`
/// collects the `(label, name)` pair so the sidebar list can be built once
/// every section has been registered.
fn add_section(stack: &gtk::Stack, sections: &mut Vec<(&'static str, &'static str)>, label: &'static str, name: &'static str, groups: &[&adw::PreferencesGroup]) {
    let section_page = adw::PreferencesPage::new();
    section_page.set_vexpand(true);
    for group in groups {
        section_page.add(*group);
    }
    stack.add_named(&section_page, Some(name));
    sections.push((label, name));
}

/// A disabled "Not implemented yet" placeholder group for a not-yet-built
/// section of the Phase 5 settings taxonomy - same honest-disabled
/// convention as the menu bar's Home/View ribbon tabs.
fn placeholder_group(title: &str) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title(title).build();
    let row = adw::ActionRow::builder().title("Not implemented yet").build();
    row.set_sensitive(false);
    group.add(&row);
    group
}

/// Wraps `content` in the app's rounded/bordered ".card" treatment (mirrors
/// window.rs's private `card_section` helper - duplicated here rather than
/// shared so this module stays decoupled from window.rs, per its own
/// data-in/widget-out convention).
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

pub fn build() -> ConfigView {
    let accounts_group = adw::PreferencesGroup::builder().title("Accounts").build();
    let add_account_row = adw::ActionRow::builder()
        .title("Add account…")
        .subtitle("Open GNOME Online Accounts settings")
        .activatable(true)
        .build();
    accounts_group.add(&add_account_row);
    let add_imap_row = adw::ActionRow::builder()
        .title("Add IMAP/SMTP account…")
        .subtitle("Connect a mail account not managed by GNOME Online Accounts")
        .activatable(true)
        .build();
    accounts_group.add(&add_imap_row);

    let mail_group = adw::PreferencesGroup::builder().title("Mail accounts").build();

    // The enable/disable list for GNOME Online Accounts accounts. Repopulated
    // by `refresh` with one switch row per discovered account.
    let goa_group = adw::PreferencesGroup::builder().title("GNOME Online Accounts").build();

    let calendar_group = adw::PreferencesGroup::builder().title("Calendar accounts").build();

    let webcal_group = adw::PreferencesGroup::builder().title("Webcal subscriptions").build();

    let appearance_group = adw::PreferencesGroup::builder().title("Appearance").build();
    let animations_row = adw::SwitchRow::builder()
        .title("Animate transitions")
        .subtitle("Fade between views when switching messages")
        .active(true)
        .build();
    appearance_group.add(&animations_row);
    let theme_model = gtk::StringList::new(&["System", "Flat Dark", "Flat Light"]);
    let theme_row = adw::ComboRow::builder()
        .title("Theme")
        .subtitle("Flat Dark is the classic Lookout look; System follows the desktop scheme")
        .model(&theme_model)
        .build();
    appearance_group.add(&theme_row);
    // The per-message "Switch message theme" default. Off (the default) is
    // light mode: messages keep their original background and text; on,
    // newly-opened HTML messages start dark - background stripped, colours
    // inverted - while the header's moon/sun toggle still overrides the
    // current message either way.
    let message_theme_dark_row = adw::SwitchRow::builder()
        .title("Dark message theme")
        .subtitle("Open HTML messages in dark mode by default instead of their original colours")
        .build();
    appearance_group.add(&message_theme_dark_row);
    let accent_switch_row = adw::SwitchRow::builder()
        .title("Custom accent color")
        .subtitle("Use a Lookout-specific accent instead of the system one")
        .build();
    appearance_group.add(&accent_switch_row);
    // The picker row only matters once the switch above is on, so it starts
    // disabled and the caller arms it with the switch. The button needs a
    // colour to show from the start - it's replaced by the persisted accent
    // (or left unused) when the switch is off anyway. The dialog is created
    // explicitly: GTK does not auto-create one for a NULL button dialog.
    let accent_color_row = adw::ActionRow::builder().title("Accent color").build();
    let accent_color_dialog = gtk::ColorDialog::new();
    let accent_color_button = gtk::ColorDialogButton::new(Some(accent_color_dialog));
    accent_color_button.set_rgba(&gtk::gdk::RGBA::new(0.21, 0.52, 0.89, 1.0));
    accent_color_button.set_halign(gtk::Align::End);
    accent_color_row.set_child(Some(&accent_color_button));
    accent_color_row.set_sensitive(false);
    appearance_group.add(&accent_color_row);
    let background_image_row = adw::ActionRow::builder()
        .title("Window background")
        .subtitle("Default Lookout artwork")
        .activatable(true)
        .build();
    appearance_group.add(&background_image_row);
    // Dimming applies to whichever background is currently shown - the
    // bundled artwork included - so this row is always enabled; the caller
    // just seeds it with the stored brightness at startup.
    let background_brightness_row = adw::ActionRow::builder().title("Background dimming").subtitle("Reduce the background toward black").build();
    let background_brightness_scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 0.05);
    background_brightness_scale.set_value(0.75);
    background_brightness_scale.set_draw_value(false);
    background_brightness_row.set_child(Some(&background_brightness_scale));
    appearance_group.add(&background_brightness_row);
    let restore_background_row = adw::ActionRow::builder()
        .title("Restore default background")
        .subtitle("Use the bundled artwork again")
        .activatable(true)
        .sensitive(false)
        .build();
    appearance_group.add(&restore_background_row);

    // The real "Mail" group, replacing that section's placeholder: the
    // reading pane's remote-images toggle and the composer's rich-text
    // default, wired by the caller into WebKit's load policy and the compose
    // window respectively.
    let mail_settings_group = adw::PreferencesGroup::builder().title("Mail").build();
    let remote_images_row = adw::SwitchRow::builder()
        .title("Load images from the web")
        .subtitle("Display images hosted on remote servers in messages")
        .build();
    mail_settings_group.add(&remote_images_row);
    let trusted_senders_row = adw::ActionRow::builder()
        .title("Trusted senders…")
        .subtitle("Choose who may load remote images and other external content")
        .activatable(true)
        .build();
    mail_settings_group.add(&trusted_senders_row);
    let rich_text_row = adw::SwitchRow::builder()
        .title("Rich text")
        .subtitle("Start new messages in the formatted editor")
        .active(true)
        .build();
    mail_settings_group.add(&rich_text_row);
    let read_receipts_row = adw::SwitchRow::builder()
        .title("Send read receipts automatically")
        .subtitle("Reply with a read receipt when a message asks for one; when off, the reading pane asks first")
        .build();
    mail_settings_group.add(&read_receipts_row);
    let mail_notifications_row = adw::SwitchRow::builder()
        .title("Mail notifications")
        .subtitle("Show a desktop notification when new mail arrives or a send fails")
        .active(true)
        .build();
    mail_settings_group.add(&mail_notifications_row);

    // The real "Calendar" group, replacing that section's placeholder: the
    // event-alerts toggle, wired by the caller into the `Gio::Notification`
    // reminder scheduling (enabled by default, matching the schema).
    let calendar_settings_group = adw::PreferencesGroup::builder().title("Calendar").build();
    let calendar_alerts_row = adw::SwitchRow::builder()
        .title("Event alerts")
        .subtitle("Show a notification when an event reminder comes due")
        .active(true)
        .build();
    calendar_settings_group.add(&calendar_alerts_row);

    // The real "Google Tasks" group (in the "Apps" section's place): the
    // OAuth client id the Tasks integration needs - Google has no public
    // client id, so the user's own registered id is configured here.
    let google_tasks_group = adw::PreferencesGroup::builder().title("Google Tasks").build();
    let google_tasks_client_row = adw::ActionRow::builder()
        .title("OAuth client id")
        .subtitle("Needed to connect Google Tasks - register a Desktop OAuth client in Google Cloud")
        .activatable(true)
        .build();
    google_tasks_group.add(&google_tasks_client_row);

    // The real "Keyboard shortcuts" group, replacing that section's
    // placeholder: an expander row (collapsed by default - there are
    // nineteen bindings) holding one row per built-in action, each showing
    // its current accelerator and starting the capture flow when activated.
    // The labels are seeded from the live shortcut table by the caller
    // (which also owns the capture controller); the defaults shown here are
    // only a stopgap for a not-yet-wired session.
    let keyboard_expander = adw::ExpanderRow::builder()
        .title("Keyboard shortcuts")
        .subtitle("Global key combinations - click a binding to record a new one")
        .build();
    let keyboard_group = adw::PreferencesGroup::new();
    keyboard_group.add(&keyboard_expander);
    let mut keyboard_rows = Vec::new();
    for d in crate::shortcuts::DEFAULT_SHORTCUTS {
        let label = gtk::Label::builder().css_classes(["dim-label"]).halign(gtk::Align::End).build();
        label.set_label(&crate::shortcuts::format_accel(d.modifiers, d.keyval));
        let row = adw::ActionRow::builder().title(d.title).activatable(true).build();
        row.add_suffix(&label);
        row.set_activatable_widget(Some(&row));
        keyboard_expander.add_row(&row);
        keyboard_rows.push(ShortcutRow { row, label, action: d.action });
    }
    let reset_shortcuts_row = adw::ActionRow::builder()
        .title("Restore default shortcuts")
        .subtitle("Put every shortcut back to its built-in binding")
        .activatable(true)
        .build();
    keyboard_expander.add_row(&reset_shortcuts_row);

    // The real "General" group, replacing that section's placeholder: the
    // login-autostart and close-to-background switches behind background
    // running, wired by the caller into `background.rs` and the main
    // window's close-request handling. "Start Lookout at login" is seeded
    // from the persisted setting at startup; the portal-managed half of it
    // can also be controlled from the desktop's own app settings.
    let general_group = adw::PreferencesGroup::builder().title("General").build();
    let start_at_login_row = adw::SwitchRow::builder()
        .title("Start Lookout at login")
        .subtitle("Run in the background from login so mail and calendar notifications keep working (also manageable in the desktop's app settings)")
        .build();
    general_group.add(&start_at_login_row);
    let close_to_background_row = adw::SwitchRow::builder()
        .title("Keep running when the window is closed")
        .subtitle("Continue syncing and notifying in the background; quit from the File menu")
        .active(true)
        .build();
    general_group.add(&close_to_background_row);

    // The "Vertical spacing" choice lives on the Appearance page rather than
    // a separate Layout section - it's about how the panes look. Spacing
    // between items in the folder and message list panes.
    let spacing_model = gtk::StringList::new(&["Tight", "Medium", "Loose"]);
    let spacing_row = adw::ComboRow::builder()
        .title("Vertical spacing")
        .subtitle("Spacing between items in the folder and message list panes")
        .model(&spacing_model)
        .build();
    appearance_group.add(&spacing_row);
    let privacy_group = placeholder_group("Privacy");

    let advanced_group = adw::PreferencesGroup::builder().title("Advanced").build();

    let mail_cache_group = adw::PreferencesGroup::builder().title("Mail cache").build();
    advanced_group.add(&mail_cache_group);

    let calendar_cache_group = adw::PreferencesGroup::builder().title("Calendar cache").build();
    advanced_group.add(&calendar_cache_group);

    let contacts_cache_group = adw::PreferencesGroup::builder().title("Contacts cache").build();
    advanced_group.add(&contacts_cache_group);

    let clear_cache_row = adw::ActionRow::builder()
        .title("Clear all caches")
        .subtitle("Delete locally-stored email, calendar and contacts data")
        .activatable(true)
        .build();
    advanced_group.add(&clear_cache_row);

    // -- assemble the sidebar (section list) + content (stack of per-section
    // pages) layout --

    let section_stack = gtk::Stack::new();
    section_stack.set_vexpand(true);
    section_stack.set_hexpand(true);

    let mut sections: Vec<(&'static str, &'static str)> = Vec::new();
    add_section(&section_stack, &mut sections, "Accounts", "accounts", &[&accounts_group, &mail_group, &goa_group]);
    add_section(&section_stack, &mut sections, "Calendar accounts", "calendar-accounts", &[&calendar_group]);
    add_section(&section_stack, &mut sections, "Webcal subscriptions", "webcal", &[&webcal_group]);
    add_section(&section_stack, &mut sections, "Appearance", "appearance", &[&appearance_group]);
    add_section(&section_stack, &mut sections, "General", "general", &[&general_group, &keyboard_group]);
    add_section(&section_stack, &mut sections, "Mail", "mail", &[&mail_settings_group]);
    add_section(&section_stack, &mut sections, "Calendar", "calendar", &[&calendar_settings_group]);
    add_section(&section_stack, &mut sections, "Privacy", "privacy", &[&privacy_group]);
    add_section(&section_stack, &mut sections, "Apps", "apps", &[&google_tasks_group]);
    add_section(&section_stack, &mut sections, "Advanced", "advanced", &[&advanced_group]);

    let section_list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::Single)
        .css_classes(["navigation-sidebar"])
        .build();
    for (label, _) in &sections {
        let row_label = gtk::Label::builder().label(*label).xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).build();
        row_label.set_margin_start(12);
        row_label.set_margin_end(8);
        row_label.set_margin_top(8);
        row_label.set_margin_bottom(8);
        let row = gtk::ListBoxRow::new();
        row.set_child(Some(&row_label));
        section_list.append(&row);
    }

    let section_names: Rc<Vec<&'static str>> = Rc::new(sections.iter().map(|(_, name)| *name).collect());
    let stack_for_selection = section_stack.clone();
    section_list.connect_row_selected(move |_, row| {
        let Some(row) = row else { return };
        let Ok(index) = usize::try_from(row.index()) else { return };
        if let Some(name) = section_names.get(index) {
            stack_for_selection.set_visible_child_name(name);
        }
    });
    if let Some(first_row) = section_list.row_at_index(0) {
        section_list.select_row(Some(&first_row));
    }

    let section_scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&section_list)
        .build();

    let paned = gtk::Paned::builder()
        .orientation(gtk::Orientation::Horizontal)
        .start_child(&card_section(&section_scroller))
        .end_child(&card_section(&section_stack))
        .resize_start_child(false)
        .resize_end_child(true)
        .shrink_start_child(false)
        .shrink_end_child(false)
        .position(200)
        .vexpand(true)
        .build();
    paned.add_css_class("seamless-paned");

    ConfigView {
        root: paned.clone().upcast(),
        paned,
        add_account_row,
        add_imap_row,
        goa_group,
        goa_rows: RefCell::new(Vec::new()),
        clear_cache_row,
        animations_row,
        theme_row,
        message_theme_dark_row,
        spacing_row,
        accent_switch_row,
        accent_color_row,
        accent_color_button,
        background_image_row,
        background_brightness_scale,
        restore_background_row,
        remote_images_row,
        trusted_senders_row,
        rich_text_row,
        read_receipts_row,
        mail_notifications_row,
        calendar_alerts_row,
        google_tasks_client_row,
        keyboard_rows: RefCell::new(keyboard_rows),
        reset_shortcuts_row,
        start_at_login_row,
        close_to_background_row,
        mail_group,
        calendar_group,
        webcal_group,
        mail_rows: RefCell::new(Vec::new()),
        calendar_rows: RefCell::new(Vec::new()),
        webcal_rows: RefCell::new(Vec::new()),
        identity_rows: RefCell::new(Vec::new()),
        mail_cache_group,
        calendar_cache_group,
        contacts_cache_group,
        mail_cache_rows: RefCell::new(Vec::new()),
        calendar_cache_rows: RefCell::new(Vec::new()),
        contacts_cache_rows: RefCell::new(Vec::new()),
    }
}

/// Rebuilds the two account groups and cache-info groups from the caller's
/// latest state. Clears any rows added by a previous refresh first, and shows
/// a dim placeholder row per group while it has no entries. Each mail account
/// gets an "Identities" row whose activation invokes `manage_identities`
/// with the row's widget (as anchor) and the account's id - the caller owns
/// the actual dialog. Manual ("other") accounts additionally get Edit and
/// Remove suffix buttons wired to `edit_other`/`remove_other`.
#[allow(clippy::too_many_arguments)]
pub fn refresh(
    view: &ConfigView,
    goa: &[GoaAccountInfo],
    mail: &[MailAccountInfo],
    calendar: &[CalendarAccountInfo],
    webcal: &[WebcalSubscriptionInfo],
    mail_cache_dir: &std::path::Path,
    mail_cache_files: &[CacheFile],
    calendar_cache_dir: &std::path::Path,
    calendar_cache_files: &[CacheFile],
    contacts_cache_dir: &std::path::Path,
    contacts_cache_files: &[CacheFile],
    manage_identities: &ManageIdentities,
    edit_other: &OtherAccountAction,
    remove_other: &OtherAccountAction,
    toggle_goa: &AccountToggle,
) {
    for row in view.goa_rows.borrow_mut().drain(..) {
        view.goa_group.remove(&row);
    }
    if goa.is_empty() {
        let row = adw::SwitchRow::builder().title("No GNOME Online Accounts accounts").sensitive(false).build();
        view.goa_group.add(&row);
        view.goa_rows.borrow_mut().push(row);
    } else {
        for info in goa {
            let row = adw::SwitchRow::builder()
                .title(glib::markup_escape_text(&info.display_name))
                .subtitle(glib::markup_escape_text(&info.email))
                .active(info.enabled)
                .build();
            // Wired per refresh rather than once at build: `active` is set
            // above before the callback attaches, so a rebuild can't fire
            // the handler for the rows it just created.
            let toggle_goa = toggle_goa.clone();
            let account_id = info.account_id.clone();
            row.connect_active_notify(move |row| toggle_goa(&account_id, row.is_active()));
            view.goa_group.add(&row);
            view.goa_rows.borrow_mut().push(row);
        }
    }

    for row in view.mail_rows.borrow_mut().drain(..) {
        view.mail_group.remove(&row);
    }
    for row in view.identity_rows.borrow_mut().drain(..) {
        view.mail_group.remove(&row);
    }
    for row in view.calendar_rows.borrow_mut().drain(..) {
        view.calendar_group.remove(&row);
    }
    for row in view.webcal_rows.borrow_mut().drain(..) {
        view.webcal_group.remove(&row);
    }

    if mail.is_empty() {
        push_row(&view.mail_group, &view.mail_rows, empty_row("No mail accounts connected"));
    } else {
        for info in mail {
            let subtitle = format!("{} · IMAP {} · SMTP {}", info.email, info.imap, info.smtp);
            let row = account_row(&info.display_name, &subtitle);
            if info.is_other {
                let edit_button = gtk::Button::from_icon_name("document-edit-symbolic");
                edit_button.set_tooltip_text(Some("Edit account"));
                edit_button.add_css_class("flat");
                let remove_button = gtk::Button::from_icon_name("user-trash-symbolic");
                remove_button.set_tooltip_text(Some("Remove account"));
                remove_button.add_css_class("flat");
                let account_id = info.account_id.clone();
                {
                    let edit_other = edit_other.clone();
                    let row_widget = row.clone();
                    let account_id = account_id.clone();
                    edit_button.connect_clicked(move |_| edit_other(row_widget.upcast_ref::<gtk::Widget>(), &account_id));
                }
                {
                    let remove_other = remove_other.clone();
                    let row_widget = row.clone();
                    let account_id = account_id.clone();
                    remove_button.connect_clicked(move |_| remove_other(row_widget.upcast_ref::<gtk::Widget>(), &account_id));
                }
                row.add_suffix(&edit_button);
                row.add_suffix(&remove_button);
            }
            push_row(&view.mail_group, &view.mail_rows, row);
            let identities_subtitle = if info.identity_labels.is_empty() {
                "Send as this account's own address".to_string()
            } else {
                glib::markup_escape_text(&info.identity_labels.join(" · ")).to_string()
            };
            let identity_row = adw::ActionRow::builder()
                .title("Sending identities")
                .subtitle(&identities_subtitle)
                .activatable(true)
                .build();
            let account_id = info.account_id.clone();
            let manage = manage_identities.clone();
            identity_row.connect_activated(move |row| manage(row.upcast_ref::<gtk::Widget>(), &account_id));
            push_row(&view.mail_group, &view.identity_rows, identity_row);
        }
    }

    if calendar.is_empty() {
        push_row(&view.calendar_group, &view.calendar_rows, empty_row("No calendar accounts connected"));
    } else {
        for info in calendar {
            push_row(&view.calendar_group, &view.calendar_rows, account_row(&info.display_name, &info.uri));
        }
    }

    // Feed subscriptions are managed from the calendar sidebar's "Add
    // calendar" dialog; this group is a read-only mirror (rows are not
    // activatable, matching the account rows above).
    if webcal.is_empty() {
        push_row(&view.webcal_group, &view.webcal_rows, empty_row("No webcal subscriptions"));
    } else {
        for info in webcal {
            push_row(&view.webcal_group, &view.webcal_rows, account_row(&info.display_name, &info.url));
        }
    }

    // -- mail cache --
    for row in view.mail_cache_rows.borrow_mut().drain(..) {
        view.mail_cache_group.remove(&row);
    }
    view.mail_cache_group.set_title(&format!("Mail cache — {}", mail_cache_dir.display()));
    if mail_cache_files.is_empty() {
        push_row(&view.mail_cache_group, &view.mail_cache_rows, empty_row("No cached files"));
    } else {
        for file in mail_cache_files {
            push_row(&view.mail_cache_group, &view.mail_cache_rows, cache_file_row(&file.name, &file.size));
        }
    }

    // -- calendar cache --
    for row in view.calendar_cache_rows.borrow_mut().drain(..) {
        view.calendar_cache_group.remove(&row);
    }
    view.calendar_cache_group.set_title(&format!("Calendar cache — {}", calendar_cache_dir.display()));
    if calendar_cache_files.is_empty() {
        push_row(&view.calendar_cache_group, &view.calendar_cache_rows, empty_row("No cached files"));
    } else {
        for file in calendar_cache_files {
            push_row(&view.calendar_cache_group, &view.calendar_cache_rows, cache_file_row(&file.name, &file.size));
        }
    }

    // -- contacts cache --
    for row in view.contacts_cache_rows.borrow_mut().drain(..) {
        view.contacts_cache_group.remove(&row);
    }
    view.contacts_cache_group.set_title(&format!("Contacts cache — {}", contacts_cache_dir.display()));
    if contacts_cache_files.is_empty() {
        push_row(&view.contacts_cache_group, &view.contacts_cache_rows, empty_row("No cached files"));
    } else {
        for file in contacts_cache_files {
            push_row(&view.contacts_cache_group, &view.contacts_cache_rows, cache_file_row(&file.name, &file.size));
        }
    }
}

fn account_row(title: &str, subtitle: &str) -> adw::ActionRow {
    adw::ActionRow::builder()
        .title(glib::markup_escape_text(title))
        .subtitle(glib::markup_escape_text(subtitle))
        .build()
}

fn empty_row(title: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(title).build();
    row.set_sensitive(false);
    row
}

fn push_row(group: &adw::PreferencesGroup, rows: &RefCell<Vec<adw::ActionRow>>, row: adw::ActionRow) {
    group.add(&row);
    rows.borrow_mut().push(row);
}

fn cache_file_row(name: &str, size: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(glib::markup_escape_text(name))
        .subtitle(glib::markup_escape_text(size))
        .build();
    row.set_sensitive(false);
    row
}

pub fn format_size(bytes: u64) -> String {
    match bytes {
        0 => "0 B".into(),
        b if b < 1024 => format!("{b} B"),
        b if b < 1024 * 1024 => format!("{:.1} KB", b as f64 / 1024.0),
        b if b < 1024 * 1024 * 1024 => format!("{:.1} MB", b as f64 / (1024.0 * 1024.0)),
        b => format!("{:.1} GB", b as f64 / (1024.0 * 1024.0 * 1024.0)),
    }
}

/// Refreshes the Config → Google Tasks client-id row's subtitle to reflect
/// the currently-configured id (or its absence). Called at build time and
/// after a save.
pub fn refresh_google_tasks_client_row(row: &adw::ActionRow) {
    let id = crate::google_tasks::configured_client_id();
    let subtitle = if id.is_empty() {
        "Not configured - needed to connect Google Tasks; register a Desktop OAuth client in Google Cloud".to_string()
    } else {
        format!("Configured: {id}")
    };
    row.set_subtitle(&subtitle);
}
