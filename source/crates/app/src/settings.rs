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

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use gtk::gio;
use gtk::gio::prelude::{SettingsExt, SettingsExtManual};

/// The GSettings schema id - deliberately the application id, so a packaged
/// install and the dev-mode bundle resolve to the same settings.
pub const SCHEMA_ID: &str = "io.github.gavindi.Lookout";

pub const ANIMATE_TRANSITIONS: &str = "animate-transitions";
pub const WINDOW_BACKGROUND_PATH: &str = "window-background-path";
pub const BACKGROUND_BRIGHTNESS: &str = "background-brightness";
pub const THEME_ID: &str = "theme-id";
pub const ACCENT_COLOR: &str = "accent-color";
pub const LAYOUT_FOLDER_PANE: &str = "layout-folder-pane";
pub const LAYOUT_READING_PANE: &str = "layout-reading-pane";
pub const LAYOUT_CALENDAR_OVERVIEW: &str = "layout-calendar-overview";
pub const PANE_FOLDER_WIDTH_PCT: &str = "pane-folder-width-percent";
pub const PANE_MESSAGE_LIST_WIDTH_PCT: &str = "pane-message-list-width-percent";
pub const PANE_CALENDAR_SIDEBAR_WIDTH_PCT: &str = "pane-calendar-sidebar-width-percent";
pub const PANE_CONTACTS_SIDEBAR_WIDTH_PCT: &str = "pane-contacts-sidebar-width-percent";
pub const SORT_KEY: &str = "sort-key";
pub const SORT_DESCENDING: &str = "sort-descending";
pub const MAIL_FAVORITES: &str = "mail-favorites";
pub const MAIL_THREADED: &str = "mail-threaded";
pub const MAIL_LOAD_REMOTE_IMAGES: &str = "mail-load-remote-images";
pub const MAIL_SEND_READ_RECEIPTS: &str = "mail-send-read-receipts";
pub const MAIL_RICH_TEXT_DEFAULT: &str = "mail-rich-text-default";
pub const CALENDAR_ALERTS_ENABLED: &str = "calendar-alerts-enabled";
pub const MAIL_NOTIFICATIONS_ENABLED: &str = "mail-notifications-enabled";
pub const SHORTCUTS: &str = "shortcuts";
pub const LAST_VIEW_UNIFIED: &str = "last-view-unified";
pub const LAST_VIEW_MAILBOX: &str = "last-view-mailbox";

/// One stored value, mirroring the GSettings key types the app uses: booleans,
/// strings, doubles (pane-width percentages), and string arrays (favorites).
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Value {
    Bool(bool),
    String(String),
    Double(f64),
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
    map.insert(WINDOW_BACKGROUND_PATH, Value::String(String::new()));
    map.insert(BACKGROUND_BRIGHTNESS, Value::Double(1.0));
    map.insert(THEME_ID, Value::String(crate::theme::DEFAULT_THEME.into()));
    map.insert(ACCENT_COLOR, Value::String(String::new()));
    map.insert(LAYOUT_FOLDER_PANE, Value::Bool(true));
    map.insert(LAYOUT_READING_PANE, Value::Bool(true));
    map.insert(LAYOUT_CALENDAR_OVERVIEW, Value::Bool(true));
    map.insert(PANE_FOLDER_WIDTH_PCT, Value::Double(-1.0));
    map.insert(PANE_MESSAGE_LIST_WIDTH_PCT, Value::Double(-1.0));
    map.insert(PANE_CALENDAR_SIDEBAR_WIDTH_PCT, Value::Double(-1.0));
    map.insert(PANE_CONTACTS_SIDEBAR_WIDTH_PCT, Value::Double(-1.0));
    map.insert(SORT_KEY, Value::String("date".into()));
    map.insert(SORT_DESCENDING, Value::Bool(true));
    map.insert(MAIL_FAVORITES, Value::Strv(Vec::new()));
    map.insert(MAIL_THREADED, Value::Bool(true));
    map.insert(MAIL_LOAD_REMOTE_IMAGES, Value::Bool(false));
    map.insert(MAIL_SEND_READ_RECEIPTS, Value::Bool(false));
    map.insert(MAIL_RICH_TEXT_DEFAULT, Value::Bool(true));
    map.insert(CALENDAR_ALERTS_ENABLED, Value::Bool(true));
    map.insert(MAIL_NOTIFICATIONS_ENABLED, Value::Bool(true));
    map.insert(SHORTCUTS, Value::Strv(Vec::new()));
    map.insert(LAST_VIEW_UNIFIED, Value::Bool(false));
    map.insert(LAST_VIEW_MAILBOX, Value::String(String::new()));
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
    fn untouched_keys_report_the_schema_defaults() {
        let store = resolve();
        assert!(!store.get_bool(MAIL_LOAD_REMOTE_IMAGES));
        assert!(store.get_bool(ANIMATE_TRANSITIONS));
        assert!(store.get_bool(CALENDAR_ALERTS_ENABLED));
        assert_eq!(store.get_string(LAST_VIEW_MAILBOX), "");
        assert_eq!(store.get_string(SORT_KEY), "date");
        assert_eq!(store.get_double(BACKGROUND_BRIGHTNESS), 1.0);
        assert_eq!(store.get_string(THEME_ID), "flat-dark");
        assert_eq!(store.get_string(ACCENT_COLOR), "");
        assert!(store.get_strv(MAIL_FAVORITES).is_empty());
        assert!(store.get_strv(SHORTCUTS).is_empty());
    }
}
