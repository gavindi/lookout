//! Config view: a read-only overview of the Mail/Calendar accounts Lookout
//! is connected to, a live Appearance section (the smooth-transitions
//! preference and the window-background image picker), live Mail and Calendar
//! preference groups (the remote-images/rich-text toggles and the event-alerts
//! toggle), disabled placeholder
//! sections mirroring the rest of the Phase 5 settings taxonomy (General/Layout/Mail/Privacy/Apps) and a live
//! Advanced section with a cache-clear action. Data-in/widget-out like
//! `calendar_view.rs` and `folder_tree.rs`: the caller (window.rs) owns the
//! session state and feeds plain display structs into `refresh`.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;

/// The caller's hook for opening the manage-identities dialog: takes the
/// anchoring widget and the account id (opaque string).
pub type ManageIdentities = Rc<dyn Fn(&gtk::Widget, &str)>;

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

/// The config screen's widget tree. `root` goes into the main `root_stack`;
/// the account groups are kept here so `refresh` can repopulate them in
/// place without rebuilding the whole page.
pub struct ConfigView {
    pub root: gtk::Widget,
    /// "Add account…" entry row, exposed so the caller can wire its
    /// `activated` signal to the same GOA-settings invocation the empty-state
    /// page uses.
    pub add_account_row: adw::ActionRow,
    /// "Clear all caches…" row, exposed so the caller can wire its
    /// `activated` signal to the actual cache-clearing (the mail cache lives
    /// in the `lookout-mail` crate, out of this module's reach).
    pub clear_cache_row: adw::ActionRow,
    /// "Animate transitions" switch, exposed so the caller (which owns the
    /// widgets that animate) can wire its `active` state to disable the
    /// reading pane's crossfades.
    pub animations_row: adw::SwitchRow,
    /// "Window background" row, exposed so the caller can wire its `activated`
    /// signal to a file chooser and update its subtitle to reflect the image
    /// currently in use.
    pub background_image_row: adw::ActionRow,
    /// "Restore default background" row, exposed so the caller can wire its
    /// `activated` signal back to the bundled artwork (and re-enable it only
    /// when a custom image is actually set).
    pub restore_background_row: adw::ActionRow,
    /// "Load images from the web" switch (Config → Mail), exposed so the
    /// caller can wire its `active` state into the reading pane's remote-image
    /// policy.
    pub remote_images_row: adw::SwitchRow,
    /// "Rich text" switch (Config → Mail), exposed so the caller can wire its
    /// `active` state into the composer's default body mode.
    pub rich_text_row: adw::SwitchRow,
    /// "Event alerts" switch (Config → Calendar), exposed so the caller can
    /// wire its `active` state into the `Gio::Notification` reminder
    /// scheduling.
    pub calendar_alerts_row: adw::SwitchRow,
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

/// Phase 5 roadmap's settings taxonomy, each rendered as a disabled
/// placeholder group until that work lands - same honest-disabled convention
/// as the menu bar's Home/View ribbon tabs. "Appearance" is deliberately
/// absent: it's a real, enabled group with the smooth-transitions preference
/// (see `build`), as is "Advanced" below and the "Mail" and "Calendar" groups
/// (the remote-images/rich-text toggles and the event-alerts toggle) the
/// loop's `Mail` and `Calendar` entries hand off to.
const PLACEHOLDER_SECTIONS: [&str; 6] = ["General", "Layout", "Mail", "Calendar", "Privacy", "Apps"];

pub fn build() -> ConfigView {
    let page = adw::PreferencesPage::new();
    page.set_vexpand(true);

    let accounts_group = adw::PreferencesGroup::builder().title("Accounts").build();
    let add_account_row = adw::ActionRow::builder()
        .title("Add account…")
        .subtitle("Open GNOME Online Accounts settings")
        .activatable(true)
        .build();
    accounts_group.add(&add_account_row);
    page.add(&accounts_group);

    let mail_group = adw::PreferencesGroup::builder().title("Mail accounts").build();
    page.add(&mail_group);

    let calendar_group = adw::PreferencesGroup::builder().title("Calendar accounts").build();
    page.add(&calendar_group);

    let webcal_group = adw::PreferencesGroup::builder().title("Webcal subscriptions").build();
    page.add(&webcal_group);

    let appearance_group = adw::PreferencesGroup::builder().title("Appearance").build();
    let animations_row = adw::SwitchRow::builder()
        .title("Animate transitions")
        .subtitle("Fade between views when switching messages")
        .active(true)
        .build();
    appearance_group.add(&animations_row);
    let background_image_row = adw::ActionRow::builder()
        .title("Window background")
        .subtitle("Default Lookout artwork")
        .activatable(true)
        .build();
    appearance_group.add(&background_image_row);
    let restore_background_row = adw::ActionRow::builder()
        .title("Restore default background")
        .subtitle("Use the bundled artwork again")
        .activatable(true)
        .sensitive(false)
        .build();
    appearance_group.add(&restore_background_row);
    page.add(&appearance_group);

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
    let rich_text_row = adw::SwitchRow::builder()
        .title("Rich text")
        .subtitle("Start new messages in the formatted editor")
        .active(true)
        .build();
    mail_settings_group.add(&rich_text_row);

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

    for section in PLACEHOLDER_SECTIONS {
        // "Mail" is a live group (see `mail_settings_group` above), not a
        // placeholder - add it in this section's place so the taxonomy order
        // is preserved.
        if section == "Mail" {
            page.add(&mail_settings_group);
            continue;
        }
        // "Calendar" is likewise a live group (see `calendar_settings_group`
        // above), not a placeholder.
        if section == "Calendar" {
            page.add(&calendar_settings_group);
            continue;
        }
        let group = adw::PreferencesGroup::builder().title(section).build();
        let row = adw::ActionRow::builder().title("Not implemented yet").build();
        row.set_sensitive(false);
        group.add(&row);
        page.add(&group);
    }

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
    page.add(&advanced_group);

    ConfigView {
        root: page.upcast(),
        add_account_row,
        clear_cache_row,
        animations_row,
        background_image_row,
        restore_background_row,
        remote_images_row,
        rich_text_row,
        calendar_alerts_row,
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
/// the actual dialog.
#[allow(clippy::too_many_arguments)]
pub fn refresh(
    view: &ConfigView,
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
) {
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
            push_row(&view.mail_group, &view.mail_rows, account_row(&info.display_name, &subtitle));
            let identities_subtitle = if info.identity_labels.is_empty() {
                "Send as this account's own address".to_string()
            } else {
                info.identity_labels.join(" · ")
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
    adw::ActionRow::builder().title(title).subtitle(subtitle).build()
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
    let row = adw::ActionRow::builder().title(name).subtitle(size).build();
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
