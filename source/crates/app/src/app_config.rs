//! The relational-data config file (`$XDG_CONFIG_HOME/lookout/settings.json`).
//!
//! GSettings (see `settings.rs`) holds the scalar preferences; this file is
//! for relational data with no natural GSettings key: sending identities and
//! folder-role overrides. Nothing populates it yet - the multi-identity and
//! role-override features are still roadmap items - so the structs here are
//! the on-disk contract those features will fill in, written now so the file
//! gets a home and a tested shape. Best-effort like `last_view.rs`: a missing
//! or broken file reads back as defaults, never an error.

use std::path::PathBuf;

use lookout_core::MailboxRole;
use serde::{Deserialize, Serialize};

/// One sending identity: a name/address pair the composer can send as.
/// `account_id` pins the identity to a GOA account; an empty list means the
/// composer keeps sending as the account's own address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Identity {
    pub name: String,
    pub email: String,
    pub account_id: Option<String>,
}

/// A user override of a mailbox's special-use role (e.g. "this folder is my
/// Archive"), winning over the server's `LIST (SPECIAL-USE)` attributes and
/// the name-heuristic fallback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderRoleOverride {
    pub account_id: String,
    pub mailbox: String,
    pub role: MailboxRole,
}

/// The whole file's contents. All fields are additive: an older build's file
/// (or a `{}` hand-edited file) deserializes into defaults for the rest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AppConfig {
    pub identities: Vec<Identity>,
    pub folder_role_overrides: Vec<FolderRoleOverride>,
}

fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| {
        let home = std::env::var_os("HOME").unwrap_or_else(|| std::env::var_os("USERPROFILE").unwrap_or_default());
        PathBuf::from(home).join(".config")
    })
}

/// `$XDG_CONFIG_HOME/lookout/settings.json` (or the equivalent `~/.config`
/// path when `XDG_CONFIG_HOME` is unset).
pub fn config_path() -> PathBuf {
    config_dir().join("lookout").join("settings.json")
}

/// Loads the config. Any failure (missing file, bad JSON, unreadable dir)
/// yields the default (empty) config - relational data is additive, so losing
/// it only costs the not-yet-implemented features it would carry.
pub fn load() -> AppConfig {
    load_at(&config_path())
}

/// Writes the config out (creating the directory as needed). Best-effort: a
/// read-only home or disk error only logs a warning - the config is a
/// preference, not user data.
pub fn save(config: &AppConfig) {
    save_at(&config_path(), config);
}

fn load_at(path: &std::path::Path) -> AppConfig {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
            tracing::warn!(path = %path.display(), "ignoring unreadable settings.json: {e}");
            AppConfig::default()
        }),
        Err(_) => AppConfig::default(),
    }
}

fn save_at(path: &std::path::Path, config: &AppConfig) {
    if let Some(dir) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            tracing::warn!(path = %path.display(), "could not create config dir: {e}");
            return;
        }
    }
    match serde_json::to_string_pretty(config) {
        Ok(text) => {
            if let Err(e) = std::fs::write(path, text) {
                tracing::warn!(path = %path.display(), "could not save settings.json: {e}");
            }
        }
        Err(e) => tracing::warn!("could not serialize settings.json: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Tests use `load_at`/`save_at` against a temp path instead of pointing
    // the module at `XDG_CONFIG_HOME`: that env var is process-global and
    // already contended by the `background_image`/`tags` tests.
    #[test]
    fn config_round_trip_and_tolerance() {
        let dir = std::env::temp_dir().join(format!("lookout-app-config-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");

        // Missing file -> default config.
        assert_eq!(load_at(&path), AppConfig::default());

        let config = AppConfig {
            identities: vec![Identity {
                name: "Ada".into(),
                email: "ada@example.com".into(),
                account_id: Some("account_1".into()),
            }],
            folder_role_overrides: vec![FolderRoleOverride {
                account_id: "account_1".into(),
                mailbox: "account_1:Archive 2024".into(),
                role: MailboxRole::Archive,
            }],
        };
        save_at(&path, &config);
        assert_eq!(load_at(&path), config);

        // A broken file also reads back as defaults, never an error.
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(load_at(&path), AppConfig::default());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
