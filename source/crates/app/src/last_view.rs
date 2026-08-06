//! Persistent record of the folder pane's last-selected view.
//!
//! The user-facing ask is that the folder pane reopens on whichever folder
//! (or the synthetic "All Inboxes" unified view) was open when the app quit,
//! defaulting to "All Inboxes" on the very first run. This module owns the
//! persistence half, over the GSettings `last-view-unified` and
//! `last-view-mailbox` keys (see `settings.rs`): whether the last view was
//! the unified one or a single mailbox (by `MailboxId`, which encodes its
//! owning account). Everything here is best-effort - a missing schema just
//! means the pane falls back to its default, never an error. Runs that predate
//! GSettings left their choice in `$XDG_CONFIG_HOME/lookout/last-view.json`;
//! the first load after the migration imports it once and drops the file.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::settings::{LAST_VIEW_MAILBOX, LAST_VIEW_UNIFIED};

/// The folder pane's last-selected view. `unified` marks the synthetic
/// "All Inboxes" view; otherwise `mailbox` holds the selected `MailboxId`
/// (whose `<account>:<path>` form doubles as the account key for routing).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LastSelection {
    pub unified: bool,
    pub mailbox: Option<String>,
}

fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| {
        let home = std::env::var_os("HOME").unwrap_or_else(|| std::env::var_os("USERPROFILE").unwrap_or_default());
        PathBuf::from(home).join(".config")
    })
}

/// The pre-GSettings `last-view.json` file, consulted once by the migration
/// in `load` and then deleted.
fn legacy_path() -> PathBuf {
    config_dir().join("lookout").join("last-view.json")
}

/// Loads the saved selection. Any failure yields `None` - the folder pane
/// then falls back to its "All Inboxes" default, which is the correct
/// first-run behavior.
pub fn load(store: &crate::settings::SettingsStore) -> Option<LastSelection> {
    if store.get_bool(LAST_VIEW_UNIFIED) {
        Some(LastSelection { unified: true, mailbox: None })
    } else {
        let mailbox = store.get_string(LAST_VIEW_MAILBOX);
        if mailbox.is_empty() {
            None
        } else {
            Some(LastSelection {
                unified: false,
                mailbox: Some(mailbox),
            })
        }
    }
}

/// Writes the selection out. Best-effort: a read-only home or disk error only
/// logs a warning - forgetting to remember the last view is harmless.
pub fn save(store: &crate::settings::SettingsStore, selection: &LastSelection) {
    store.set_bool(LAST_VIEW_UNIFIED, selection.unified);
    store.set_string(LAST_VIEW_MAILBOX, selection.mailbox.as_deref().unwrap_or(""));
}

/// Runs the one-time import of the pre-GSettings `last-view.json`. Called
/// once from `build_window` before the first `load`, so a machine upgrading
/// from a plain-file build keeps its last-open folder. The import only
/// happens while the GSettings keys still hold their defaults - a session
/// that already made a choice keeps it - and the legacy file is dropped
/// whether or not it could be imported, so a stale file can never be
/// re-imported over a newer selection.
pub fn migrate_legacy(store: &crate::settings::SettingsStore) {
    migrate_legacy_file_at(&legacy_path(), store);
}

fn migrate_legacy_file_at(path: &std::path::Path, store: &crate::settings::SettingsStore) {
    if !path.exists() {
        return;
    }
    let imported = (|| -> Option<LastSelection> {
        if store.get_bool(LAST_VIEW_UNIFIED) || !store.get_string(LAST_VIEW_MAILBOX).is_empty() {
            return None;
        }
        let text = std::fs::read_to_string(path).ok()?;
        let selection: LastSelection = serde_json::from_str(&text).ok()?;
        store.set_bool(LAST_VIEW_UNIFIED, selection.unified);
        store.set_string(LAST_VIEW_MAILBOX, selection.mailbox.as_deref().unwrap_or(""));
        Some(selection)
    })();
    if imported.is_none() {
        tracing::debug!(path = %path.display(), "ignored legacy last-view.json (already migrated or unreadable)");
    }
    let _ = std::fs::remove_file(path);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_load_round_trip_and_legacy_migration() {
        let store = crate::settings::resolve();
        assert_eq!(load(&store), None);

        save(&store, &LastSelection { unified: true, mailbox: None });
        assert_eq!(load(&store), Some(LastSelection { unified: true, mailbox: None }));

        save(
            &store,
            &LastSelection {
                unified: false,
                mailbox: Some("account_1:INBOX".into()),
            },
        );
        assert_eq!(
            load(&store),
            Some(LastSelection {
                unified: false,
                mailbox: Some("account_1:INBOX".into())
            })
        );
    }

    #[test]
    fn legacy_file_is_imported_once() {
        let dir = std::env::temp_dir().join(format!("lookout-last-view-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let legacy = dir.join("last-view.json");
        let store = crate::settings::resolve();

        std::fs::write(&legacy, r#"{"unified": false, "mailbox": "old_account:INBOX"}"#).unwrap();
        migrate_legacy_file_at(&legacy, &store);
        assert_eq!(
            load(&store),
            Some(LastSelection {
                unified: false,
                mailbox: Some("old_account:INBOX".into())
            })
        );
        // The file is dropped, so the import can't run twice.
        assert!(!legacy.exists());

        // A later session's fresh keys are not clobbered by a stale file.
        std::fs::write(&legacy, r#"{"unified": true, "mailbox": null}"#).unwrap();
        save(
            &store,
            &LastSelection {
                unified: false,
                mailbox: Some("new_account:INBOX".into()),
            },
        );
        migrate_legacy_file_at(&legacy, &store);
        assert_eq!(
            load(&store),
            Some(LastSelection {
                unified: false,
                mailbox: Some("new_account:INBOX".into())
            })
        );
        assert!(!legacy.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
