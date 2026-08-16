//! The relational-data config file (`$XDG_CONFIG_HOME/lookout/settings.json`).
//!
//! GSettings (see `settings.rs`) holds the scalar preferences; this file is
//! for relational data with no natural GSettings key: sending identities,
//! folder-role overrides, webcal subscriptions, and the app's own list of
//! manually-configured IMAP/SMTP mail accounts ("other accounts" - ones not
//! managed by GNOME Online Accounts). Identities are populated by the
//! multi-identity feature (composer From selector + manage dialog); the
//! role-override feature is still a roadmap item, but the structs are the
//! on-disk contract it will fill in. Other accounts are created by the
//! add-account dialog (`other_accounts.rs`); their passwords live in the
//! GNOME keyring, never in this file. Best-effort like `last_view.rs`: a
//! missing or broken file reads back as defaults, never an error.

use std::path::PathBuf;

use lookout_core::{AccountId, Identity, MailboxRole, WebcalSubscription};
use serde::{Deserialize, Serialize};

/// The id prefix distinguishing manually-added accounts (stored here) from
/// GOA accounts (whose ids are D-Bus object paths like
/// `/org/gnome/OnlineAccounts/Accounts/account_1234`). Everything in the app
/// keys on the `AccountId` string, so the prefix is just the marker the
/// account management UI greps for.
pub const OTHER_ACCOUNT_ID_PREFIX: &str = "other:";

/// A user override of a mailbox's special-use role (e.g. "this folder is my
/// Archive"), winning over the server's `LIST (SPECIAL-USE)` attributes and
/// the name-heuristic fallback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderRoleOverride {
    pub account_id: String,
    pub mailbox: String,
    pub role: MailboxRole,
}

/// One manually-configured IMAP/SMTP mail account - a mail account Lookout
/// manages itself rather than one sourced from GNOME Online Accounts (see
/// `other_accounts.rs` for the UI and the keyring-backed password store).
/// Never holds a secret: passwords are stored in the GNOME keyring under the
/// account's `id`, and fetched fresh per connection attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OtherAccount {
    /// Stable `other:<uuid>` id. Filesystem-safe (the mail cache derives
    /// filenames from it) and collision-free.
    pub id: String,
    pub display_name: String,
    pub email: String,
    pub imap_host: String,
    pub imap_port: u16,
    pub imap_use_tls: bool,
    pub imap_username: String,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_use_tls: bool,
    pub smtp_username: String,
    /// Whether the dialog stored one password for both protocols (the common
    /// case) or distinct IMAP/SMTP passwords. The keyring always holds both
    /// slots either way - the flag only drives the edit dialog's prefill.
    pub use_same_password: bool,
}

/// The whole file's contents. All fields are additive: an older build's file
/// (or a `{}` hand-edited file) deserializes into defaults for the rest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AppConfig {
    pub identities: Vec<Identity>,
    pub folder_role_overrides: Vec<FolderRoleOverride>,
    pub webcal_subscriptions: Vec<WebcalSubscription>,
    pub other_accounts: Vec<OtherAccount>,
}

impl AppConfig {
    /// The persisted sending identities pinned to `account_id`, in file
    /// order. The account's own address is *not* in this list - it's always
    /// available implicitly (see `identities_for_account`), and a user-added
    /// identity duplicating it is treated as redundant.
    pub fn identities_for(&self, account_id: &AccountId) -> Vec<Identity> {
        self.identities.iter().filter(|i| &i.account_id == account_id).cloned().collect()
    }

    /// Every identity the composer can send as for one account: the
    /// persisted identities plus - always first - the account's own default
    /// identity (its GOA name/address). Persisted identities whose email
    /// duplicates the account's own are dropped so the default never
    /// appears twice.
    pub fn identities_for_account(&self, account_id: &AccountId, default_name: &str, default_email: &str) -> Vec<Identity> {
        let mut identities: Vec<Identity> = self
            .identities_for(account_id)
            .into_iter()
            .filter(|i| !i.email.eq_ignore_ascii_case(default_email))
            .collect();
        identities.insert(0, Identity::new(account_id.clone(), default_name, default_email));
        identities
    }

    /// The persisted other account with `id`, if any.
    pub fn other_account(&self, id: &str) -> Option<OtherAccount> {
        self.other_accounts.iter().find(|a| a.id == id).cloned()
    }

    /// Inserts `account`, replacing any existing entry with the same id
    /// (the edit path) or appending it (the add path).
    pub fn upsert_other_account(&mut self, account: OtherAccount) {
        if let Some(existing) = self.other_accounts.iter_mut().find(|a| a.id == account.id) {
            *existing = account;
        } else {
            self.other_accounts.push(account);
        }
    }
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

        let account_id = AccountId("account_1".into());
        let mut identity = Identity::new(account_id.clone(), "Ada", "ada@example.com");
        identity.reply_to = vec![lookout_core::EmailAddress::new("replies@example.com")];
        identity.bcc = vec![lookout_core::EmailAddress::new("archive@example.com")];
        let config = AppConfig {
            identities: vec![identity],
            folder_role_overrides: vec![FolderRoleOverride {
                account_id: "account_1".into(),
                mailbox: "account_1:Archive 2024".into(),
                role: MailboxRole::Archive,
            }],
            webcal_subscriptions: vec![WebcalSubscription {
                id: "sub-1".into(),
                display_name: "Holidays".into(),
                url: "https://example.com/holidays.ics".into(),
            }],
            other_accounts: vec![OtherAccount {
                id: format!("{OTHER_ACCOUNT_ID_PREFIX}abc"),
                display_name: "Work Mail".into(),
                email: "work@example.com".into(),
                imap_host: "imap.example.com".into(),
                imap_port: 993,
                imap_use_tls: true,
                imap_username: "work".into(),
                smtp_host: "smtp.example.com".into(),
                smtp_port: 587,
                smtp_use_tls: true,
                smtp_username: "work".into(),
                use_same_password: true,
            }],
        };
        save_at(&path, &config);
        assert_eq!(load_at(&path), config);

        // A broken file also reads back as defaults, never an error.
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(load_at(&path), AppConfig::default());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn identities_for_account_prepends_the_default_and_drops_duplicates() {
        let account = AccountId("account_1".into());
        let other = AccountId("account_2".into());
        let config = AppConfig {
            identities: vec![
                Identity::new(other.clone(), "Other Account", "other@example.com"),
                Identity::new(account.clone(), "Work", "work@example.com"),
                // Duplicates the account's own address - must be dropped.
                Identity::new(account.clone(), "The Account Itself", "ME@example.com"),
            ],
            folder_role_overrides: Vec::new(),
            webcal_subscriptions: Vec::new(),
            other_accounts: Vec::new(),
        };

        let identities = config.identities_for_account(&account, "My Name", "me@example.com");
        assert_eq!(identities.len(), 2);
        // The synthesized default always comes first.
        assert_eq!(identities[0].email, "me@example.com");
        assert_eq!(identities[0].name, "My Name");
        assert_eq!(identities[1].email, "work@example.com");

        // Other accounts' identities are never mixed in.
        assert!(config.identities_for(&other).iter().all(|i| i.email == "other@example.com"));
    }

    #[test]
    fn other_accounts_round_trip_upsert_and_tolerance() {
        let dir = std::env::temp_dir().join(format!("lookout-other-accounts-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");

        // A file predating `other_accounts` (no such key) loads as an empty
        // list rather than failing.
        std::fs::write(&path, r#"{"identities": []}"#).unwrap();
        assert!(load_at(&path).other_accounts.is_empty());

        let account = OtherAccount {
            id: format!("{OTHER_ACCOUNT_ID_PREFIX}1234"),
            display_name: "Self-hosted".into(),
            email: "me@mydomain.example".into(),
            imap_host: "mail.mydomain.example".into(),
            imap_port: 993,
            imap_use_tls: true,
            imap_username: "me".into(),
            smtp_host: "mail.mydomain.example".into(),
            smtp_port: 587,
            smtp_use_tls: true,
            smtp_username: "me".into(),
            use_same_password: false,
        };
        let mut config = AppConfig::default();
        config.upsert_other_account(account.clone());
        assert_eq!(config.other_account(&account.id), Some(account.clone()));
        save_at(&path, &config);
        assert_eq!(load_at(&path).other_accounts, vec![account.clone()]);

        // Upsert replaces in place rather than appending a duplicate.
        let mut edited = account.clone();
        edited.display_name = "Renamed".into();
        config.upsert_other_account(edited.clone());
        assert_eq!(config.other_accounts.len(), 1);
        assert_eq!(config.other_account(&account.id), Some(edited));
        assert_eq!(config.other_account("other:nope"), None);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
