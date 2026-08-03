//! Persistent record of the folder pane's last-selected view.
//!
//! The user-facing ask is that the folder pane reopens on whichever folder
//! (or the synthetic "All Inboxes" unified view) was open when the app quit,
//! defaulting to "All Inboxes" on the very first run. This module owns the
//! persistence half: a plain JSON file (`$XDG_CONFIG_HOME/lookout/last-view.json`)
//! recording whether the last view was the unified one or a single mailbox
//! (by `MailboxId`, which encodes its owning account). Everything here is
//! best-effort - a missing, corrupt, or unwritable file just means the pane
//! falls back to its default, never an error.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

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

/// `$XDG_CONFIG_HOME/lookout/last-view.json` (or the equivalent
/// `~/.config` path when `XDG_CONFIG_HOME` is unset).
pub fn last_view_path() -> PathBuf {
    config_dir().join("lookout").join("last-view.json")
}

/// Loads the saved selection. Any failure (missing file, bad JSON, unreadable
/// dir) yields `None` - the folder pane then falls back to its "All Inboxes"
/// default, which is the correct first-run behavior.
pub fn load() -> Option<LastSelection> {
    match std::fs::read_to_string(last_view_path()) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
            tracing::warn!(path = %last_view_path().display(), "ignoring unreadable last-view.json: {e}");
            None
        }),
        Err(_) => None,
    }
}

/// Writes the selection out (creating the directory as needed). Best-effort:
/// a read-only home or disk error only logs a warning - forgetting to remember
/// the last view is harmless.
pub fn save(selection: &LastSelection) {
    let path = last_view_path();
    if let Some(dir) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            tracing::warn!(path = %path.display(), "could not create config dir: {e}");
            return;
        }
    }
    match serde_json::to_string_pretty(selection) {
        Ok(text) => {
            if let Err(e) = std::fs::write(&path, text) {
                tracing::warn!(path = %path.display(), "could not save last view: {e}");
            }
        }
        Err(e) => tracing::warn!("could not serialize last view: {e}"),
    }
}
