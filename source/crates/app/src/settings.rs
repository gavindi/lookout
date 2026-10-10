//! GSettings-backed persistence for the app's scalar preferences.
//!
//! The Phase 5 goal is that every user preference survives restarts. The
//! scalars (layout toggles, sort key/direction, favorites, the Mail-section
//! switches, and the session-memory keys migrated from `last_view.rs` /
//! `background_image.rs`) live in a `gio::Settings` object for the
//! `io.github.gavindi.Lookout` schema. That schema is found either in the
//! `$OUT_DIR` bundle `build.rs` compiles (dev builds - the bundle always
//! matches the freshly-built binary, unlike a system install, which can
//! shadow it with a stale copy) or, for a packaged install (which has no
//! `$OUT_DIR` at runtime), system-installed in
//! `/usr/share/glib-2.0/schemas`.
//!
//! When neither is available - or in tests - [`resolve`] falls back to a
//! process-local in-memory map seeded with the schema's defaults, which is
//! exactly the session-only behaviour the app had before this module existed.
//! Nothing here ever fails hard, matching the best-effort philosophy of the
//! other config modules. The store is created once in `build_window` (GTK is
//! single-threaded, so a plain `Rc` held by `UiState` and the widgets'
//! closures is all the sharing that's needed).

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use gtk::gio;
use gtk::gio::prelude::{SettingsExt, SettingsExtManual};

/// The GSettings schema id - deliberately the application id, so a packaged
/// install and the dev-mode bundle resolve to the same settings.
pub const SCHEMA_ID: &str = "io.github.gavindi.Lookout";

pub const ANIMATE_TRANSITIONS: &str = "animate-transitions";
pub const START_AT_LOGIN: &str = "start-at-login";
pub const CLOSE_TO_BACKGROUND: &str = "close-to-background";
/// Config → Appearance → "Background wallpaper": whether the window shows a
/// wallpaper (bundled artwork or a user-picked image) behind the app
/// content at all. Off disables the image picker, dimming slider and
/// restore-default row below it too - none of them mean anything without a
/// wallpaper to apply to.
pub const BACKGROUND_WALLPAPER_ENABLED: &str = "background-wallpaper-enabled";
pub const WINDOW_BACKGROUND_PATH: &str = "window-background-path";
pub const BACKGROUND_BRIGHTNESS: &str = "background-brightness";
pub const THEME_ID: &str = "theme-id";
pub const ACCENT_COLOR: &str = "accent-color";
pub const LAYOUT_FOLDER_PANE: &str = "layout-folder-pane";
pub const LAYOUT_READING_PANE: &str = "layout-reading-pane";
pub const LAYOUT_CALENDAR_OVERVIEW: &str = "layout-calendar-overview";
/// Vertical spacing between items in the folder and message list panes: one
/// of [`SPACINGS`] ("medium" default, "tight", "loose"). Drives the
/// `spacing-*` CSS class on both panes via [`spacing_class`].
pub const LAYOUT_SPACING: &str = "layout-vertical-spacing";
pub const PANE_FOLDER_WIDTH_PCT: &str = "pane-folder-width-percent";
pub const PANE_MESSAGE_LIST_WIDTH_PCT: &str = "pane-message-list-width-percent";
pub const PANE_CALENDAR_SIDEBAR_WIDTH_PCT: &str = "pane-calendar-sidebar-width-percent";
pub const PANE_CONTACTS_SIDEBAR_WIDTH_PCT: &str = "pane-contacts-sidebar-width-percent";
pub const PANE_CONFIG_SIDEBAR_WIDTH_PCT: &str = "pane-config-sidebar-width-percent";
pub const PANE_MAIL_OVERVIEW_WIDTH_PCT: &str = "pane-mail-overview-width-percent";
/// Main window's last non-maximized width/height in pixels, and whether it
/// was maximized. Restored once at window construction; width/height are
/// only saved while un-maximized, since a maximized size isn't the size to
/// restore to.
pub const WINDOW_WIDTH: &str = "window-width";
pub const WINDOW_HEIGHT: &str = "window-height";
pub const WINDOW_MAXIMIZED: &str = "window-maximized";
pub const SORT_KEY: &str = "sort-key";
pub const SORT_DESCENDING: &str = "sort-descending";
pub const MAIL_FAVORITES: &str = "mail-favorites";
/// Set the first time a favourite is moved up or down (the folder pane's
/// "⋯" menu): from then on the Favorites section follows `mail-favorites`'
/// stored order instead of sorting alphabetically.
pub const MAIL_FAVORITES_CUSTOM_ORDER: &str = "mail-favorites-custom-order";
/// AccountIds (GOA object paths) of accounts the user disabled in Config →
/// Accounts; everything else is enabled by default.
pub const ACCOUNTS_DISABLED: &str = "accounts-disabled";
pub const MAIL_THREADED: &str = "mail-threaded";
pub const MAIL_LOAD_REMOTE_IMAGES: &str = "mail-load-remote-images";
pub const MAIL_SEND_READ_RECEIPTS: &str = "mail-send-read-receipts";
pub const MAIL_RICH_TEXT_DEFAULT: &str = "mail-rich-text-default";
/// Config → Appearance → "Dark message theme": whether newly-opened HTML
/// messages start with the reading pane's "Switch message theme" override
/// already on - background stripped, colours inverted - instead of in their
/// original light form. A default for new messages only; the per-email
/// toggle in the header still overrides the current message either way.
pub const MAIL_MESSAGE_THEME_DARK: &str = "mail-message-theme-dark";
pub const CALENDAR_ALERTS_ENABLED: &str = "calendar-alerts-enabled";
/// Calendar → "Week starts on": first day of the week, as 0=Sunday .. 6=Saturday.
pub const CALENDAR_WEEK_START: &str = "calendar-week-start";
/// Calendar → "Working days" (Work week view columns), day numbers 0..6.
pub const CALENDAR_WORK_DAYS: &str = "calendar-work-days";
/// Calendar → "Working hours start" (local hour, 0-23).
pub const CALENDAR_WORK_START: &str = "calendar-work-start";
/// Calendar → "Working hours end" (local hour, exclusive, 1-24).
pub const CALENDAR_WORK_END: &str = "calendar-work-end";
/// Calendar → "Primary time zone" (IANA name; empty = system local).
pub const CALENDAR_PRIMARY_TZ: &str = "calendar-primary-tz";
/// Calendar → "Second time zone" (IANA name; empty = none).
pub const CALENDAR_SECONDARY_TZ: &str = "calendar-secondary-tz";
/// Calendar → "Default reminder" (minutes before a new event; -1 = none).
pub const CALENDAR_DEFAULT_REMINDER: &str = "calendar-default-reminder";
pub const MAIL_NOTIFICATIONS_ENABLED: &str = "mail-notifications-enabled";
/// Config → Assistant → "API URL": the base URL of the OpenAI-compatible
/// API the assistant talks to. The API token itself is kept in the GNOME
/// keyring (`assistant.rs`), never in GSettings.
pub const ASSISTANT_API_URL: &str = "assistant-api-url";
/// Config → Assistant → "Agent": the model id chosen from the API's
/// `/models` list, for the assistant to talk to.
pub const ASSISTANT_AGENT: &str = "assistant-agent";
/// Config → Mail → "Dock badge": whether the app icon's Ubuntu-dock badge
/// (the Unity LauncherEntry count) shows the summed Inbox unread count.
pub const DOCK_BADGE_ENABLED: &str = "dock-badge-enabled";
/// Config → General → "Tray icon": whether the app shows a StatusNotifierItem
/// (AppIndicator) icon in the notification area with the unread count.
pub const TRAY_ICON_ENABLED: &str = "tray-icon-enabled";
pub const SHORTCUTS: &str = "shortcuts";
pub const LAST_VIEW_UNIFIED: &str = "last-view-unified";
pub const LAST_VIEW_MAILBOX: &str = "last-view-mailbox";
/// Config → Advanced → "Aggressive prefetch": whether the background body
/// prefetch runs eagerly (short batch timer, larger per-folder warm-up,
/// periodic full-pass re-scans) instead of one cooperative pass per session.
pub const PREFETCH_AGGRESSIVE: &str = "prefetch-aggressive";
/// Config → Advanced → "Aggressive prefetch" → "Batch interval": seconds
/// between prefetch batches while aggressive prefetch is enabled.
pub const PREFETCH_BATCH_INTERVAL_SECONDS: &str = "prefetch-batch-interval-seconds";
/// Config → Advanced → "Aggressive prefetch" → "Messages per folder": how
/// many of a folder's newest messages the prefetch warms up.
pub const PREFETCH_FOLDER_LIMIT: &str = "prefetch-folder-limit";
/// Config → Advanced → "Aggressive prefetch" → "Bodies per batch": message
/// bodies each prefetch round trip downloads.
pub const PREFETCH_BATCH_SIZE: &str = "prefetch-batch-size";
/// Config → Advanced → "Aggressive prefetch" → "Re-scan every": minutes
/// between full prefetch re-scans of every folder.
pub const PREFETCH_REFRESH_INTERVAL_MINUTES: &str = "prefetch-refresh-interval-minutes";

/// One stored value, mirroring the GSettings key types the app uses: booleans,
/// strings, doubles (pane-width percentages), integers (prefetch counts),
/// and string arrays (favorites).
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Value {
    Bool(bool),
    String(String),
    Double(f64),
    Int(i32),
    Strv(Vec<String>),
}

/// Typed access over the app's preferences, on top of one of two backends:
/// real `gio::Settings` (persisted by the GSettings backend, usually dconf)
/// or the in-memory fallback (session-only). Not thread-shared by design -
/// everything that touches it runs on the UI thread, like the rest of
/// `window.rs`'s `Rc<RefCell<_>>` state.
pub enum SettingsStore {
    Gio(Rc<gio::Settings>),
    Memory(RefCell<HashMap<&'static str, Value>>),
}

/// Resolves the backend: real GSettings when the schema is available,
/// session-only memory otherwise. In tests it's always memory, so tests are
/// deterministic and never touch dconf.
pub fn resolve() -> SettingsStore {
    if cfg!(test) {
        return SettingsStore::Memory(RefCell::new(defaults()));
    }
    if let Some(settings) = create_gio_settings() {
        SettingsStore::Gio(Rc::new(settings))
    } else {
        tracing::warn!("no GSettings schema for {SCHEMA_ID}; preferences are session-only");
        SettingsStore::Memory(RefCell::new(defaults()))
    }
}

/// Finds the schema for this build, preferring the `OUT_DIR` bundle
/// `build.rs` compiled. The bundle exists on dev machines - where the
/// freshly-built binary and its schema always match - while a
/// system-installed schema is often stale (the leftover of an earlier
/// packaged install), and a stale schema makes `gio::Settings` abort on
/// any key the running binary reads but the old schema lacks. A packaged
/// install has no `OUT_DIR` at runtime, so the lookup then falls through
/// to the system-installed schema in `/usr/share/glib-2.0/schemas`.
/// `gio::Settings::new` aborts on a missing schema, so it's only ever
/// called after a successful lookup here.
fn create_gio_settings() -> Option<gio::Settings> {
    let out_dir = std::path::PathBuf::from(env!("OUT_DIR"));
    if let Ok(source) = gio::SettingsSchemaSource::from_directory(&out_dir, None, false) {
        if let Some(schema) = source.lookup(SCHEMA_ID, true) {
            return Some(gio::Settings::new_full(&schema, None::<&gio::SettingsBackend>, None));
        }
    }
    if let Some(source) = gio::SettingsSchemaSource::default() {
        if source.lookup(SCHEMA_ID, true).is_some() {
            return Some(gio::Settings::new(SCHEMA_ID));
        }
    }
    None
}

/// The in-memory backend's initial state, mirroring the schema's defaults so
/// the session-only path behaves identically to a fresh GSettings store.
fn defaults() -> HashMap<&'static str, Value> {
    let mut map = HashMap::new();
    map.insert(ANIMATE_TRANSITIONS, Value::Bool(true));
    map.insert(START_AT_LOGIN, Value::Bool(false));
    map.insert(CLOSE_TO_BACKGROUND, Value::Bool(true));
    map.insert(BACKGROUND_WALLPAPER_ENABLED, Value::Bool(true));
    map.insert(WINDOW_BACKGROUND_PATH, Value::String(String::new()));
    map.insert(BACKGROUND_BRIGHTNESS, Value::Double(0.75));
    map.insert(THEME_ID, Value::String(crate::theme::DEFAULT_THEME.into()));
    map.insert(ACCENT_COLOR, Value::String(String::new()));
    map.insert(LAYOUT_FOLDER_PANE, Value::Bool(true));
    map.insert(LAYOUT_READING_PANE, Value::Bool(true));
    map.insert(LAYOUT_CALENDAR_OVERVIEW, Value::Bool(true));
    map.insert(LAYOUT_SPACING, Value::String("medium".into()));
    map.insert(PANE_FOLDER_WIDTH_PCT, Value::Double(-1.0));
    map.insert(PANE_MESSAGE_LIST_WIDTH_PCT, Value::Double(-1.0));
    map.insert(PANE_CALENDAR_SIDEBAR_WIDTH_PCT, Value::Double(-1.0));
    map.insert(PANE_CONTACTS_SIDEBAR_WIDTH_PCT, Value::Double(-1.0));
    map.insert(PANE_CONFIG_SIDEBAR_WIDTH_PCT, Value::Double(-1.0));
    map.insert(PANE_MAIL_OVERVIEW_WIDTH_PCT, Value::Double(-1.0));
    map.insert(WINDOW_WIDTH, Value::Int(1600));
    map.insert(WINDOW_HEIGHT, Value::Int(900));
    map.insert(WINDOW_MAXIMIZED, Value::Bool(false));
    map.insert(SORT_KEY, Value::String("date".into()));
    map.insert(SORT_DESCENDING, Value::Bool(true));
    map.insert(MAIL_FAVORITES, Value::Strv(Vec::new()));
    map.insert(MAIL_FAVORITES_CUSTOM_ORDER, Value::Bool(false));
    map.insert(ACCOUNTS_DISABLED, Value::Strv(Vec::new()));
    map.insert(MAIL_THREADED, Value::Bool(true));
    map.insert(MAIL_LOAD_REMOTE_IMAGES, Value::Bool(false));
    map.insert(MAIL_SEND_READ_RECEIPTS, Value::Bool(false));
    map.insert(MAIL_RICH_TEXT_DEFAULT, Value::Bool(true));
    map.insert(MAIL_MESSAGE_THEME_DARK, Value::Bool(false));
    map.insert(CALENDAR_ALERTS_ENABLED, Value::Bool(true));
    map.insert(CALENDAR_WEEK_START, Value::Int(0));
    map.insert(CALENDAR_WORK_DAYS, Value::Strv(vec!["1".into(), "2".into(), "3".into(), "4".into(), "5".into()]));
    map.insert(CALENDAR_WORK_START, Value::Int(8));
    map.insert(CALENDAR_WORK_END, Value::Int(18));
    map.insert(CALENDAR_PRIMARY_TZ, Value::String(String::new()));
    map.insert(CALENDAR_SECONDARY_TZ, Value::String(String::new()));
    map.insert(CALENDAR_DEFAULT_REMINDER, Value::Int(-1));
    map.insert(MAIL_NOTIFICATIONS_ENABLED, Value::Bool(true));
    map.insert(ASSISTANT_API_URL, Value::String(String::new()));
    map.insert(ASSISTANT_AGENT, Value::String(String::new()));
    map.insert(DOCK_BADGE_ENABLED, Value::Bool(true));
    map.insert(TRAY_ICON_ENABLED, Value::Bool(false));
    map.insert(SHORTCUTS, Value::Strv(Vec::new()));
    map.insert(LAST_VIEW_UNIFIED, Value::Bool(false));
    map.insert(LAST_VIEW_MAILBOX, Value::String(String::new()));
    map.insert(PREFETCH_AGGRESSIVE, Value::Bool(false));
    map.insert(PREFETCH_BATCH_INTERVAL_SECONDS, Value::Int(30));
    map.insert(PREFETCH_FOLDER_LIMIT, Value::Int(200));
    map.insert(PREFETCH_BATCH_SIZE, Value::Int(1));
    map.insert(PREFETCH_REFRESH_INTERVAL_MINUTES, Value::Int(60));
    map
}

impl SettingsStore {
    pub fn get_bool(&self, key: &'static str) -> bool {
        match self {
            SettingsStore::Gio(settings) => settings.boolean(key),
            SettingsStore::Memory(map) => match map.borrow().get(key) {
                Some(Value::Bool(value)) => *value,
                _ => false,
            },
        }
    }

    pub fn set_bool(&self, key: &'static str, value: bool) {
        match self {
            SettingsStore::Gio(settings) => {
                if let Err(e) = settings.set_boolean(key, value) {
                    tracing::warn!(key, "could not write setting: {e}");
                }
            }
            SettingsStore::Memory(map) => {
                map.borrow_mut().insert(key, Value::Bool(value));
            }
        }
    }

    pub fn get_string(&self, key: &'static str) -> String {
        match self {
            SettingsStore::Gio(settings) => settings.string(key).to_string(),
            SettingsStore::Memory(map) => match map.borrow().get(key) {
                Some(Value::String(value)) => value.clone(),
                _ => String::new(),
            },
        }
    }

    pub fn set_string(&self, key: &'static str, value: &str) {
        match self {
            SettingsStore::Gio(settings) => {
                if let Err(e) = settings.set_string(key, value) {
                    tracing::warn!(key, "could not write setting: {e}");
                }
            }
            SettingsStore::Memory(map) => {
                map.borrow_mut().insert(key, Value::String(value.into()));
            }
        }
    }

    pub fn get_double(&self, key: &'static str) -> f64 {
        match self {
            SettingsStore::Gio(settings) => settings.double(key),
            SettingsStore::Memory(map) => match map.borrow().get(key) {
                Some(Value::Double(value)) => *value,
                _ => -1.0,
            },
        }
    }

    pub fn set_double(&self, key: &'static str, value: f64) {
        match self {
            SettingsStore::Gio(settings) => {
                if let Err(e) = settings.set_double(key, value) {
                    tracing::warn!(key, "could not write setting: {e}");
                }
            }
            SettingsStore::Memory(map) => {
                map.borrow_mut().insert(key, Value::Double(value));
            }
        }
    }

    pub fn get_int(&self, key: &'static str) -> i32 {
        match self {
            SettingsStore::Gio(settings) => settings.int(key),
            SettingsStore::Memory(map) => match map.borrow().get(key) {
                Some(Value::Int(value)) => *value,
                _ => 0,
            },
        }
    }

    pub fn set_int(&self, key: &'static str, value: i32) {
        match self {
            SettingsStore::Gio(settings) => {
                if let Err(e) = settings.set_int(key, value) {
                    tracing::warn!(key, "could not write setting: {e}");
                }
            }
            SettingsStore::Memory(map) => {
                map.borrow_mut().insert(key, Value::Int(value));
            }
        }
    }

    pub fn get_strv(&self, key: &'static str) -> Vec<String> {
        match self {
            SettingsStore::Gio(settings) => settings.strv(key).iter().map(|s| s.to_string()).collect(),
            SettingsStore::Memory(map) => match map.borrow().get(key) {
                Some(Value::Strv(values)) => values.clone(),
                _ => Vec::new(),
            },
        }
    }

    pub fn set_strv(&self, key: &'static str, values: Vec<String>) {
        match self {
            SettingsStore::Gio(settings) => {
                if let Err(e) = settings.set_strv(key, values) {
                    tracing::warn!(key, "could not write setting: {e}");
                }
            }
            SettingsStore::Memory(map) => {
                map.borrow_mut().insert(key, Value::Strv(values));
            }
        }
    }

    /// Assembles the session's background-prefetch policy from the Advanced
    /// settings. When aggressive prefetch is off, the policy is `Default` -
    /// the app's original cooperative one-pass behavior - so the
    /// frequency/limit values (greyed out in the UI while off) only ever
    /// shape an enabled aggressive pass.
    pub fn prefetch_policy(&self) -> lookout_mail::session::PrefetchPolicy {
        if !self.get_bool(PREFETCH_AGGRESSIVE) {
            return lookout_mail::session::PrefetchPolicy::default();
        }
        lookout_mail::session::PrefetchPolicy {
            aggressive: true,
            batch_interval: std::time::Duration::from_secs(self.get_int(PREFETCH_BATCH_INTERVAL_SECONDS).max(1) as u64),
            folder_limit: self.get_int(PREFETCH_FOLDER_LIMIT).max(1) as u32,
            batch_size: self.get_int(PREFETCH_BATCH_SIZE).max(1) as usize,
            refresh_interval: std::time::Duration::from_secs((self.get_int(PREFETCH_REFRESH_INTERVAL_MINUTES).max(1) as u64) * 60),
        }
    }

    /// Assembles the Calendar section's preferences from GSettings (item 0.6).
    /// Every value is validated here, so a hand-edited or stale dconf entry
    /// can't produce an empty Work week or an inverted business-hours band -
    /// the defaults stand in for anything out of range.
    pub fn calendar_preferences(&self) -> CalendarPreferences {
        let week_start = weekday_from_sun_index(i64::from(self.get_int(CALENDAR_WEEK_START))).unwrap_or(chrono::Weekday::Sun);

        let mut work_days: Vec<chrono::Weekday> = self
            .get_strv(CALENDAR_WORK_DAYS)
            .iter()
            .filter_map(|entry| entry.parse::<i64>().ok().and_then(weekday_from_sun_index))
            .collect();
        let mut seen = Vec::new();
        work_days.retain(|day| {
            if seen.contains(day) {
                false
            } else {
                seen.push(*day);
                true
            }
        });
        if work_days.is_empty() {
            work_days = CalendarPreferences::default().work_days;
        }

        let start = self.get_int(CALENDAR_WORK_START).clamp(0, 23) as u32;
        let end = self.get_int(CALENDAR_WORK_END).clamp(1, 24) as u32;
        let (work_start, work_end) = if end > start { (start, end) } else { (8, 18) };

        let time_zone = |key: &'static str| {
            let value = self.get_string(key);
            let value = value.trim().to_string();
            (!value.is_empty()).then_some(value)
        };
        let default_reminder_minutes = match self.get_int(CALENDAR_DEFAULT_REMINDER) {
            minutes if minutes >= 0 => Some(i64::from(minutes)),
            _ => None,
        };

        CalendarPreferences {
            week_start,
            work_days,
            work_start,
            work_end,
            primary_tz: time_zone(CALENDAR_PRIMARY_TZ),
            secondary_tz: time_zone(CALENDAR_SECONDARY_TZ),
            default_reminder_minutes,
        }
    }
}

/// The Calendar section's validated preferences (item 0.6). `calendar_view`
/// reads these when it builds the calendar panel; the second/primary time zones
/// and the default reminder are consumed by items 3.1/3.2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarPreferences {
    /// First day of the week for the grids and mini-calendar.
    pub week_start: chrono::Weekday,
    /// Days shown in the Work week view, in week order (never empty).
    pub work_days: Vec<chrono::Weekday>,
    /// Local hour the business-hours band starts (0-23).
    pub work_start: u32,
    /// Local hour the business-hours band ends, exclusive (1-24).
    pub work_end: u32,
    /// The user's home time zone (IANA), if set.
    pub primary_tz: Option<String>,
    /// An optional second grid time zone (IANA).
    pub secondary_tz: Option<String>,
    /// Default reminder lead time for a new event, in minutes; `None` = none.
    pub default_reminder_minutes: Option<i64>,
}

impl Default for CalendarPreferences {
    fn default() -> Self {
        CalendarPreferences {
            week_start: chrono::Weekday::Sun,
            work_days: vec![chrono::Weekday::Mon, chrono::Weekday::Tue, chrono::Weekday::Wed, chrono::Weekday::Thu, chrono::Weekday::Fri],
            work_start: 8,
            work_end: 18,
            primary_tz: None,
            secondary_tz: None,
            default_reminder_minutes: None,
        }
    }
}

/// The `Weekday` for the stored `0=Sunday .. 6=Saturday` scheme, or `None` for
/// an out-of-range value.
pub fn weekday_from_sun_index(index: i64) -> Option<chrono::Weekday> {
    Some(match index {
        0 => chrono::Weekday::Sun,
        1 => chrono::Weekday::Mon,
        2 => chrono::Weekday::Tue,
        3 => chrono::Weekday::Wed,
        4 => chrono::Weekday::Thu,
        5 => chrono::Weekday::Fri,
        6 => chrono::Weekday::Sat,
        _ => return None,
    })
}

/// The stored `0=Sunday .. 6=Saturday` number for a `Weekday`.
pub fn weekday_sun_index(day: chrono::Weekday) -> i64 {
    day.num_days_from_sunday() as i64
}

/// The `layout-vertical-spacing` setting's values, in UI (Config → Appearance
/// → "Vertical spacing" ComboRow) order. "medium" is the default (and what an
/// unset GSettings value resolves to); "tight" is the app's original, tightest
/// look, and "loose" is the roomiest.
pub const SPACINGS: [&str; 3] = ["tight", "medium", "loose"];

/// The CSS class each spacing value drives on the folder/message-list panes.
/// Unknown values fall back to the default's class ("spacing-medium").
pub fn spacing_class(name: &str) -> &'static str {
    match name {
        "tight" => "spacing-tight",
        "loose" => "spacing-loose",
        _ => "spacing-medium",
    }
}

/// Index of a stored spacing value (for the config `ComboRow`); unknown
/// values fall back to the default ("medium")'s index.
pub fn spacing_index(name: &str) -> u32 {
    let default = SPACINGS.iter().position(|s| *s == "medium").unwrap_or(1);
    SPACINGS.iter().position(|s| *s == name).unwrap_or(default) as u32
}

/// The spacing value at a [`SPACINGS`] index (from the config `ComboRow`);
/// out-of-range indexes fall back to the default.
pub fn spacing_at(index: u32) -> &'static str {
    SPACINGS.get(index as usize).copied().unwrap_or("medium")
}

/// Config → Calendar → "Week starts on" labels, indexed 0=Sunday .. 6=Saturday
/// (the stored `calendar-week-start` scheme).
pub const WEEK_START_LABELS: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];

/// Config → Calendar → "Working days" presets: a label and the
/// `calendar-work-days` value it stores (day numbers 0=Sunday .. 6=Saturday).
pub const WORK_DAY_PRESETS: &[(&str, &[&str])] = &[
    ("Monday to Friday", &["1", "2", "3", "4", "5"]),
    ("Sunday to Thursday", &["0", "1", "2", "3", "4"]),
    ("Monday to Saturday", &["1", "2", "3", "4", "5", "6"]),
    ("Every day", &["0", "1", "2", "3", "4", "5", "6"]),
    ("Weekends", &["0", "6"]),
];

/// The preset index whose day set equals `days`, or the default (Monday to
/// Friday)'s index when none matches.
pub fn work_day_preset_index(days: &[String]) -> u32 {
    WORK_DAY_PRESETS
        .iter()
        .position(|(_, preset)| preset.len() == days.len() && preset.iter().all(|day| days.iter().any(|value| value == day)))
        .unwrap_or(0) as u32
}

/// The stored day list for a [`WORK_DAY_PRESETS`] index; out-of-range indexes
/// fall back to the default preset.
pub fn work_day_preset_at(index: u32) -> Vec<String> {
    WORK_DAY_PRESETS
        .get(index as usize)
        .map(|(_, days)| days.iter().map(|day| day.to_string()).collect())
        .unwrap_or_else(|| WORK_DAY_PRESETS[0].1.iter().map(|day| day.to_string()).collect())
}

/// Config → Calendar → "Default reminder" presets: a label and the minutes
/// before the event it stores (-1 = no reminder).
pub const DEFAULT_REMINDER_PRESETS: &[(&str, i32)] = &[
    ("No reminder", -1),
    ("At time of event", 0),
    ("5 minutes before", 5),
    ("10 minutes before", 10),
    ("15 minutes before", 15),
    ("30 minutes before", 30),
    ("1 hour before", 60),
    ("1 day before", 1440),
];

/// The preset index matching `minutes`, or the default ("No reminder") index.
pub fn default_reminder_index(minutes: i32) -> u32 {
    DEFAULT_REMINDER_PRESETS.iter().position(|(_, preset)| *preset == minutes).unwrap_or(0) as u32
}

/// The stored minutes for a [`DEFAULT_REMINDER_PRESETS`] index; out-of-range
/// indexes fall back to "No reminder".
pub fn default_reminder_at(index: u32) -> i32 {
    DEFAULT_REMINDER_PRESETS.get(index as usize).map(|(_, minutes)| *minutes).unwrap_or(-1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bool_round_trip() {
        let store = resolve();
        store.set_bool(ANIMATE_TRANSITIONS, false);
        assert!(!store.get_bool(ANIMATE_TRANSITIONS));
        store.set_bool(ANIMATE_TRANSITIONS, true);
        assert!(store.get_bool(ANIMATE_TRANSITIONS));
    }

    #[test]
    fn string_round_trip() {
        let store = resolve();
        store.set_string(SORT_KEY, "sender");
        assert_eq!(store.get_string(SORT_KEY), "sender");
        store.set_string(SORT_KEY, "subject");
        assert_eq!(store.get_string(SORT_KEY), "subject");
    }

    #[test]
    fn strv_round_trip() {
        let store = resolve();
        store.set_strv(MAIL_FAVORITES, vec!["a:Inbox".into(), "b:Projects".into()]);
        assert_eq!(store.get_strv(MAIL_FAVORITES), vec!["a:Inbox", "b:Projects"]);
        store.set_strv(MAIL_FAVORITES, Vec::new());
        assert!(store.get_strv(MAIL_FAVORITES).is_empty());
        assert!(!store.get_bool(MAIL_FAVORITES_CUSTOM_ORDER));
    }

    #[test]
    fn double_round_trip() {
        let store = resolve();
        assert_eq!(store.get_double(PANE_FOLDER_WIDTH_PCT), -1.0);
        store.set_double(PANE_FOLDER_WIDTH_PCT, 13.75);
        assert_eq!(store.get_double(PANE_FOLDER_WIDTH_PCT), 13.75);
        store.set_double(PANE_MESSAGE_LIST_WIDTH_PCT, 30.0);
        assert_eq!(store.get_double(PANE_MESSAGE_LIST_WIDTH_PCT), 30.0);
        store.set_double(BACKGROUND_BRIGHTNESS, 0.4);
        assert_eq!(store.get_double(BACKGROUND_BRIGHTNESS), 0.4);
    }

    #[test]
    fn every_pane_width_key_has_the_same_never_set_default() {
        // All six must be declared in `defaults()`, not left to the getter's
        // catch-all: the memory backend is meant to mirror the schema exactly,
        // and a key missing here is indistinguishable from one that is present
        // until someone changes that catch-all.
        let store = resolve();
        for key in [
            PANE_FOLDER_WIDTH_PCT,
            PANE_MESSAGE_LIST_WIDTH_PCT,
            PANE_CALENDAR_SIDEBAR_WIDTH_PCT,
            PANE_CONTACTS_SIDEBAR_WIDTH_PCT,
            PANE_CONFIG_SIDEBAR_WIDTH_PCT,
            PANE_MAIL_OVERVIEW_WIDTH_PCT,
        ] {
            assert_eq!(store.get_double(key), -1.0, "{key} should default to the never-set sentinel");
            store.set_double(key, 21.5);
            assert_eq!(store.get_double(key), 21.5, "{key} should round-trip");
        }
    }

    #[test]
    fn int_round_trip() {
        let store = resolve();
        assert_eq!(store.get_int(PREFETCH_BATCH_INTERVAL_SECONDS), 30);
        store.set_int(PREFETCH_BATCH_INTERVAL_SECONDS, 15);
        assert_eq!(store.get_int(PREFETCH_BATCH_INTERVAL_SECONDS), 15);
        store.set_int(PREFETCH_FOLDER_LIMIT, 1000);
        assert_eq!(store.get_int(PREFETCH_FOLDER_LIMIT), 1000);
        store.set_int(PREFETCH_BATCH_SIZE, 10);
        assert_eq!(store.get_int(PREFETCH_BATCH_SIZE), 10);
        store.set_int(PREFETCH_REFRESH_INTERVAL_MINUTES, 120);
        assert_eq!(store.get_int(PREFETCH_REFRESH_INTERVAL_MINUTES), 120);
    }

    #[test]
    fn window_geometry_round_trip() {
        let store = resolve();
        assert_eq!(store.get_int(WINDOW_WIDTH), 1600);
        assert_eq!(store.get_int(WINDOW_HEIGHT), 900);
        assert!(!store.get_bool(WINDOW_MAXIMIZED));
        store.set_int(WINDOW_WIDTH, 1200);
        store.set_int(WINDOW_HEIGHT, 800);
        store.set_bool(WINDOW_MAXIMIZED, true);
        assert_eq!(store.get_int(WINDOW_WIDTH), 1200);
        assert_eq!(store.get_int(WINDOW_HEIGHT), 800);
        assert!(store.get_bool(WINDOW_MAXIMIZED));
    }

    #[test]
    fn prefetch_policy_assembly() {
        let store = resolve();
        let off = store.prefetch_policy();
        assert!(!off.aggressive);
        assert_eq!(off.folder_limit, 200);
        assert_eq!(off.batch_size, 1);
        store.set_bool(PREFETCH_AGGRESSIVE, true);
        let on = store.prefetch_policy();
        assert!(on.aggressive);
        assert_eq!(on.batch_interval, std::time::Duration::from_secs(30));
        assert_eq!(on.folder_limit, 200);
        assert_eq!(on.batch_size, 1);
        assert_eq!(on.refresh_interval, std::time::Duration::from_secs(60 * 60));
        store.set_int(PREFETCH_BATCH_INTERVAL_SECONDS, 10);
        store.set_int(PREFETCH_FOLDER_LIMIT, 500);
        store.set_int(PREFETCH_BATCH_SIZE, 20);
        store.set_int(PREFETCH_REFRESH_INTERVAL_MINUTES, 45);
        let tuned = store.prefetch_policy();
        assert_eq!(tuned.batch_interval, std::time::Duration::from_secs(10));
        assert_eq!(tuned.folder_limit, 500);
        assert_eq!(tuned.batch_size, 20);
        assert_eq!(tuned.refresh_interval, std::time::Duration::from_secs(45 * 60));
        store.set_bool(PREFETCH_AGGRESSIVE, false);
        assert!(!store.prefetch_policy().aggressive);
    }

    #[test]
    fn spacing_helpers_map_values_to_css_classes_and_indexes() {
        assert_eq!(spacing_index("tight"), 0);
        assert_eq!(spacing_index("medium"), 1);
        assert_eq!(spacing_index("loose"), 2);
        assert_eq!(spacing_index("no-such-spacing"), 1, "unknown values land on the default");
        assert_eq!(spacing_at(0), "tight");
        assert_eq!(spacing_at(1), "medium");
        assert_eq!(spacing_at(2), "loose");
        assert_eq!(spacing_at(99), "medium", "out-of-range indexes land on the default");
        assert_eq!(spacing_class("tight"), "spacing-tight");
        assert_eq!(spacing_class("medium"), "spacing-medium");
        assert_eq!(spacing_class("loose"), "spacing-loose");
        assert_eq!(spacing_class("no-such-spacing"), "spacing-medium");
    }

    #[test]
    fn untouched_keys_report_the_schema_defaults() {
        let store = resolve();
        assert!(!store.get_bool(MAIL_LOAD_REMOTE_IMAGES));
        assert!(store.get_bool(ANIMATE_TRANSITIONS));
        assert!(store.get_bool(CALENDAR_ALERTS_ENABLED));
        assert!(store.get_bool(BACKGROUND_WALLPAPER_ENABLED));
        assert_eq!(store.get_string(LAST_VIEW_MAILBOX), "");
        assert_eq!(store.get_string(SORT_KEY), "date");
        assert_eq!(store.get_string(ASSISTANT_API_URL), "");
        assert_eq!(store.get_string(ASSISTANT_AGENT), "");
        assert_eq!(store.get_double(BACKGROUND_BRIGHTNESS), 0.75);
        assert_eq!(store.get_string(THEME_ID), "flat-dark");
        assert_eq!(store.get_string(ACCENT_COLOR), "");
        assert_eq!(store.get_string(LAYOUT_SPACING), "medium");
        assert_eq!(store.get_int(PREFETCH_BATCH_INTERVAL_SECONDS), 30);
        assert_eq!(store.get_int(PREFETCH_FOLDER_LIMIT), 200);
        assert_eq!(store.get_int(PREFETCH_BATCH_SIZE), 1);
        assert_eq!(store.get_int(PREFETCH_REFRESH_INTERVAL_MINUTES), 60);
        assert!(!store.get_bool(PREFETCH_AGGRESSIVE));
        assert!(store.get_strv(MAIL_FAVORITES).is_empty());
        assert!(!store.get_bool(MAIL_FAVORITES_CUSTOM_ORDER));
        assert!(store.get_strv(ACCOUNTS_DISABLED).is_empty());
        assert!(store.get_strv(SHORTCUTS).is_empty());
        assert_eq!(store.get_int(CALENDAR_WEEK_START), 0);
        assert_eq!(store.get_strv(CALENDAR_WORK_DAYS), vec!["1", "2", "3", "4", "5"]);
        assert_eq!(store.get_int(CALENDAR_WORK_START), 8);
        assert_eq!(store.get_int(CALENDAR_WORK_END), 18);
        assert_eq!(store.get_string(CALENDAR_PRIMARY_TZ), "");
        assert_eq!(store.get_int(CALENDAR_DEFAULT_REMINDER), -1);
    }

    #[test]
    fn calendar_preferences_assembly_validates_and_defaults() {
        use chrono::Weekday;
        let store = resolve();
        assert_eq!(store.calendar_preferences(), CalendarPreferences::default(), "a fresh store reads the schema defaults");

        store.set_int(CALENDAR_WEEK_START, 1);
        store.set_strv(CALENDAR_WORK_DAYS, vec!["1".into(), "2".into(), "2".into(), "6".into()]);
        store.set_int(CALENDAR_WORK_START, 9);
        store.set_int(CALENDAR_WORK_END, 17);
        store.set_string(CALENDAR_PRIMARY_TZ, "Europe/Berlin");
        store.set_string(CALENDAR_SECONDARY_TZ, "  ");
        store.set_int(CALENDAR_DEFAULT_REMINDER, 15);
        let prefs = store.calendar_preferences();
        assert_eq!(prefs.week_start, Weekday::Mon);
        assert_eq!(prefs.work_days, vec![Weekday::Mon, Weekday::Tue, Weekday::Sat], "dedup keeps the first occurrence");
        assert_eq!((prefs.work_start, prefs.work_end), (9, 17));
        assert_eq!(prefs.primary_tz.as_deref(), Some("Europe/Berlin"));
        assert_eq!(prefs.secondary_tz, None, "whitespace-only reads as unset");
        assert_eq!(prefs.default_reminder_minutes, Some(15));

        // Out-of-range or inverted values fall back rather than producing a
        // broken grid.
        store.set_int(CALENDAR_WEEK_START, 99);
        store.set_strv(CALENDAR_WORK_DAYS, vec!["9".into(), "x".into()]);
        store.set_int(CALENDAR_WORK_START, 20);
        store.set_int(CALENDAR_WORK_END, 5);
        store.set_int(CALENDAR_DEFAULT_REMINDER, -1);
        let fallback = store.calendar_preferences();
        assert_eq!(fallback.week_start, Weekday::Sun);
        assert_eq!(fallback.work_days, CalendarPreferences::default().work_days);
        assert_eq!((fallback.work_start, fallback.work_end), (8, 18));
        assert_eq!(fallback.default_reminder_minutes, None);
    }

    #[test]
    fn calendar_preset_helpers_round_trip() {
        assert_eq!(work_day_preset_at(0), vec!["1", "2", "3", "4", "5"]);
        assert_eq!(work_day_preset_index(&work_day_preset_at(3)), 3, "every day");
        assert_eq!(work_day_preset_index(&["9".to_string()]), 0, "an unknown set falls back to the default preset");
        assert_eq!(work_day_preset_at(99), work_day_preset_at(0));

        assert_eq!(default_reminder_at(0), -1);
        assert_eq!(default_reminder_at(6), 60);
        assert_eq!(default_reminder_index(15), 4);
        assert_eq!(default_reminder_index(1234), 0, "an unknown value falls back to No reminder");
        assert_eq!(default_reminder_at(99), -1);
    }
}
