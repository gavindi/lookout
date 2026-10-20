//! Config view: a read-only overview of the Mail/Calendar accounts Lookout
//! is connected to, a live Appearance section (the smooth-transitions
//! preference, the background-wallpaper switch, the window-background image
//! picker and its dimming slider), live Mail and Calendar
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
use lookout_core::AccountId;

use crate::util::card_section;

/// The caller's hook for opening the manage-identities dialog: takes the
/// anchoring widget and the account id (opaque string).
pub type ManageIdentities = Rc<dyn Fn(&gtk::Widget, &str)>;

/// The spellcheck language picker's choices (Config → Mail): WebKit language
/// tags in parallel with their display labels. The empty first entry means
/// "follow the system locale" and is stored as an empty `mail-spell-check-
/// languages` list.
pub const SPELL_LANGUAGE_TAGS: &[&str] = &[
    "", "en_US", "en_GB", "fr_FR", "de_DE", "es_ES", "it_IT", "pt_BR", "nl_NL", "ru_RU", "pl_PL", "tr_TR", "cs_CZ", "ja_JP", "zh_CN",
];
pub const SPELL_LANGUAGE_LABELS: &[&str] = &[
    "System language",
    "English (United States)",
    "English (United Kingdom)",
    "French",
    "German",
    "Spanish",
    "Italian",
    "Portuguese (Brazil)",
    "Dutch",
    "Russian",
    "Polish",
    "Turkish",
    "Czech",
    "Japanese",
    "Chinese (Simplified)",
];

/// The caller's hook for one signature row's actions: takes the anchoring
/// widget and the signature's id. Used for both editing (opens the signature
/// editor prefilled) and removing - the caller distinguishes the two by
/// which closure it registers.
pub type SignaturesAction = Rc<dyn Fn(&gtk::Widget, &uuid::Uuid)>;

/// The caller's hook for one template row's actions (item 2.10 / C-17): takes
/// the anchoring widget and the template's id. Used for both editing (opens
/// the template editor prefilled) and removing - the caller distinguishes the
/// two by which closure it registers.
pub type TemplatesAction = Rc<dyn Fn(&gtk::Widget, &uuid::Uuid)>;

/// Which default-signature slot a dropdown writes (see `refresh`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SignatureDefault {
    /// "Default for new messages".
    New,
    /// "Default for replies & forwards".
    Reply,
}

/// The caller's hook for a per-account default-signature dropdown's
/// selection: the account id (opaque string), the dropdown's slot, and the
/// chosen signature's id (`None` = no signature). The caller owns the
/// persistence.
pub type SetAccountSignatureDefault = Rc<dyn Fn(&str, SignatureDefault, Option<uuid::Uuid>)>;

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

/// Whether a mail account can mirror Lookout pins to Outlook's own
/// MAPI-property pin via Microsoft Graph, and if so, whether it's turned on.
/// `\Flagged`-driven pinning itself is unaffected by any of these states -
/// this only governs the "Sync pins with Outlook" row/button.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum GraphPinStatus {
    /// Not a Microsoft 365 account - no row shown.
    NotApplicable,
    /// Already consented; pins mirror to Outlook automatically.
    Enabled,
    /// A Microsoft 365 account that hasn't opted in yet.
    NeedsConsent,
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
    pub graph_pin_status: GraphPinStatus,
}

/// Plain display data for one calendar account.
pub struct CalendarAccountInfo {
    pub display_name: String,
    /// The account's connection label: its CalDAV base URL, or "Microsoft 365"
    /// for a Graph-backed account.
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

/// Shows the given section page in the settings screen, syncing the
/// sidebar's selection to it - e.g. the composer's "Manage Signatures…"
/// jumps to the Accounts section where the signature list lives.
pub fn show_section(view: &ConfigView, name: &str) {
    view.section_stack.set_visible_child_name(name);
    if let Some(index) = view.section_names.iter().position(|n| *n == name) {
        if let Some(row) = view.section_list.row_at_index(index as i32) {
            view.section_list.select_row(Some(&row));
        }
    }
}

/// The config screen's widget tree. `root` goes into the Settings dialog
/// (a modal window opened from the header bar's Settings button);
/// the account groups are kept here so `refresh` can repopulate them in
/// place without rebuilding the whole page.
pub struct ConfigView {
    pub root: gtk::Widget,
    /// The sidebar/content split itself, exposed so the caller can wire its
    /// `position` to the same persist-width-as-a-percentage mechanism used
    /// by the Mail/Calendar/Contacts panes.
    pub paned: gtk::Paned,
    /// The section stack and sidebar list (and the section names in order),
    /// kept so `show_section` can jump to a named section - e.g. the
    /// composer's "Manage Signatures…" lands on the Accounts section.
    section_stack: gtk::Stack,
    section_list: gtk::ListBox,
    section_names: Rc<Vec<&'static str>>,
    /// "Add account…" entry row, exposed so the caller can wire its
    /// `activated` signal to the same GOA-settings invocation the empty-state
    /// page uses.
    pub add_account_row: adw::ActionRow,
    /// "Add IMAP/SMTP account…" entry row - the entry point for the app's
    /// own manually-managed mail accounts (see `other_accounts.rs`), wired
    /// by the caller to the add-account dialog.
    pub add_imap_row: adw::ActionRow,
    /// The "Add shared mailbox…" row (item 5.6 / M-42): opens the shared
    /// mailbox manager.
    pub add_shared_mailbox_row: adw::ActionRow,
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
    /// "Reading pane" dropdown (Config → Appearance): Right / Bottom / Off.
    /// Exposed so the caller can seed it from the `layout-reading-pane`
    /// boolean and `layout-reading-pane-position` key, and apply a change
    /// through the same path as the View ribbon's dropdown.
    pub reading_pane_row: adw::ComboRow,
    /// "Ribbon style" dropdown (Config → Appearance): Simplified / Classic.
    /// Exposed so the caller can seed it from the `ribbon-style` key and
    /// apply a change through the ribbon's own layout switch.
    pub ribbon_style_row: adw::ComboRow,
    /// "Message preview" dropdown (Config → Appearance): Off / 1 / 2 / 3
    /// lines of body preview per message-list row.
    pub preview_lines_row: adw::ComboRow,
    /// "Show sender images" switch (Config → Appearance): whether message-list
    /// rows and the reading-pane header show a sender avatar.
    pub show_sender_images_row: adw::SwitchRow,
    /// "Show email grouped by conversation" switch (Config → Appearance),
    /// mirroring View ▸ Messages ▸ "Show as conversations" (`mail-threaded`).
    pub conversations_row: adw::SwitchRow,
    /// "Conversation reading pane" dropdown (Config → Appearance): newest
    /// message on top or bottom (`mail-conversation-order`, item 1.7).
    pub conversation_order_row: adw::ComboRow,
    /// "Include messages from Sent Items" switch (Config → Appearance):
    /// whether the conversation view shows your replies from Sent Items.
    pub conversation_include_sent_row: adw::SwitchRow,
    /// "Custom accent color" switch (Config → Appearance), exposed so the
    /// caller can arm the accent picker row and map its state to the
    /// `accent-color` GSettings key (off = follow the system accent).
    pub accent_switch_row: adw::SwitchRow,
    /// "Accent color" picker row, exposed so the caller can wire its colour
    /// button into the `ThemeManager`; disabled until the switch above is on.
    pub accent_color_row: adw::ActionRow,
    pub accent_color_button: gtk::ColorDialogButton,
    /// "Background wallpaper" switch, exposed so the caller can wire its
    /// `active` state into showing/hiding the window background image
    /// entirely and gating the sensitivity of the three rows below it - none
    /// of them mean anything without a wallpaper to apply to.
    pub wallpaper_enabled_row: adw::SwitchRow,
    /// "Window background" row, exposed so the caller can wire its `activated`
    /// signal to a file chooser and update its subtitle to reflect the image
    /// currently in use.
    pub background_image_row: adw::ActionRow,
    /// The "Background dimming" row, exposed (alongside the scale below) so
    /// the caller can grey out the row - title included - when the
    /// "Background wallpaper" switch above is off.
    pub background_brightness_row: adw::ActionRow,
    /// The "Background dimming" slider, exposed so the caller can wire its
    /// `value-changed` signal into the window background's brightness and
    /// seed it with the stored value. Enabled whenever wallpapers are - the
    /// bundled artwork as well as a custom background - so beyond that gate
    /// the row itself needs no further caller-side wiring.
    pub background_brightness_scale: gtk::Scale,
    /// "Restore default background" row, exposed so the caller can wire its
    /// `activated` signal back to the bundled artwork (and re-enable it only
    /// when a custom image is actually set).
    pub restore_background_row: adw::ActionRow,
    /// "Load images from the web" switch (Config → Mail), exposed so the
    /// caller can wire its `active` state into the reading pane's remote-image
    /// policy.
    pub remote_images_row: adw::SwitchRow,
    /// "Safe senders…" row (Config → Mail → Junk email), exposed so the caller
    /// can wire its `activated` signal to the external-content trust manage
    /// dialog (and refresh the subtitle after a change). This is the same list
    /// the Junk filter treats as safe (item 2.9 / M-17).
    pub trusted_senders_row: adw::ActionRow,
    /// "Blocked senders…" row (Config → Mail → Junk email), exposed so the
    /// caller can wire its `activated` signal to the blocked-senders manage
    /// dialog (item 2.9 / M-17).
    pub blocked_senders_row: adw::ActionRow,
    /// "Ignored conversations…" row (Config → Mail → Junk email), exposed so
    /// the caller can open the ignored-conversations manage dialog (item 2.8 /
    /// M-20).
    pub ignored_conversations_row: adw::ActionRow,
    /// "Search Folders…" row (Config → Mail), exposed so the caller can open
    /// the saved-search manager (item 2.4 / M-19).
    pub search_folders_row: adw::ActionRow,
    /// "Categories…" row (Config → Mail), exposed so the caller can open the
    /// shared category master-list editor (item 2.1 / M-09).
    pub categories_row: adw::ActionRow,
    /// "Rules…" row (Config → Mail), exposed so the caller can open the rules
    /// manager (item 2.5 / M-14).
    pub rules_row: adw::ActionRow,
    /// "Quick Steps…" row (Config → Mail), exposed so the caller can open the
    /// Quick Steps manager (item 2.6 / M-15).
    pub quick_steps_row: adw::ActionRow,
    /// "Automatic replies…" row (Config → Mail), exposed so the caller can
    /// open the automatic-replies settings dialog (item 2.11 / M-24).
    pub automatic_replies_row: adw::ActionRow,
    /// "New template…" row (Config → Mail → My Templates), exposed so the
    /// caller can open the template editor (item 2.10 / C-17).
    pub new_template_row: adw::ActionRow,
    /// "Rich text" switch (Config → Mail), exposed so the caller can wire its
    /// `active` state into the composer's default body mode.
    pub rich_text_row: adw::SwitchRow,
    /// "Check spelling as you type" switch (Config → Mail), exposed so the
    /// caller can persist it and apply it to the composer's spellchecking.
    pub spell_check_row: adw::SwitchRow,
    /// "Spelling language" combo (Config → Mail); its selected index maps to
    /// `SPELL_LANGUAGE_TAGS`.
    pub spell_languages_row: adw::ComboRow,
    /// "Send read receipts automatically" switch (Config → Mail), exposed so
    /// the caller can wire its `active` state into the reading pane's
    /// read-receipt policy (read per message at display time).
    pub read_receipts_row: adw::SwitchRow,
    /// "Mark as read" combo (Config → Mail); its selected index maps to
    /// `MARK_AS_READ_MODES`.
    pub mark_as_read_row: adw::ComboRow,
    /// "After" spin (Config → Mail): seconds a message stays open before it is
    /// marked read; only enabled when "Mark as read" selects "after-delay".
    pub mark_as_read_delay_row: adw::SpinRow,
    /// "Undo send" combo (Config → Mail); its selected index maps to
    /// `UNDO_SEND_DELAYS`.
    pub undo_send_row: adw::ComboRow,
    /// "Warn if a message looks like it is missing an attachment" switch
    /// (Config → Mail), exposed so the caller can persist it and gate the
    /// pre-send attachment reminder (item 1.19).
    pub warn_no_attachment_row: adw::SwitchRow,
    /// "Mail notifications" switch (Config → Mail), exposed so the caller can
    /// wire its `active` state into the `Gio::Notification` new-mail/send-
    /// failure notifications.
    pub mail_notifications_row: adw::SwitchRow,
    /// "Dock badge" switch (Config → Mail), exposed so the caller can wire
    /// its `active` state into the Unity LauncherEntry dock badge (the
    /// Ubuntu dock's unread-count overlay).
    pub dock_badge_row: adw::SwitchRow,
    /// "Internal domains" entry (Config → Mail): a comma-separated list of
    /// domains the reading pane treats as internal (item 6.2 / S-06).
    pub internal_domains_entry: gtk::Entry,
    /// "Report phishing address" entry (Config → Mail): where Report phishing
    /// forwards a message as an attachment (item 6.7 / S-09).
    pub report_phishing_entry: gtk::Entry,
    /// "Event alerts" switch (Config → Calendar), exposed so the caller can
    /// wire its `active` state into the `Gio::Notification` reminder
    /// scheduling.
    pub calendar_alerts_row: adw::SwitchRow,
    /// "Week starts on" combo (Config → Calendar).
    pub calendar_week_start_row: adw::ComboRow,
    /// "Working days" preset combo (Config → Calendar).
    pub calendar_work_days_row: adw::ComboRow,
    /// "Working hours start" spin (Config → Calendar), local hour 0-23.
    pub calendar_work_start_row: adw::SpinRow,
    /// "Working hours end" spin (Config → Calendar), local hour 1-24.
    pub calendar_work_end_row: adw::SpinRow,
    /// "Time zone" entry (Config → Calendar): an IANA name, empty = system.
    pub calendar_primary_tz_row: adw::ComboRow,
    /// "Second time zone" picker (Config → Calendar): "None", then every
    /// IANA zone (see [`time_zone_at`]).
    pub calendar_secondary_tz_row: adw::ComboRow,
    /// "Default reminder" preset combo (Config → Calendar).
    pub calendar_default_reminder_row: adw::ComboRow,
    /// "File contacts as" combo (Config → People, P-07); its selected index
    /// maps to `CONTACT_FILE_AS`.
    pub contacts_file_as_row: adw::ComboRow,
    /// "Include desktop address books" switch (Config → People, EDS_TODO.md
    /// E1.4), bound to `EDS_ADDRESS_BOOKS`. The caller greys it out when the
    /// desktop has no Evolution Data Server Lookout can reach.
    pub eds_address_books_row: adw::SwitchRow,
    /// "OAuth client id" row (Config → Google Tasks), exposed so the caller
    /// can wire its `activated` signal to the client-id dialog (and update
    /// the subtitle after a save).
    pub google_tasks_client_row: adw::ActionRow,
    /// "API URL" entry (Config → Assistant): the row's child is a `gtk::Entry`
    /// (placeholders aren't bound on `adw::EntryRow`), exposed so the
    /// caller can seed it from GSettings and write edits back to the
    /// `assistant-api-url` key.
    pub assistant_url_entry: gtk::Entry,
    /// "API token" entry (Config → Assistant), exposed so the caller can
    /// seed it from the keyring and write edits back there (the token never
    /// touches dconf or settings.json).
    pub assistant_token_row: adw::PasswordEntryRow,
    /// "Test connection" row (Config → Assistant): its subtitle is the
    /// connection status line ("Not tested yet", "Testing…", then success
    /// or a failure message), updated by the caller after each run.
    pub assistant_test_row: adw::ActionRow,
    /// The "Test" button on the row above, exposed so the caller can wire
    /// its click and arm/disarm it with the URL's presence.
    pub assistant_test_button: gtk::Button,
    /// "Agent" dropdown (Config → Assistant), listing the API's available
    /// models (`/models` `data[].id`); its subtitle is the list's status
    /// line ("Refresh to list the API's agents", "Loading…", "N agents
    /// available", or a failure), updated by the caller after each load.
    pub assistant_agent_row: adw::ComboRow,
    /// The dropdown's model, exposed so the caller can repopulate it with
    /// the fetched agent list (and re-assert the stored selection).
    pub assistant_agent_model: gtk::StringList,
    /// The "Refresh" button on the Agent row, exposed so the caller can
    /// wire its click and arm/disarm it with the URL's presence.
    pub assistant_agent_refresh_button: gtk::Button,
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
    /// "Tray icon" switch (Config → General), exposed so the caller can
    /// start/stop the StatusNotifierItem (AppIndicator) tray service.
    pub tray_icon_row: adw::SwitchRow,
    /// "Aggressive prefetch" switch (Config → Advanced), exposed so the caller
    /// can wire it into the session's `PrefetchPolicy`: on runs batches on a
    /// short timer, warms more of each folder, and re-scans periodically. The
    /// rows below are disabled while it's off, so the cooperative default is
    /// always what an off switch means.
    pub aggressive_prefetch_row: adw::SwitchRow,
    /// "Batch interval" spin row (Config → Advanced), exposed so the caller
    /// can wire its value into the policy's `batch_interval`.
    pub prefetch_interval_row: adw::SpinRow,
    /// "Messages per folder" spin row (Config → Advanced), exposed so the
    /// caller can wire its value into the policy's `folder_limit`.
    pub prefetch_limit_row: adw::SpinRow,
    /// "Bodies per batch" spin row (Config → Advanced), exposed so the caller
    /// can wire its value into the policy's `batch_size`.
    pub prefetch_batch_row: adw::SpinRow,
    /// "Re-scan every" spin row (Config → Advanced), exposed so the caller
    /// can wire its value into the policy's `refresh_interval`.
    pub prefetch_refresh_row: adw::SpinRow,
    mail_group: adw::PreferencesGroup,
    calendar_group: adw::PreferencesGroup,
    webcal_group: adw::PreferencesGroup,
    mail_rows: RefCell<Vec<adw::ActionRow>>,
    calendar_rows: RefCell<Vec<adw::ActionRow>>,
    webcal_rows: RefCell<Vec<adw::ActionRow>>,
    /// The per-account "Identities" rows, rebuilt alongside `mail_rows`.
    identity_rows: RefCell<Vec<adw::ActionRow>>,
    /// "New Signature…" entry row (Config → Accounts → Signatures) - the
    /// entry point for the signature editor, wired by the caller.
    pub new_signature_row: adw::ActionRow,
    /// The list of saved signatures below the "New Signature…" button.
    signatures_group: adw::PreferencesGroup,
    /// The per-account "Default for new messages" / "Default for replies &
    /// forwards" dropdown rows in the Mail accounts section, rebuilt
    /// alongside `mail_rows`/`identity_rows` (one pair per account).
    signature_default_rows: RefCell<Vec<adw::ComboRow>>,
    /// The signature list rows rebuilt by `refresh_signatures`. The button
    /// row itself stays put; only these are cleared and re-added.
    signature_rows: RefCell<Vec<adw::ActionRow>>,
    /// The user-authored "My Templates" group (item 2.10 / C-17) and its list
    /// rows, rebuilt by `refresh_templates`; the "New template…" button row
    /// stays put.
    templates_group: adw::PreferencesGroup,
    templates_rows: RefCell<Vec<adw::ActionRow>>,
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
/// The "Include desktop address books" row's subtitle while the desktop's
/// address books are reachable (and before that's known).
pub const EDS_ADDRESS_BOOKS_SUBTITLE: &str = "Show people from GNOME Contacts and Evolution in People, read-only, and suggest them when choosing recipients";

/// Its subtitle once the check finds no Evolution Data Server Lookout can
/// reach - none installed or running, an unsupported version, or a sandbox
/// without the permission.
pub const EDS_ADDRESS_BOOKS_UNAVAILABLE_SUBTITLE: &str = "Not available - this desktop has no Evolution Data Server address books Lookout can reach";

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
    let add_shared_mailbox_row = adw::ActionRow::builder()
        .title("Add shared mailbox…")
        .subtitle("Another mailbox you have full access to on a Microsoft 365 account")
        .activatable(true)
        .build();
    accounts_group.add(&add_shared_mailbox_row);

    // The user-authored signature list: the "New Signature…" button opens
    // the rich-text signature editor; saved signatures appear as rows below
    // it, rebuilt by `refresh_signatures`. The per-account default-signature
    // dropdowns live in the Mail accounts section (`refresh`), one pair per
    // account, selecting from this global list.
    let signatures_group = adw::PreferencesGroup::builder().title("Signatures").build();
    let new_signature_row = adw::ActionRow::builder()
        .title("New Signature…")
        .subtitle("Create a rich-text signature for your messages")
        .activatable(true)
        .build();
    signatures_group.add(&new_signature_row);

    // The user-authored "My Templates" list (item 2.10 / C-17): the "New
    // template…" row opens the rich-text template editor; saved templates
    // appear below it, rebuilt by `refresh_templates`, and are inserted into
    // the composer from its "My Templates" menu.
    let templates_group = adw::PreferencesGroup::builder().title("My Templates").build();
    let new_template_row = adw::ActionRow::builder()
        .title("New template…")
        .subtitle("Create a reusable snippet to insert into messages")
        .activatable(true)
        .build();
    templates_group.add(&new_template_row);

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
    let wallpaper_enabled_row = adw::SwitchRow::builder()
        .title("Background wallpaper")
        .subtitle("Show a wallpaper behind the app content")
        .active(true)
        .build();
    appearance_group.add(&wallpaper_enabled_row);
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
    // The "Junk email" group (item 2.9 / M-17): the safe senders (the same
    // list the external-content trust flow uses, S-01) and the blocked
    // senders, mirroring Outlook's Mail → Junk email page.
    let junk_email_group = adw::PreferencesGroup::builder().title("Junk email").build();
    let trusted_senders_row = adw::ActionRow::builder()
        .title("Safe senders…")
        .subtitle("Senders whose mail is never treated as junk and may load remote content")
        .activatable(true)
        .build();
    junk_email_group.add(&trusted_senders_row);
    let blocked_senders_row = adw::ActionRow::builder()
        .title("Blocked senders…")
        .subtitle("Senders whose mail goes straight to Junk")
        .activatable(true)
        .build();
    junk_email_group.add(&blocked_senders_row);
    let ignored_conversations_row = adw::ActionRow::builder()
        .title("Ignored conversations…")
        .subtitle("Conversations whose new mail goes straight to Deleted Items")
        .activatable(true)
        .build();
    junk_email_group.add(&ignored_conversations_row);
    let categories_row = adw::ActionRow::builder()
        .title("Categories…")
        .subtitle("Manage the categories shared by mail, events and tasks")
        .activatable(true)
        .build();
    mail_settings_group.add(&categories_row);
    let search_folders_row = adw::ActionRow::builder()
        .title("Search Folders…")
        .subtitle("Saved searches shown as virtual folders in the folder pane")
        .activatable(true)
        .build();
    mail_settings_group.add(&search_folders_row);
    let rules_row = adw::ActionRow::builder()
        .title("Rules…")
        .subtitle("Organise incoming mail automatically")
        .activatable(true)
        .build();
    mail_settings_group.add(&rules_row);
    let quick_steps_row = adw::ActionRow::builder()
        .title("Quick Steps…")
        .subtitle("Chains of actions you run on the selected messages")
        .activatable(true)
        .build();
    mail_settings_group.add(&quick_steps_row);
    let automatic_replies_row = adw::ActionRow::builder()
        .title("Automatic replies…")
        .subtitle("Send out-of-office replies")
        .activatable(true)
        .build();
    mail_settings_group.add(&automatic_replies_row);
    let rich_text_row = adw::SwitchRow::builder()
        .title("Rich text")
        .subtitle("Start new messages in the formatted editor")
        .active(true)
        .build();
    mail_settings_group.add(&rich_text_row);
    let spell_check_row = adw::SwitchRow::builder()
        .title("Check spelling as you type")
        .subtitle("Underline misspelled words in the composer")
        .active(true)
        .build();
    mail_settings_group.add(&spell_check_row);
    let spell_languages_row = adw::ComboRow::builder()
        .title("Spelling language")
        .subtitle("Language to check spelling in")
        .model(&gtk::StringList::new(SPELL_LANGUAGE_LABELS))
        .build();
    mail_settings_group.add(&spell_languages_row);
    let read_receipts_row = adw::SwitchRow::builder()
        .title("Send read receipts automatically")
        .subtitle("Reply with a read receipt when a message asks for one; when off, the reading pane asks first")
        .build();
    mail_settings_group.add(&read_receipts_row);
    let mark_as_read_row = adw::ComboRow::builder()
        .title("Mark as read")
        .subtitle("When looking at a message marks it as read")
        .model(&gtk::StringList::new(&crate::settings::MARK_AS_READ_LABELS))
        .build();
    mail_settings_group.add(&mark_as_read_row);
    let mark_as_read_delay_row = adw::SpinRow::with_range(1.0, 60.0, 1.0);
    mark_as_read_delay_row.set_title("After");
    mark_as_read_delay_row.set_subtitle("Seconds a message stays open before it is marked read");
    mail_settings_group.add(&mark_as_read_delay_row);
    let undo_send_row = adw::ComboRow::builder()
        .title("Undo send")
        .subtitle("Delay sending so you can cancel")
        .model(&gtk::StringList::new(&crate::settings::UNDO_SEND_LABELS))
        .build();
    mail_settings_group.add(&undo_send_row);
    let warn_no_attachment_row = adw::SwitchRow::builder()
        .title("Warn if a message looks like it is missing an attachment")
        .subtitle("Ask before sending when the subject or body mentions an attachment but none is attached")
        .active(true)
        .build();
    mail_settings_group.add(&warn_no_attachment_row);
    let mail_notifications_row = adw::SwitchRow::builder()
        .title("Mail notifications")
        .subtitle("Show a desktop notification when new mail arrives or a send fails")
        .active(true)
        .build();
    mail_settings_group.add(&mail_notifications_row);
    let dock_badge_row = adw::SwitchRow::builder()
        .title("Dock badge")
        .subtitle("Show the total unread count on the app icon in the dock")
        .active(true)
        .build();
    mail_settings_group.add(&dock_badge_row);
    // Internal domains (item 6.2 / S-06): addresses outside this list are
    // tagged external in the reading pane. Comma-separated in one entry; the
    // caller splits it into the `mail-internal-domains` string list.
    let internal_domains_row = adw::ActionRow::builder()
        .title("Internal domains")
        .subtitle("Tag senders outside these domains as external")
        .build();
    let internal_domains_entry = gtk::Entry::builder().placeholder_text("example.com, example.org").build();
    internal_domains_row.set_child(Some(&internal_domains_entry));
    mail_settings_group.add(&internal_domains_row);
    // Report phishing address (item 6.7 / S-09): where Report phishing
    // forwards a message as an attachment before moving it to Junk.
    let report_phishing_row = adw::ActionRow::builder()
        .title("Report phishing address")
        .subtitle("Forward reported messages here as an attachment; empty hides the action")
        .build();
    let report_phishing_entry = gtk::Entry::builder().placeholder_text("phishing@example.com").build();
    report_phishing_row.set_child(Some(&report_phishing_entry));
    mail_settings_group.add(&report_phishing_row);

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

    let calendar_week_start_row = adw::ComboRow::builder()
        .title("Week starts on")
        .subtitle("First day of the week in the calendar")
        .model(&gtk::StringList::new(&crate::settings::WEEK_START_LABELS))
        .build();
    calendar_settings_group.add(&calendar_week_start_row);
    let work_day_labels: Vec<&str> = crate::settings::WORK_DAY_PRESETS.iter().map(|(label, _)| *label).collect();
    let calendar_work_days_row = adw::ComboRow::builder()
        .title("Working days")
        .subtitle("Days shown in the Work week view")
        .model(&gtk::StringList::new(&work_day_labels))
        .build();
    calendar_settings_group.add(&calendar_work_days_row);
    let calendar_work_start_row = adw::SpinRow::with_range(0.0, 23.0, 1.0);
    calendar_work_start_row.set_title("Working hours start");
    calendar_work_start_row.set_subtitle("Hour the working-hours band begins");
    calendar_settings_group.add(&calendar_work_start_row);
    let calendar_work_end_row = adw::SpinRow::with_range(1.0, 24.0, 1.0);
    calendar_work_end_row.set_title("Working hours end");
    calendar_work_end_row.set_subtitle("Hour the working-hours band ends");
    calendar_settings_group.add(&calendar_work_end_row);
    let calendar_primary_tz_row = time_zone_row("Time zone", false);
    calendar_settings_group.add(&calendar_primary_tz_row);
    let calendar_secondary_tz_row = time_zone_row("Second time zone", true);
    calendar_settings_group.add(&calendar_secondary_tz_row);
    let reminder_labels: Vec<&str> = crate::settings::REMINDER_CHOICES.iter().map(|(label, _)| *label).collect();
    let calendar_default_reminder_row = adw::ComboRow::builder()
        .title("Default reminder")
        .subtitle("Reminder applied to a new event")
        .model(&gtk::StringList::new(&reminder_labels))
        .build();
    calendar_settings_group.add(&calendar_default_reminder_row);

    // People (P-07): how a contact with no per-contact "File as" value is
    // listed and sorted in the People screen.
    let contacts_settings_group = adw::PreferencesGroup::builder().title("People").build();
    let contacts_file_as_row = adw::ComboRow::builder()
        .title("File contacts as")
        .subtitle("How a contact with no \"File as\" of its own is listed and sorted")
        .model(&gtk::StringList::new(&crate::settings::CONTACT_FILE_AS_LABELS))
        .build();
    contacts_settings_group.add(&contacts_file_as_row);
    let eds_address_books_row = adw::SwitchRow::builder()
        .title("Include desktop address books")
        .subtitle(EDS_ADDRESS_BOOKS_SUBTITLE)
        .build();
    contacts_settings_group.add(&eds_address_books_row);

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

    // The real "Assistant" group: the OpenAI-compatible endpoint the
    // assistant talks to. The URL is a plain GSettings string
    // (`assistant-api-url`); the token lives in the GNOME keyring, never in
    // dconf or settings.json (see `assistant.rs`). The "Test connection"
    // row's button runs the `GET {url}/models` probe; its subtitle is the
    // status line, updated by the caller after each run and armed/disarmed
    // with the URL's presence.
    let assistant_group = adw::PreferencesGroup::builder().title("Assistant").build();
    let assistant_url_row = adw::ActionRow::builder().title("API URL").build();
    let assistant_url_entry = gtk::Entry::builder().placeholder_text("https://api.openai.com/v1").build();
    assistant_url_row.set_child(Some(&assistant_url_entry));
    assistant_group.add(&assistant_url_row);
    let assistant_token_row = adw::PasswordEntryRow::builder().title("API token").build();
    assistant_group.add(&assistant_token_row);
    let assistant_test_row = adw::ActionRow::builder()
        .title("Test connection")
        .subtitle("Checks the URL and token against the server's /models endpoint")
        .build();
    let assistant_test_button = gtk::Button::with_label("Test");
    assistant_test_button.add_css_class("flat");
    assistant_test_button.set_sensitive(false);
    assistant_test_row.add_suffix(&assistant_test_button);
    assistant_group.add(&assistant_test_row);
    // The "Agent" dropdown lists the API's available models (the same
    // `/models` endpoint the Test button probes); the refresh button
    // repopulates it. Both start disabled - the caller arms them with the
    // URL's presence - and the row's subtitle is the list's status line.
    let assistant_agent_model = gtk::StringList::new(&[]);
    let assistant_agent_row = adw::ComboRow::builder()
        .title("Agent")
        .subtitle("Refresh to list the API's available agents")
        .model(&assistant_agent_model)
        .enable_search(true)
        .expression(gtk::PropertyExpression::new(gtk::StringObject::static_type(), gtk::Expression::NONE, "string"))
        .sensitive(false)
        .build();
    let assistant_agent_refresh_button = gtk::Button::from_icon_name("view-refresh-symbolic");
    assistant_agent_refresh_button.set_tooltip_text(Some("Reload the agent list"));
    assistant_agent_refresh_button.add_css_class("flat");
    assistant_agent_refresh_button.set_sensitive(false);
    assistant_agent_row.add_suffix(&assistant_agent_refresh_button);
    assistant_group.add(&assistant_agent_row);

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
        crate::privacy::mark_ui(&label);
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
    let tray_icon_row = adw::SwitchRow::builder()
        .title("Tray icon")
        .subtitle("Show an icon in the notification area with the unread mail count; clicking it shows or hides the window")
        .build();
    general_group.add(&tray_icon_row);

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
    // Position of the reading pane: Right (beside the list), Bottom (below it)
    // or Off. Mirrors the View ribbon's "Reading pane" dropdown.
    let reading_pane_model = gtk::StringList::new(&["Right", "Bottom", "Off"]);
    let reading_pane_row = adw::ComboRow::builder()
        .title("Reading pane")
        .subtitle("Where the reading pane sits, or Off to hide it")
        .model(&reading_pane_model)
        .build();
    appearance_group.add(&reading_pane_row);
    // The command ribbon's layout: Simplified (one row, the label beside the
    // icon) or Classic (the icon over the label, captioned groups). Mirrors
    // the ribbon's own "Ribbon layout" menu.
    let ribbon_style_row = adw::ComboRow::builder()
        .title("Ribbon style")
        .subtitle("How the command ribbon lays its actions out")
        .model(&gtk::StringList::new(&["Simplified", "Classic"]))
        .build();
    appearance_group.add(&ribbon_style_row);
    // How many body-preview lines each message-list row shows.
    let preview_lines_model = gtk::StringList::new(&["Off", "1 line", "2 lines", "3 lines"]);
    let preview_lines_row = adw::ComboRow::builder()
        .title("Message preview")
        .subtitle("Lines of message body to show on each row of the message list")
        .model(&preview_lines_model)
        .build();
    appearance_group.add(&preview_lines_row);
    // Whether sender avatars (contact photo, else coloured initials) appear on
    // message-list rows and in the reading-pane header.
    let show_sender_images_row = adw::SwitchRow::builder()
        .title("Show sender images")
        .subtitle("Show a contact photo, or coloured initials, for the sender of each message")
        .build();
    appearance_group.add(&show_sender_images_row);
    // Conversations (item 1.7): grouping in the list, plus the order and
    // reach of the stacked conversation in the reading pane. The first two
    // mirror the View ribbon's "Messages" menu.
    let conversations_row = adw::SwitchRow::builder()
        .title("Show email grouped by conversation")
        .subtitle("Group replies under one row in the message list and show the whole conversation in the reading pane")
        .build();
    appearance_group.add(&conversations_row);
    let conversation_order_row = adw::ComboRow::builder()
        .title("Conversation reading pane")
        .subtitle("Order of the messages when the reading pane shows a conversation")
        .model(&gtk::StringList::new(&crate::settings::CONVERSATION_ORDER_LABELS))
        .build();
    appearance_group.add(&conversation_order_row);
    // These pickers show app-defined choices (not account data), so their
    // selected values are chrome too.
    for row in [
        &theme_row,
        &spell_languages_row,
        &mark_as_read_row,
        &undo_send_row,
        &calendar_week_start_row,
        &calendar_work_days_row,
        &calendar_default_reminder_row,
        &contacts_file_as_row,
        &spacing_row,
        &reading_pane_row,
        &ribbon_style_row,
        &preview_lines_row,
        &conversation_order_row,
    ] {
        crate::privacy::mark_ui(row);
    }
    let conversation_include_sent_row = adw::SwitchRow::builder()
        .title("Include messages from Sent Items")
        .subtitle("Show your own replies among a conversation's messages")
        .build();
    appearance_group.add(&conversation_include_sent_row);
    conversations_row.bind_property("active", &conversation_order_row, "sensitive").sync_create().build();
    conversations_row.bind_property("active", &conversation_include_sent_row, "sensitive").sync_create().build();
    let privacy_group = placeholder_group("Privacy");

    let advanced_group = adw::PreferencesGroup::builder().title("Advanced").build();

    // Background body prefetch: the cooperative default (one pass, one small
    // batch per activity, newest 200 per folder) vs. the user-tuned
    // "aggressive" mode. The frequency/limit rows sit under a switch so an
    // off switch always means exactly the default behavior.
    let prefetch_group = adw::PreferencesGroup::builder().title("Mail prefetch").build();
    let aggressive_prefetch_row = adw::SwitchRow::builder()
        .title("Aggressive prefetch")
        .subtitle("Download message bodies more eagerly: batches run on a timer, more of each folder is warmed, and folders re-scan periodically. Uses more bandwidth")
        .build();
    prefetch_group.add(&aggressive_prefetch_row);
    let prefetch_interval_row = adw::SpinRow::with_range(5.0, 300.0, 5.0);
    prefetch_interval_row.set_value(30.0);
    prefetch_interval_row.set_title("Batch interval");
    prefetch_interval_row.set_subtitle("Seconds between prefetch batches");
    prefetch_group.add(&prefetch_interval_row);
    let prefetch_limit_row = adw::SpinRow::with_range(10.0, 5000.0, 10.0);
    prefetch_limit_row.set_value(200.0);
    prefetch_limit_row.set_title("Messages per folder");
    prefetch_limit_row.set_subtitle("How many of the newest messages each folder warms up");
    prefetch_group.add(&prefetch_limit_row);
    let prefetch_batch_row = adw::SpinRow::with_range(1.0, 100.0, 1.0);
    prefetch_batch_row.set_value(3.0);
    prefetch_batch_row.set_title("Bodies per batch");
    prefetch_batch_row.set_subtitle("Message bodies each prefetch round trip downloads");
    prefetch_group.add(&prefetch_batch_row);
    let prefetch_refresh_row = adw::SpinRow::with_range(5.0, 240.0, 5.0);
    prefetch_refresh_row.set_value(60.0);
    prefetch_refresh_row.set_title("Re-scan every");
    prefetch_refresh_row.set_subtitle("Minutes between full prefetch re-scans of every folder");
    prefetch_group.add(&prefetch_refresh_row);
    // The frequency/limit rows are meaningless while the switch is off -
    // "off" always means the cooperative default - so they grey out with it.
    for row in [&prefetch_interval_row, &prefetch_limit_row, &prefetch_batch_row, &prefetch_refresh_row] {
        row.set_sensitive(false);
    }
    advanced_group.add(&prefetch_group);

    // The privacy blur has no Settings row: it is driven entirely by the
    // title bar's cycling toggle (see `window.rs`), which is one click away
    // and mirrors the state as an icon.

    let mail_cache_group = adw::PreferencesGroup::builder().title("Mail cache").build();
    advanced_group.add(&mail_cache_group);

    let calendar_cache_group = adw::PreferencesGroup::builder().title("Calendar cache").build();
    advanced_group.add(&calendar_cache_group);

    let contacts_cache_group = adw::PreferencesGroup::builder().title("Contacts cache").build();
    advanced_group.add(&contacts_cache_group);
    // Their headings gain the cache path during refresh, so they are content.
    crate::privacy::mark_content_heading(&mail_cache_group);
    crate::privacy::mark_content_heading(&calendar_cache_group);
    crate::privacy::mark_content_heading(&contacts_cache_group);

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
    add_section(
        &section_stack,
        &mut sections,
        "Accounts",
        "accounts",
        &[&accounts_group, &signatures_group, &mail_group, &goa_group],
    );
    add_section(&section_stack, &mut sections, "Calendar accounts", "calendar-accounts", &[&calendar_group]);
    add_section(&section_stack, &mut sections, "Webcal subscriptions", "webcal", &[&webcal_group]);
    add_section(&section_stack, &mut sections, "Appearance", "appearance", &[&appearance_group]);
    add_section(&section_stack, &mut sections, "General", "general", &[&general_group, &keyboard_group]);
    add_section(&section_stack, &mut sections, "Mail", "mail", &[&junk_email_group, &templates_group, &mail_settings_group]);
    add_section(&section_stack, &mut sections, "Calendar", "calendar", &[&calendar_settings_group]);
    add_section(&section_stack, &mut sections, "People", "people", &[&contacts_settings_group]);
    add_section(&section_stack, &mut sections, "Privacy", "privacy", &[&privacy_group]);
    add_section(&section_stack, &mut sections, "Apps", "apps", &[&google_tasks_group]);
    add_section(&section_stack, &mut sections, "Assistant", "assistant", &[&assistant_group]);
    add_section(&section_stack, &mut sections, "Advanced", "advanced", &[&advanced_group]);

    let section_list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::Single)
        .css_classes(["navigation-sidebar"])
        .build();
    for (label, _) in &sections {
        let row_label = gtk::Label::builder().label(*label).xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).build();
        crate::privacy::mark_ui(&row_label);
        row_label.set_margin_start(12);
        row_label.set_margin_end(8);
        row_label.set_margin_top(8);
        row_label.set_margin_bottom(8);
        let row = gtk::ListBoxRow::new();
        row.set_child(Some(&row_label));
        section_list.append(&row);
    }

    let section_names: Rc<Vec<&'static str>> = Rc::new(sections.iter().map(|(_, name)| *name).collect());
    let section_names_for_stack = section_names.clone();
    let stack_for_selection = section_stack.clone();
    section_list.connect_row_selected(move |_, row| {
        let Some(row) = row else { return };
        let Ok(index) = usize::try_from(row.index()) else { return };
        if let Some(name) = section_names_for_stack.get(index) {
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
        section_stack,
        section_list,
        section_names,
        add_account_row,
        add_imap_row,
        add_shared_mailbox_row,
        goa_group,
        goa_rows: RefCell::new(Vec::new()),
        new_signature_row,
        new_template_row,
        signatures_group,
        signature_default_rows: RefCell::new(Vec::new()),
        signature_rows: RefCell::new(Vec::new()),
        templates_group,
        templates_rows: RefCell::new(Vec::new()),
        clear_cache_row,
        animations_row,
        theme_row,
        message_theme_dark_row,
        spacing_row,
        reading_pane_row,
        ribbon_style_row,
        preview_lines_row,
        show_sender_images_row,
        conversations_row,
        conversation_order_row,
        conversation_include_sent_row,
        accent_switch_row,
        accent_color_row,
        accent_color_button,
        wallpaper_enabled_row,
        background_image_row,
        background_brightness_row,
        background_brightness_scale,
        restore_background_row,
        remote_images_row,
        trusted_senders_row,
        blocked_senders_row,
        ignored_conversations_row,
        categories_row,
        search_folders_row,
        rules_row,
        quick_steps_row,
        automatic_replies_row,
        rich_text_row,
        spell_check_row,
        spell_languages_row,
        read_receipts_row,
        mark_as_read_row,
        mark_as_read_delay_row,
        undo_send_row,
        warn_no_attachment_row,
        mail_notifications_row,
        dock_badge_row,
        internal_domains_entry,
        report_phishing_entry,
        calendar_alerts_row,
        calendar_week_start_row,
        calendar_work_days_row,
        calendar_work_start_row,
        calendar_work_end_row,
        calendar_primary_tz_row,
        calendar_secondary_tz_row,
        calendar_default_reminder_row,
        contacts_file_as_row,
        eds_address_books_row,
        google_tasks_client_row,
        assistant_url_entry,
        assistant_token_row,
        assistant_test_row,
        assistant_test_button,
        assistant_agent_row,
        assistant_agent_model,
        assistant_agent_refresh_button,
        keyboard_rows: RefCell::new(keyboard_rows),
        reset_shortcuts_row,
        start_at_login_row,
        close_to_background_row,
        tray_icon_row,
        aggressive_prefetch_row,
        prefetch_interval_row,
        prefetch_limit_row,
        prefetch_batch_row,
        prefetch_refresh_row,
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
/// Remove suffix buttons wired to `edit_other`/`remove_other`; a Microsoft
/// 365 account with `graph_pin_status == NeedsConsent` gets a "Sync pins
/// with Outlook" suffix button wired to `enable_graph_pin`.
#[allow(clippy::too_many_arguments)]
pub fn refresh(
    view: &ConfigView,
    goa: &[GoaAccountInfo],
    goa_unavailable: bool,
    mail: &[MailAccountInfo],
    calendar: &[CalendarAccountInfo],
    webcal: &[WebcalSubscriptionInfo],
    mail_cache_dir: &std::path::Path,
    mail_cache_files: &[CacheFile],
    calendar_cache_dir: &std::path::Path,
    calendar_cache_files: &[CacheFile],
    contacts_cache_dir: &std::path::Path,
    contacts_cache_files: &[CacheFile],
    signatures: &[lookout_core::Signature],
    signature_defaults: &std::collections::HashMap<AccountId, crate::app_config::AccountSignatureDefaults>,
    templates: &[lookout_core::MailTemplate],
    manage_identities: &ManageIdentities,
    set_default_signature: &SetAccountSignatureDefault,
    edit_signature: &SignaturesAction,
    remove_signature: &SignaturesAction,
    edit_template: &TemplatesAction,
    remove_template: &TemplatesAction,
    edit_other: &OtherAccountAction,
    remove_other: &OtherAccountAction,
    enable_graph_pin: &OtherAccountAction,
    toggle_goa: &AccountToggle,
) {
    // No GOA daemon on the session bus at all (the normal case on KDE) -
    // hide the section rather than showing it permanently empty with a
    // "No GNOME Online Accounts accounts" placeholder that can never fill in.
    view.goa_group.set_visible(!goa_unavailable);
    for row in view.goa_rows.borrow_mut().drain(..) {
        view.goa_group.remove(&row);
    }
    if goa_unavailable {
        // Nothing to populate - the group above is hidden anyway.
    } else if goa.is_empty() {
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
            // Account name + address: blurred by privacy mode.
            crate::privacy::mark_row(&row);
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
    for row in view.signature_default_rows.borrow_mut().drain(..) {
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
            // Account name + address: blurred by privacy mode.
            crate::privacy::mark_row(&row);
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
            if info.graph_pin_status == GraphPinStatus::NeedsConsent {
                let sync_button = gtk::Button::with_label("Sync pins and flags with Outlook");
                sync_button.add_css_class("flat");
                let enable_graph_pin = enable_graph_pin.clone();
                let row_widget = row.clone();
                let account_id = info.account_id.clone();
                sync_button.connect_clicked(move |_| enable_graph_pin(row_widget.upcast_ref::<gtk::Widget>(), &account_id));
                row.add_suffix(&sync_button);
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
            // The subtitle is this account's sending addresses.
            crate::privacy::mark_row(&identity_row);
            let account_id = info.account_id.clone();
            let manage = manage_identities.clone();
            identity_row.connect_activated(move |row| manage(row.upcast_ref::<gtk::Widget>(), &account_id));
            push_row(&view.mail_group, &view.identity_rows, identity_row);

            // This account's default signatures, chosen from the global
            // signature list (Config → Accounts → Signatures). Rebuilt per
            // refresh like the rows above, so the options and stored
            // selection always match the current signature list.
            let account_defaults = signature_defaults.get(&AccountId(info.account_id.clone())).cloned().unwrap_or_default();
            let new_default = signature_default_dropdown(
                "Default for new messages",
                "Use this signature when composing a new message from this account",
                &info.account_id,
                account_defaults.new,
                signatures,
                SignatureDefault::New,
                set_default_signature,
            );
            view.mail_group.add(&new_default);
            view.signature_default_rows.borrow_mut().push(new_default);
            let reply_default = signature_default_dropdown(
                "Default for replies &amp; forwards",
                "Use this signature when replying to or forwarding from this account",
                &info.account_id,
                account_defaults.reply,
                signatures,
                SignatureDefault::Reply,
                set_default_signature,
            );
            view.mail_group.add(&reply_default);
            view.signature_default_rows.borrow_mut().push(reply_default);
        }
    }

    if calendar.is_empty() {
        push_row(&view.calendar_group, &view.calendar_rows, empty_row("No calendar accounts connected"));
    } else {
        for info in calendar {
            let row = account_row(&info.display_name, &info.uri);
            crate::privacy::mark_row(&row);
            push_row(&view.calendar_group, &view.calendar_rows, row);
        }
    }

    // Feed subscriptions are managed from the calendar sidebar's "Add
    // calendar" dialog; this group is a read-only mirror (rows are not
    // activatable, matching the account rows above).
    if webcal.is_empty() {
        push_row(&view.webcal_group, &view.webcal_rows, empty_row("No webcal subscriptions"));
    } else {
        for info in webcal {
            let row = account_row(&info.display_name, &info.url);
            crate::privacy::mark_row(&row);
            push_row(&view.webcal_group, &view.webcal_rows, row);
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

    // -- signatures --
    refresh_signatures(view, signatures, edit_signature, remove_signature);

    // -- My Templates --
    refresh_templates(view, templates, edit_template, remove_template);
}

fn account_row(title: &str, subtitle: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(glib::markup_escape_text(title))
        .subtitle(glib::markup_escape_text(subtitle))
        .build();
    // Account/signature/template names and details are content, not chrome.
    // Sensitive rows are additionally tagged at their call sites.
    crate::privacy::mark_content(&row);
    row
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
    // Cached file names/sizes are content, not chrome.
    crate::privacy::mark_content(&row);
    row.set_sensitive(false);
    row
}

/// The row subtitle for a signature: its plain-text first line, truncated
/// to a preview width.
fn signature_preview(text: &str) -> String {
    let first_line = text.lines().next().unwrap_or("").trim();
    if first_line.is_empty() {
        "Empty signature".to_string()
    } else if first_line.chars().count() > 80 {
        let mut preview: String = first_line.chars().take(80).collect();
        preview.push('…');
        preview
    } else {
        first_line.to_string()
    }
}

/// Rebuilds the signature rows below the "New Signature…" button (Config →
/// Accounts → Signatures). The button row itself stays put; only these rows
/// are cleared and re-added, so the button keeps its place above the list.
/// Each row's Edit/Delete suffix buttons invoke the caller's `edit`/`remove`
/// hooks (which own the actual editor dialog and persistence).
pub fn refresh_signatures(view: &ConfigView, signatures: &[lookout_core::Signature], edit: &SignaturesAction, remove: &SignaturesAction) {
    for row in view.signature_rows.borrow_mut().drain(..) {
        view.signatures_group.remove(&row);
    }
    if signatures.is_empty() {
        push_row(&view.signatures_group, &view.signature_rows, empty_row("No signatures yet"));
        return;
    }
    for signature in signatures {
        let row = account_row(&signature.name, &signature_preview(&signature.text));
        let edit_button = gtk::Button::from_icon_name("document-edit-symbolic");
        edit_button.set_tooltip_text(Some("Edit signature"));
        edit_button.add_css_class("flat");
        let remove_button = gtk::Button::from_icon_name("user-trash-symbolic");
        remove_button.set_tooltip_text(Some("Delete signature"));
        remove_button.add_css_class("flat");
        let id = signature.id;
        {
            let edit = edit.clone();
            let row_widget = row.clone();
            edit_button.connect_clicked(move |_| edit(row_widget.upcast_ref::<gtk::Widget>(), &id));
        }
        {
            let remove = remove.clone();
            let row_widget = row.clone();
            remove_button.connect_clicked(move |_| remove(row_widget.upcast_ref::<gtk::Widget>(), &id));
        }
        row.add_suffix(&edit_button);
        row.add_suffix(&remove_button);
        push_row(&view.signatures_group, &view.signature_rows, row);
    }
}

/// Rebuilds the template rows below the "New template…" button (Config →
/// Mail → My Templates, item 2.10 / C-17). The button row itself stays put;
/// only these rows are cleared and re-added. Each row's Edit/Delete suffix
/// buttons invoke the caller's `edit`/`remove` hooks (which own the actual
/// editor dialog and persistence).
pub fn refresh_templates(view: &ConfigView, templates: &[lookout_core::MailTemplate], edit: &TemplatesAction, remove: &TemplatesAction) {
    for row in view.templates_rows.borrow_mut().drain(..) {
        view.templates_group.remove(&row);
    }
    if templates.is_empty() {
        push_row(&view.templates_group, &view.templates_rows, empty_row("No templates yet"));
        return;
    }
    for template in templates {
        let row = account_row(&template.name, &signature_preview(&template.text));
        let edit_button = gtk::Button::from_icon_name("document-edit-symbolic");
        edit_button.set_tooltip_text(Some("Edit template"));
        edit_button.add_css_class("flat");
        let remove_button = gtk::Button::from_icon_name("user-trash-symbolic");
        remove_button.set_tooltip_text(Some("Delete template"));
        remove_button.add_css_class("flat");
        let id = template.id;
        {
            let edit = edit.clone();
            let row_widget = row.clone();
            edit_button.connect_clicked(move |_| edit(row_widget.upcast_ref::<gtk::Widget>(), &id));
        }
        {
            let remove = remove.clone();
            let row_widget = row.clone();
            remove_button.connect_clicked(move |_| remove(row_widget.upcast_ref::<gtk::Widget>(), &id));
        }
        row.add_suffix(&edit_button);
        row.add_suffix(&remove_button);
        push_row(&view.templates_group, &view.templates_rows, row);
    }
}

/// One per-account default-signature dropdown: title/subtitle for the slot,
/// options from the global signature list ("None" + one per signature), the
/// account's stored selection seeded, and the selection's write-through
/// wired to the caller's `set_default` hook. A stored default whose
/// signature no longer exists (a hand-edited file) is healed to "None"
/// through the same hook.
fn signature_default_dropdown(
    title: &str,
    subtitle: &str,
    account_id: &str,
    default: Option<uuid::Uuid>,
    signatures: &[lookout_core::Signature],
    kind: SignatureDefault,
    set_default: &SetAccountSignatureDefault,
) -> adw::ComboRow {
    let mut names: Vec<String> = vec!["None".to_string()];
    names.extend(signatures.iter().map(|s| s.name.clone()));
    let mut ids: Vec<Option<uuid::Uuid>> = vec![None];
    ids.extend(signatures.iter().map(|s| Some(s.id)));
    let name_refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
    let selected_index = ids.iter().position(|id| *id == default).unwrap_or(0) as u32;

    let row = adw::ComboRow::builder().title(title).subtitle(subtitle).build();
    let model = gtk::StringList::new(&name_refs);
    row.set_model(Some(&model));
    row.set_selected(selected_index);
    if default.is_some() && !ids.contains(&default) {
        set_default(account_id, kind, None);
    }
    let ids_for_row = ids.clone();
    let set_default = set_default.clone();
    let account_id = account_id.to_string();
    row.connect_selected_notify(move |row| {
        let id = ids_for_row.get(row.selected() as usize).copied().flatten();
        set_default(&account_id, kind, id);
    });
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

/// Every IANA zone name the time-zone pickers list, sorted (item 3.1) - the
/// shared list lives in `calendar_tz` now that the event editor's per-event
/// picker (CAL-11) uses it too.
fn time_zone_names() -> &'static [&'static str] {
    crate::calendar_tz::iana_zone_names()
}

/// The stored value for a time-zone picker's `index`: "" for its first entry
/// ("System time zone" / "None"), else the IANA name.
pub fn time_zone_at(index: u32) -> String {
    match index {
        0 => String::new(),
        i => time_zone_names().get(i as usize - 1).map(|name| name.to_string()).unwrap_or_default(),
    }
}

/// The picker index showing a stored zone: its entry, or the first entry for
/// a blank or unknown value (which then stores back as "" on the next change).
pub fn time_zone_index(stored: &str) -> u32 {
    let Some(tz) = crate::settings::parse_time_zone(stored) else { return 0 };
    time_zone_names().iter().position(|name| *name == tz.name()).map_or(0, |i| i as u32 + 1)
}

/// The row subtitle for a picked zone: its current abbreviation and offset
/// ("AEDT · UTC+11"), or what the first entry means.
fn time_zone_subtitle(index: u32, secondary: bool) -> String {
    let now = chrono::Utc::now();
    let zone = match crate::settings::parse_time_zone(&time_zone_at(index)) {
        Some(tz) => crate::calendar_tz::DisplayZone::Named(tz),
        None if secondary => return "Show a second time zone beside the hours in the Day and Week views".to_string(),
        None => crate::calendar_tz::DisplayZone::System,
    };
    let offset = crate::calendar_tz::offset_label(zone.offset_seconds(now));
    let label = zone.label(now);
    if label == offset {
        offset
    } else {
        format!("{label} · {offset}")
    }
}

/// A searchable time-zone picker row: "System time zone (<name>)" (primary)
/// or "None" (second zone) first, then every IANA zone. Its subtitle follows
/// the selection.
fn time_zone_row(title: &str, secondary: bool) -> adw::ComboRow {
    let first = if secondary {
        "None".to_string()
    } else {
        match crate::calendar_tz::system_zone_name() {
            Some(name) => format!("System time zone ({name})"),
            None => "System time zone".to_string(),
        }
    };
    let model = gtk::StringList::new(&[first.as_str()]);
    for name in time_zone_names() {
        model.append(name);
    }
    let row = adw::ComboRow::builder()
        .title(title)
        .model(&model)
        .enable_search(true)
        .expression(gtk::PropertyExpression::new(gtk::StringObject::static_type(), None::<gtk::Expression>, "string"))
        .subtitle(time_zone_subtitle(0, secondary))
        .build();
    row.connect_selected_notify(move |row| row.set_subtitle(&time_zone_subtitle(row.selected(), secondary)));
    // Zone names are app chrome, not account content.
    crate::privacy::mark_ui(&row);
    row
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The time-zone pickers store "" for their first entry and the IANA
    /// name otherwise, and a stored value maps back to its entry - an
    /// unknown or blank one to the first.
    #[test]
    fn time_zone_picker_round_trips_stored_values() {
        let index = time_zone_index("Asia/Kolkata");
        assert!(index > 0);
        assert_eq!(time_zone_at(index), "Asia/Kolkata");
        assert_eq!(time_zone_index(""), 0);
        assert_eq!(time_zone_index("Mars/Olympus_Mons"), 0);
        assert_eq!(time_zone_at(0), "");
        assert_eq!(time_zone_at(u32::MAX), "", "out of range reads as unset");
    }

    fn card_section(content: &gtk::Widget) -> gtk::Box {
        let card = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .css_classes(["card"])
            .overflow(gtk::Overflow::Hidden)
            .build();
        card.append(content);
        card
    }

    /// The Settings window's close/reopen contract from window.rs, guarded
    /// without mapping real windows (GTK may only be initialized on one
    /// thread, and libtest gives each test its own - the repo's other
    /// GTK-touching tests race to `gtk::init()` the same way). Closing the
    /// settings dialog must leave the shared `config_view.root` detached so
    /// the next open can re-attach it: the window's close handler unparents
    /// it explicitly because reference cycles into the window's content
    /// chain (the close button's handler holds the dialog, the dialog holds
    /// the chain) can otherwise leave the root parented to a destroyed
    /// window, and the reopen's attach then fails - the empty modal bug.
    /// Ignored by default: it needs a real display, and GTK is not
    /// thread-safe while libtest spreads tests across threads, so it only
    /// runs on demand - `cargo test -p lookout-app -- --ignored
    /// --test-threads=1 config_view::tests`. Same self-skipping convention
    /// as the window.rs GTK tests when run among them.
    #[ignore = "needs a display and a single test thread; run with --ignored --test-threads=1"]
    #[test]
    fn settings_window_reopen_reparents_the_config_root() {
        // Same convention as the window.rs GTK tests: libtest gives each
        // test its own thread, GTK may only be initialized on one - if
        // another test thread got there first, skip rather than panic. The
        // shared `gtk_test::gtk_ready` lock also serializes the init itself.
        // Headless environments (CI) can't initialize GTK at all; the
        // lifecycle is exercised where a display exists.
        if !crate::gtk_test::gtk_ready() {
            eprintln!("skipping: no display available, or GTK initialized on another test thread");
            return;
        }
        let config_view = Rc::new(build());

        // First open: the root attaches to the window's content chain.
        let card = card_section(&config_view.root);
        assert!(config_view.root.parent().is_some(), "root must attach to the window's chain");

        // Close: the window.rs close handler unparents the root explicitly
        // before the window is destroyed.
        config_view.root.unparent();
        assert!(config_view.root.parent().is_none(), "close-path unparent must detach the root");

        // Second open: the defensive unparent is a no-op, and the root
        // re-attaches to the fresh window's chain.
        config_view.root.unparent();
        let card2 = card_section(&config_view.root);
        assert!(config_view.root.parent().is_some(), "root must re-attach to the second window");
        let _ = (card, card2);
    }

    /// `AdwPreferencesRow::title`/`subtitle` are rendered as Pango markup, so
    /// a literal "&" (as "Default for replies & forwards" used to be) fails
    /// to parse and GTK logs a "Gtk-WARNING **: Failed to set text ... from
    /// markup" instead of showing the row's title at all. Same
    /// display/single-thread convention as `settings_window_reopen_...`
    /// above.
    #[ignore = "needs a display and a single test thread; run with --ignored --test-threads=1"]
    #[test]
    fn signature_default_dropdown_title_survives_markup_parsing() {
        if !crate::gtk_test::gtk_ready() {
            eprintln!("skipping: no display available, or GTK initialized on another test thread");
            return;
        }
        let warnings: std::sync::Arc<std::sync::Mutex<Vec<String>>> = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let warnings_for_handler = warnings.clone();
        let handler_id = glib::log_set_handler(Some("Gtk"), glib::LogLevels::all(), false, false, move |_domain, _level, message| {
            warnings_for_handler.lock().unwrap().push(message.to_string());
        });

        let set_default: SetAccountSignatureDefault = Rc::new(|_, _, _| {});
        let row = signature_default_dropdown("Default for replies &amp; forwards", "subtitle", "acct", None, &[], SignatureDefault::Reply, &set_default);

        glib::log_remove_handler(Some("Gtk"), handler_id);
        assert_eq!(row.title(), "Default for replies & forwards", "the markup escape must render back to a plain ampersand");
        assert!(
            warnings.lock().unwrap().is_empty(),
            "setting the title must not trip a markup-parsing Gtk-WARNING: {:?}",
            warnings.lock().unwrap()
        );
    }
}
