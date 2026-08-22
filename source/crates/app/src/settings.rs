//! GSettings-backed persistence for the app's scalar preferences.
//!
//! The Phase 5 goal is that every user preference survives restarts. The
//! scalars (layout toggles, sort key/direction, favorites, the Mail-section
//! switches, and the session-memory keys migrated from `last_view.rs` /
//! `background_image.rs`) live in a `gio::Settings` object for the
//! `io.github.gavindi.Lookout` schema. That schema is found either
//! system-installed (a packaged install puts it in
//! `/usr/share/glib-2.0/schemas`) or, for cargo-run dev builds, compiled by
//! `build.rs` into `$OUT_DIR` and registered at runtime as an extra schema
//! source.
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
pub const SORT_KEY: &str = "sort-key";
pub const SORT_DESCENDING: &str = "sort-descending";
pub const MAIL_FAVORITES: &str = "mail-favorites";
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
pub const MAIL_NOTIFICATIONS_ENABLED: &str = "mail-notifications-enabled";
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

/// Finds the schema in the system install first (which also honours
/// `GSETTINGS_SCHEMA_DIR`), then in the `OUT_DIR` bundle `build.rs` compiled.
/// `gio::Settings::new` aborts on a missing schema, so it's only ever called
/// after a successful lookup here.
fn create_gio_settings() -> Option<gio::Settings> {
    if let Some(source) = gio::SettingsSchemaSource::default() {
        if source.lookup(SCHEMA_ID, true).is_some() {
            return Some(gio::Settings::new(SCHEMA_ID));
        }
    }
    let out_dir = std::path::PathBuf::from(env!("OUT_DIR"));
    if let Ok(source) = gio::SettingsSchemaSource::from_directory(&out_dir, None, false) {
        if let Some(schema) = source.lookup(SCHEMA_ID, true) {
            return Some(gio::Settings::new_full(&schema, None::<&gio::SettingsBackend>, None));
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
    map.insert(SORT_KEY, Value::String("date".into()));
    map.insert(SORT_DESCENDING, Value::Bool(true));
    map.insert(MAIL_FAVORITES, Value::Strv(Vec::new()));
    map.insert(ACCOUNTS_DISABLED, Value::Strv(Vec::new()));
    map.insert(MAIL_THREADED, Value::Bool(true));
    map.insert(MAIL_LOAD_REMOTE_IMAGES, Value::Bool(false));
    map.insert(MAIL_SEND_READ_RECEIPTS, Value::Bool(false));
    map.insert(MAIL_RICH_TEXT_DEFAULT, Value::Bool(true));
    map.insert(MAIL_MESSAGE_THEME_DARK, Value::Bool(false));
    map.insert(CALENDAR_ALERTS_ENABLED, Value::Bool(true));
    map.insert(MAIL_NOTIFICATIONS_ENABLED, Value::Bool(true));
    map.insert(DOCK_BADGE_ENABLED, Value::Bool(true));
    map.insert(TRAY_ICON_ENABLED, Value::Bool(false));
    map.insert(SHORTCUTS, Value::Strv(Vec::new()));
    map.insert(LAST_VIEW_UNIFIED, Value::Bool(false));
    map.insert(LAST_VIEW_MAILBOX, Value::String(String::new()));
    map.insert(PREFETCH_AGGRESSIVE, Value::Bool(false));
    map.insert(PREFETCH_BATCH_INTERVAL_SECONDS, Value::Int(30));
    map.insert(PREFETCH_FOLDER_LIMIT, Value::Int(200));
    map.insert(PREFETCH_BATCH_SIZE, Value::Int(3));
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
    fn prefetch_policy_assembly() {
        let store = resolve();
        let off = store.prefetch_policy();
        assert!(!off.aggressive);
        assert_eq!(off.folder_limit, 200);
        assert_eq!(off.batch_size, 3);
        store.set_bool(PREFETCH_AGGRESSIVE, true);
        let on = store.prefetch_policy();
        assert!(on.aggressive);
        assert_eq!(on.batch_interval, std::time::Duration::from_secs(30));
        assert_eq!(on.folder_limit, 200);
        assert_eq!(on.batch_size, 3);
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
        assert_eq!(store.get_string(LAST_VIEW_MAILBOX), "");
        assert_eq!(store.get_string(SORT_KEY), "date");
        assert_eq!(store.get_double(BACKGROUND_BRIGHTNESS), 0.75);
        assert_eq!(store.get_string(THEME_ID), "flat-dark");
        assert_eq!(store.get_string(ACCENT_COLOR), "");
        assert_eq!(store.get_string(LAYOUT_SPACING), "medium");
        assert_eq!(store.get_int(PREFETCH_BATCH_INTERVAL_SECONDS), 30);
        assert_eq!(store.get_int(PREFETCH_FOLDER_LIMIT), 200);
        assert_eq!(store.get_int(PREFETCH_BATCH_SIZE), 3);
        assert_eq!(store.get_int(PREFETCH_REFRESH_INTERVAL_MINUTES), 60);
        assert!(!store.get_bool(PREFETCH_AGGRESSIVE));
        assert!(store.get_strv(MAIL_FAVORITES).is_empty());
        assert!(store.get_strv(ACCOUNTS_DISABLED).is_empty());
        assert!(store.get_strv(SHORTCUTS).is_empty());
    }
}
