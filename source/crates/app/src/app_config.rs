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

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::path::PathBuf;

use lookout_core::{AccountId, Identity, MailboxRole, QuickStep, Rule, Signature, WebcalSubscription};
use serde::{Deserialize, Serialize};

use crate::categories::CategoryDef;

/// The id prefix distinguishing manually-added accounts (stored here) from
/// GOA accounts (whose ids are D-Bus object paths like
/// `/org/gnome/OnlineAccounts/Accounts/account_1234`). Everything in the app
/// keys on the `AccountId` string, so the prefix is just the marker the
/// account management UI greps for.
pub const OTHER_ACCOUNT_ID_PREFIX: &str = "other:";
/// The id prefix for shared mailboxes added by address (item 5.6 / M-42).
pub const SHARED_MAILBOX_ID_PREFIX: &str = "shared:";
/// The id prefix for shared/delegated calendars opened from another user's
/// mailbox (item 5.6 / CAL-25).
pub const SHARED_CALENDAR_ID_PREFIX: &str = "sharedcal:";

/// A saved search shown as a virtual folder in the folder pane (item 2.4 /
/// M-19): a display name and a query in the search language
/// (`mail/src/search_query.rs`: `from:`, `is:unread`, `category:`, …).
/// Stored in `settings.json` beside the rules and Quick Steps; the shipped
/// defaults are seeded once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchFolder {
    pub id: String,
    pub name: String,
    pub query: String,
}

/// The shipped saved searches (item 2.4 / M-19), seeded once like the Quick
/// Steps. "From VIPs" is intentionally not a default: it needs the starred
/// contacts, so it stays a query the user can write.
pub fn default_search_folders() -> Vec<SearchFolder> {
    [
        ("search:unread", "Unread mail", "is:unread"),
        ("search:flagged", "Flagged", "is:flagged"),
        ("search:large", "Large mail", "size:>5MB"),
        ("search:attachments", "Has attachments", "has:attachment"),
    ]
    .into_iter()
    .map(|(id, name, query)| SearchFolder {
        id: id.to_string(),
        name: name.to_string(),
        query: query.to_string(),
    })
    .collect()
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

/// One shared mailbox added by address (OUTLOOK_PARITY.md item 5.6 / M-42).
/// A shared mailbox is a second mailbox a Microsoft 365 user has been granted
/// access to; Lookout surfaces it as its own account-like group, reusing the
/// parent account's OAuth token rather than the delegate's own credentials.
/// Never holds a secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedMailbox {
    /// Stable `shared:<uuid>` id. Filesystem-safe (the mail cache derives
    /// filenames from it) and collision-free.
    pub id: String,
    /// The Microsoft 365 account whose OAuth token (and Graph consent) this
    /// shared mailbox borrows.
    pub parent_account: AccountId,
    /// The shared mailbox's own address (e.g. `support@contoso.com`).
    pub address: String,
    /// The mailbox's display name, for the folder pane.
    pub display_name: String,
    /// The shared mailbox's Graph user id, resolved once from `address`
    /// (`GET /users/{address}`) and cached here so `/users/{id}/...` requests
    /// don't need a lookup each time. `None` until resolved.
    #[serde(default)]
    pub owner_id: Option<String>,
}

/// One shared or delegated calendar opened from another user's mailbox
/// (OUTLOOK_PARITY.md item 5.6 / CAL-25). The other user's mailbox is read
/// through Graph (`/users/{owner}/calendars`), gated by the
/// `Calendars.ReadWrite.Shared` consent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedCalendar {
    /// Stable `sharedcal:<uuid>` id.
    pub id: String,
    /// The Microsoft 365 account whose OAuth token borrows the access.
    pub parent_account: AccountId,
    /// The other user's address (e.g. `ada@contoso.com`).
    pub address: String,
    /// The other user's Graph user id, resolved from `address`.
    pub owner_id: String,
    /// The Graph calendar id (`href`) under that user's mailbox.
    pub calendar_id: String,
    /// The calendar's display name, for the sidebar.
    pub display_name: String,
}

/// One local, on-device task list (OUTLOOK_PARITY.md item 4.8 / T-01). The
/// fallback task store when no connected server carries `VTODO`s is a single
/// `CalendarId("local")`; this gives it user-created sibling lists as well, so
/// the Tasks navigation pane behaves the same whether a list is server-backed
/// or local. Structured config, so it lives in `settings.json` beside the rules
/// and Quick Steps (cross-cutting rule §10). A task's list is its synthetic
/// calendar id (`local` for the built-in default, `local:<id>` otherwise).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalTaskList {
    /// Stable id. The built-in default list is always `"local"` (its synthetic
    /// calendar id is the bare `CalendarId("local")`); a user list is a UUID.
    pub id: String,
    pub name: String,
}

/// The synthetic calendar id of the built-in default local task list - the
/// same `CalendarId` the pre-multi-list fallback store used, kept so existing
/// `local_tasks` rows keep resolving.
pub const LOCAL_DEFAULT_LIST_ID: &str = "local";

/// One account's default-signature selections - which signature its new
/// messages (and its replies & forwards) start with, by id. `None` (or an id
/// that's no longer in `signatures`, which `remove_signature` clears and the
/// settings dropdowns heal) means no signature.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AccountSignatureDefaults {
    pub new: Option<uuid::Uuid>,
    pub reply: Option<uuid::Uuid>,
}

/// One account's automatic replies (item 2.11 / M-24). This is the Sieve
/// backend's source of truth: the settings dialog writes it and the Sieve push
/// path recompiles the account's script from it. Microsoft 365 accounts are
/// configured server-side through Graph; their fetched state lives in the UI
/// instead of here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountAutomaticReplies {
    pub account_id: AccountId,
    pub settings: lookout_core::AutomaticReplies,
}

/// The whole file's contents. All fields are additive: an older build's file
/// (or a `{}` hand-edited file) deserializes into defaults for the rest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AppConfig {
    pub identities: Vec<Identity>,
    /// The user-authored signatures (see `signatures.rs` for the editor UI).
    /// Global - not pinned to an account - so the list lives on the Accounts
    /// settings screen and a future send-time step can pick one per message.
    pub signatures: Vec<Signature>,
    /// Which signature each mail account applies by default - keyed by the
    /// account id, per-account rows in Config → Mail accounts select from
    /// the global `signatures` list.
    pub signature_defaults: std::collections::HashMap<AccountId, AccountSignatureDefaults>,
    pub folder_role_overrides: Vec<FolderRoleOverride>,
    pub webcal_subscriptions: Vec<WebcalSubscription>,
    pub other_accounts: Vec<OtherAccount>,
    /// Shared mailboxes added by address (item 5.6 / M-42), each borrowing a
    /// parent Microsoft 365 account's OAuth token and Graph consent.
    #[serde(default)]
    pub shared_mailboxes: Vec<SharedMailbox>,
    /// Shared or delegated calendars opened from another user's mailbox
    /// (item 5.6 / CAL-25), read through Graph under the parent account's
    /// token.
    #[serde(default)]
    pub shared_calendars: Vec<SharedCalendar>,
    /// The shared category master list (item 2.1 / M-09, CAL-07, T-08), used
    /// by mail, events and tasks. Replaces the pre-2.1 `tags.json`; a legacy
    /// file is migrated on first load (see `categories::migrate_legacy_tags`).
    /// On Microsoft 365 accounts the list is synced with Graph's
    /// `masterCategories`.
    #[serde(default)]
    pub categories: Vec<CategoryDef>,
    /// The user's rules (item 2.5 / M-14), evaluated in list order. Each rule
    /// names the backend it runs on (`This device`, Sieve or Graph); a rule
    /// whose backend is unavailable degrades to the client engine. See
    /// `lookout_core::Rule` and `app/src/rules.rs`.
    #[serde(default)]
    pub rules: Vec<Rule>,
    /// The user-authored "My Templates" snippets (item 2.10 / C-17) inserted
    /// into the composer. Global - not pinned to an account. `#[serde(default)]`
    /// is required so an older `settings.json` without the key still
    /// deserializes (the struct has no container-level default).
    #[serde(default)]
    pub templates: Vec<lookout_core::MailTemplate>,
    /// Per-account automatic replies (item 2.11 / M-24) for the Sieve backend.
    /// Graph accounts keep theirs on the server, so they never appear here.
    #[serde(default)]
    pub automatic_replies: Vec<AccountAutomaticReplies>,
    /// The user's Quick Steps (item 2.6 / M-15): named action chains run by
    /// hand from the Mail ribbon's **Quick steps** gallery or the
    /// `Ctrl+Shift+1`…`9` slots (in gallery order). Local only - stored here
    /// because Outlook's own Quick Steps live in the mailbox, which IMAP has
    /// no equivalent of (`app/src/quick_steps.rs` runs them).
    #[serde(default)]
    pub quick_steps: Vec<QuickStep>,
    /// Whether the shipped default Quick Steps have been seeded once. Set on
    /// the first load so that deleting every step stays deleted (a plain
    /// "empty means seed" guard would resurrect them on the next launch).
    #[serde(default)]
    pub quick_steps_seeded: bool,
    /// The user's saved searches (item 2.4 / M-19), shown as virtual folders
    /// in the folder pane. Structured config, so they live in `settings.json`
    /// beside the rules and Quick Steps; the shipped defaults are seeded once.
    #[serde(default)]
    pub search_folders: Vec<SearchFolder>,
    /// Whether the shipped saved searches have been seeded once (same guard as
    /// `quick_steps_seeded`, so deleting them all stays deleted).
    #[serde(default)]
    pub search_folders_seeded: bool,
    /// The user's local on-device task lists (item 4.8 / T-01), beyond the
    /// built-in default. The built-in default (`"local"`) is synthesized when
    /// absent, so an empty list is a valid, fully-working state.
    #[serde(default)]
    pub local_task_lists: Vec<LocalTaskList>,
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

    /// The parent Microsoft 365 account whose OAuth token a shared mailbox
    /// borrows, when `id` names one (item 5.6 / M-42).
    pub fn shared_mailbox_parent(&self, id: &AccountId) -> Option<AccountId> {
        self.shared_mailboxes.iter().find(|m| m.id == id.0).map(|m| m.parent_account.clone())
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

    /// Inserts `signature`, replacing any existing entry with the same id
    /// (the edit path) or appending it (the add path).
    pub fn upsert_signature(&mut self, signature: Signature) {
        if let Some(existing) = self.signatures.iter_mut().find(|s| s.id == signature.id) {
            *existing = signature;
        } else {
            self.signatures.push(signature);
        }
    }

    /// Inserts `rule`, replacing any existing entry with the same id (the edit
    /// path) or appending it (the add path).
    pub fn upsert_rule(&mut self, rule: Rule) {
        if let Some(existing) = self.rules.iter_mut().find(|r| r.id == rule.id) {
            *existing = rule;
        } else {
            self.rules.push(rule);
        }
    }

    /// Inserts `template`, replacing any existing entry with the same id (the
    /// edit path) or appending it (the add path).
    pub fn upsert_template(&mut self, template: lookout_core::MailTemplate) {
        if let Some(existing) = self.templates.iter_mut().find(|t| t.id == template.id) {
            *existing = template;
        } else {
            self.templates.push(template);
        }
    }

    /// Removes the template with `id`, if any.
    pub fn remove_template(&mut self, id: &uuid::Uuid) {
        self.templates.retain(|t| &t.id != id);
    }

    /// Removes the rule with `id`, if any.
    pub fn remove_rule(&mut self, id: &str) {
        self.rules.retain(|r| r.id != id);
    }

    /// Inserts `step`, replacing any existing entry with the same id (the edit
    /// path) or appending it (the add path) - so gallery order is list order.
    pub fn upsert_quick_step(&mut self, step: QuickStep) {
        if let Some(existing) = self.quick_steps.iter_mut().find(|s| s.id == step.id) {
            *existing = step;
        } else {
            self.quick_steps.push(step);
        }
    }

    /// Removes the quick step with `id`, if any.
    pub fn remove_quick_step(&mut self, id: &str) {
        self.quick_steps.retain(|s| s.id != id);
    }

    /// Moves the quick step at `from` to `to`, clamping both to the list's
    /// bounds - the manager's reorder controls (which also change the
    /// `Ctrl+Shift+1`…`9` slot a step occupies).
    pub fn move_quick_step(&mut self, from: usize, to: usize) {
        if from >= self.quick_steps.len() || to >= self.quick_steps.len() || from == to {
            return;
        }
        let step = self.quick_steps.remove(from);
        self.quick_steps.insert(to, step);
    }

    /// The automatic-replies settings configured for `account_id`, if any.
    pub fn automatic_replies_for(&self, account_id: &AccountId) -> Option<lookout_core::AutomaticReplies> {
        self.automatic_replies
            .iter()
            .find(|entry| &entry.account_id == account_id)
            .map(|entry| entry.settings.clone())
    }

    /// Inserts `account_id`'s automatic replies, replacing any existing entry
    /// (the edit path) or appending it (the first save). Turning replies off
    /// stores a `Disabled` entry rather than deleting it, so the messages are
    /// still there the next time they're switched on.
    pub fn upsert_automatic_replies(&mut self, account_id: AccountId, settings: lookout_core::AutomaticReplies) {
        if let Some(existing) = self.automatic_replies.iter_mut().find(|entry| entry.account_id == account_id) {
            existing.settings = settings;
        } else {
            self.automatic_replies.push(AccountAutomaticReplies { account_id, settings });
        }
    }

    /// Moves the rule at `from` to `to`, clamping both to the list's bounds -
    /// the editor's reorder controls.
    pub fn move_rule(&mut self, from: usize, to: usize) {
        if from >= self.rules.len() || to >= self.rules.len() || from == to {
            return;
        }
        let rule = self.rules.remove(from);
        self.rules.insert(to, rule);
    }

    /// Removes the signature with `id`, if any - and clears every account
    /// default-signature slot that pointed at it, so a deleted signature can
    /// never leave a dangling default behind.
    pub fn remove_signature(&mut self, id: &uuid::Uuid) {
        self.signatures.retain(|s| &s.id != id);
        for defaults in self.signature_defaults.values_mut() {
            if defaults.new.as_ref() == Some(id) {
                defaults.new = None;
            }
            if defaults.reply.as_ref() == Some(id) {
                defaults.reply = None;
            }
        }
    }

    /// Every local task list, with the built-in default always present first.
    /// A rename of the default is persisted as an explicit `"local"` entry;
    /// the button also never removes it (see [`Self::remove_local_task_list`]),
    /// so a renamed default keeps its name.
    pub fn local_task_lists(&self) -> Vec<LocalTaskList> {
        let mut lists = self.local_task_lists.clone();
        if !lists.iter().any(|l| l.id == LOCAL_DEFAULT_LIST_ID) {
            lists.insert(
                0,
                LocalTaskList {
                    id: LOCAL_DEFAULT_LIST_ID.to_string(),
                    name: "Tasks".to_string(),
                },
            );
        }
        lists
    }

    /// Inserts `list`, replacing any existing entry with the same id (the edit
    /// path) or appending it (the add path).
    pub fn upsert_local_task_list(&mut self, list: LocalTaskList) {
        if let Some(existing) = self.local_task_lists.iter_mut().find(|l| l.id == list.id) {
            *existing = list;
        } else {
            self.local_task_lists.push(list);
        }
    }

    /// Removes the local task list with `id`, if any. The built-in default is
    /// never removed (its entry, if any, records only a rename).
    pub fn remove_local_task_list(&mut self, id: &str) {
        if id == LOCAL_DEFAULT_LIST_ID {
            return;
        }
        self.local_task_lists.retain(|l| l.id != id);
    }
}

pub(crate) fn config_dir() -> PathBuf {
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
    let mut config = load_at(&config_path());
    // One-time migration: move a pre-2.1 `tags.json` into the shared category
    // master list. Only when the user has no categories yet, so a later edit
    // is never clobbered by a stale legacy file.
    if config.categories.is_empty() {
        if let Some(migrated) = crate::categories::migrate_legacy_tags() {
            config.categories = migrated;
            save(&config);
        }
    }
    // One-time seed of the shipped Quick Steps (item 2.6 / M-15). The seeded
    // flag keeps a deliberately emptied gallery empty on the next launch.
    if !config.quick_steps_seeded {
        config.quick_steps = lookout_core::default_quick_steps();
        config.quick_steps_seeded = true;
        save(&config);
    }
    // One-time seed of the shipped saved searches (item 2.4 / M-19), same
    // guard so an intentionally emptied list stays empty.
    if !config.search_folders_seeded {
        config.search_folders = default_search_folders();
        config.search_folders_seeded = true;
        save(&config);
    }
    config
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
            signatures: Vec::new(),
            signature_defaults: std::collections::HashMap::new(),
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
            shared_mailboxes: vec![SharedMailbox {
                id: "shared:abc".into(),
                parent_account: AccountId("account_1".into()),
                address: "support@example.com".into(),
                display_name: "Support".into(),
                owner_id: Some("u1".into()),
            }],
            shared_calendars: vec![SharedCalendar {
                id: format!("{SHARED_CALENDAR_ID_PREFIX}abc"),
                parent_account: AccountId("account_1".into()),
                address: "ada@example.com".into(),
                owner_id: "u2".into(),
                calendar_id: "AAAACAL".into(),
                display_name: "Ada's Calendar".into(),
            }],
            categories: Vec::new(),
            rules: Vec::new(),
            templates: Vec::new(),
            automatic_replies: Vec::new(),
            quick_steps: Vec::new(),
            quick_steps_seeded: false,
            search_folders: Vec::new(),
            search_folders_seeded: false,
            local_task_lists: vec![LocalTaskList {
                id: "work".into(),
                name: "Work".into(),
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
            signatures: Vec::new(),
            signature_defaults: std::collections::HashMap::new(),
            folder_role_overrides: Vec::new(),
            webcal_subscriptions: Vec::new(),
            other_accounts: Vec::new(),
            shared_mailboxes: Vec::new(),
            shared_calendars: Vec::new(),
            categories: Vec::new(),
            rules: Vec::new(),
            templates: Vec::new(),
            automatic_replies: Vec::new(),
            quick_steps: Vec::new(),
            quick_steps_seeded: false,
            search_folders: Vec::new(),
            search_folders_seeded: false,
            local_task_lists: Vec::new(),
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
    fn default_search_folders_are_unique_and_parse() {
        let folders = default_search_folders();
        assert!(!folders.is_empty());
        let mut ids: Vec<&str> = folders.iter().map(|f| f.id.as_str()).collect();
        ids.sort_unstable();
        let unique = {
            let mut seen = ids.clone();
            seen.dedup();
            seen
        };
        assert_eq!(unique.len(), folders.len(), "ids are unique");
        for folder in &folders {
            // Every shipped query parses in the search language.
            let _ = lookout_mail::parse_search_query(&folder.query);
            assert!(!folder.name.trim().is_empty() && !folder.query.trim().is_empty());
        }
    }

    #[test]
    fn shared_mailbox_parent_resolves_the_owning_account() {
        let mut config = AppConfig::default();
        config.shared_mailboxes.push(SharedMailbox {
            id: format!("{SHARED_MAILBOX_ID_PREFIX}1"),
            parent_account: AccountId("acct".into()),
            address: "support@example.com".into(),
            display_name: "Support".into(),
            owner_id: None,
        });
        assert_eq!(
            config.shared_mailbox_parent(&AccountId(format!("{SHARED_MAILBOX_ID_PREFIX}1"))),
            Some(AccountId("acct".into()))
        );
        assert_eq!(config.shared_mailbox_parent(&AccountId("nope".into())), None);
    }

    #[test]
    fn shared_calendars_round_trip_through_the_config_file() {
        let mut config = AppConfig::default();
        config.shared_calendars.push(SharedCalendar {
            id: format!("{SHARED_CALENDAR_ID_PREFIX}1"),
            parent_account: AccountId("acct".into()),
            address: "ada@example.com".into(),
            owner_id: "u2".into(),
            calendar_id: "AAAACAL".into(),
            display_name: "Ada's Calendar".into(),
        });
        let json = serde_json::to_string(&config).unwrap();
        let loaded: AppConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.shared_calendars, config.shared_calendars);
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

    #[test]
    fn remove_signature_clears_defaults_pointing_at_it() {
        let account_a = AccountId("account_a".into());
        let account_b = AccountId("account_b".into());
        let mut config = AppConfig::default();
        let work = Signature::new("Work", "<p>Work</p>", "Work");
        let personal = Signature::new("Personal", "<p>Personal</p>", "Personal");
        config.upsert_signature(work.clone());
        config.upsert_signature(personal.clone());
        config.signature_defaults.insert(
            account_a.clone(),
            AccountSignatureDefaults {
                new: Some(work.id),
                reply: Some(personal.id),
            },
        );
        config.signature_defaults.insert(account_b.clone(), AccountSignatureDefaults::default());

        // Removing a signature that is nobody's default only drops the row.
        config.remove_signature(&personal.id);
        assert_eq!(config.signatures.len(), 1);
        assert_eq!(config.signature_defaults[&account_a].new, Some(work.id));
        assert_eq!(config.signature_defaults[&account_a].reply, None, "the reply default must clear with its signature");

        config.remove_signature(&work.id);
        assert_eq!(config.signature_defaults[&account_a].new, None, "the new-message default must clear with its signature");

        // Removing an unknown id is a no-op.
        config.remove_signature(&uuid::Uuid::new_v4());
        assert!(config.signatures.is_empty());
    }

    #[test]
    fn signature_defaults_round_trip_through_the_config_file() {
        let dir = std::env::temp_dir().join(format!("lookout-defaults-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");

        // A file predating the defaults (no such key) loads as an empty map.
        std::fs::write(&path, r#"{"identities": []}"#).unwrap();
        assert!(load_at(&path).signature_defaults.is_empty());

        let mut config = AppConfig::default();
        let signature = Signature::new("Work", "<p>Work</p>", "Work");
        config.upsert_signature(signature.clone());
        config.signature_defaults.insert(
            AccountId("account_1".into()),
            AccountSignatureDefaults {
                new: Some(signature.id),
                reply: Some(signature.id),
            },
        );
        save_at(&path, &config);
        let loaded = load_at(&path);
        assert_eq!(loaded.signature_defaults.len(), 1);
        assert_eq!(loaded.signature_defaults[&AccountId("account_1".into())].new, Some(signature.id));
        assert_eq!(loaded.signature_defaults[&AccountId("account_1".into())].reply, Some(signature.id));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn templates_round_trip_upsert_and_remove() {
        let dir = std::env::temp_dir().join(format!("lookout-templates-config-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");

        // A file predating `templates` loads as an empty list rather than
        // failing (the key carries `#[serde(default)]`).
        std::fs::write(&path, r#"{"identities": []}"#).unwrap();
        assert!(load_at(&path).templates.is_empty());

        let mut config = AppConfig::default();
        let status = lookout_core::MailTemplate::new("Status update", "<p>Status: </p>", "Status: ");
        let meeting = lookout_core::MailTemplate::new("Meeting notes", "<p>Notes</p>", "Notes");
        config.upsert_template(status.clone());
        config.upsert_template(meeting.clone());
        assert_eq!(config.templates.len(), 2);

        // Upsert replaces in place rather than appending a duplicate.
        let mut edited = status.clone();
        edited.html = "<p>Status: done</p>".into();
        config.upsert_template(edited.clone());
        assert_eq!(config.templates.len(), 2);
        assert_eq!(config.templates[0].html, "<p>Status: done</p>");

        config.remove_template(&meeting.id);
        assert_eq!(config.templates.len(), 1);
        // Removing an unknown id is a no-op.
        config.remove_template(&uuid::Uuid::new_v4());
        assert_eq!(config.templates.len(), 1);

        save_at(&path, &config);
        assert_eq!(load_at(&path).templates, vec![edited]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn automatic_replies_round_trip_upsert_and_remove() {
        let dir = std::env::temp_dir().join(format!("lookout-auto-replies-config-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");

        // A file predating `automatic_replies` loads as an empty list.
        std::fs::write(&path, r#"{"identities": []}"#).unwrap();
        assert!(load_at(&path).automatic_replies.is_empty());

        let account = AccountId("account_1".into());
        let other = AccountId("account_2".into());
        let away = lookout_core::AutomaticReplies {
            status: lookout_core::AutomaticReplyStatus::AlwaysEnabled,
            internal_message: "Away until Friday.".into(),
            ..lookout_core::AutomaticReplies::default()
        };
        let mut config = AppConfig::default();
        assert_eq!(config.automatic_replies_for(&account), None);
        config.upsert_automatic_replies(account.clone(), away.clone());
        assert_eq!(config.automatic_replies_for(&account), Some(away.clone()));

        // Upsert replaces in place rather than appending a duplicate.
        let mut edited = away.clone();
        edited.internal_message = "Back Monday.".into();
        config.upsert_automatic_replies(account.clone(), edited.clone());
        assert_eq!(config.automatic_replies.len(), 1);
        assert_eq!(config.automatic_replies_for(&account), Some(edited.clone()));

        config.upsert_automatic_replies(other.clone(), lookout_core::AutomaticReplies::default());
        save_at(&path, &config);
        let loaded = load_at(&path);
        assert_eq!(loaded.automatic_replies_for(&account), Some(edited));
        assert_eq!(loaded.automatic_replies_for(&other), Some(lookout_core::AutomaticReplies::default()));
        assert_eq!(loaded.automatic_replies_for(&AccountId("account_nope".into())), None);

        // Turning replies off keeps the entry (messages are remembered).
        let mut loaded = loaded;
        loaded.upsert_automatic_replies(account.clone(), lookout_core::AutomaticReplies::default());
        assert_eq!(loaded.automatic_replies_for(&account), Some(lookout_core::AutomaticReplies::default()));
        assert_eq!(loaded.automatic_replies.len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rules_round_trip_upsert_move_and_remove() {
        let dir = std::env::temp_dir().join(format!("lookout-rules-config-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");

        // A file predating `rules` loads as an empty list.
        std::fs::write(&path, r#"{"identities": []}"#).unwrap();
        assert!(load_at(&path).rules.is_empty());

        let mut config = AppConfig::default();
        let mut a = lookout_core::Rule::new("r1", "Archive newsletters");
        a.conditions = vec![lookout_core::RuleCondition::ListId("news.example.com".into())];
        a.actions = vec![lookout_core::RuleAction::Move(lookout_core::RuleTarget::Role(MailboxRole::Archive))];
        let b = lookout_core::Rule::new("r2", "Flag VIPs");
        config.upsert_rule(a.clone());
        config.upsert_rule(b.clone());
        assert_eq!(config.rules.len(), 2);

        // Upsert replaces in place.
        let mut edited = a.clone();
        edited.name = "Archive news".into();
        config.upsert_rule(edited.clone());
        assert_eq!(config.rules.len(), 2);
        assert_eq!(config.rules[0].name, "Archive news");

        // Reorder, then remove.
        config.move_rule(0, 1);
        assert_eq!(config.rules[0].id, "r2");
        config.remove_rule("r2");
        assert_eq!(config.rules.len(), 1);

        save_at(&path, &config);
        let loaded = load_at(&path);
        assert_eq!(loaded.rules, config.rules);
        // An out-of-range move is a no-op, never a panic.
        let mut loaded = loaded;
        loaded.move_rule(9, 0);
        assert_eq!(loaded.rules.len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn quick_steps_round_trip_upsert_move_and_remove() {
        let dir = std::env::temp_dir().join(format!("lookout-quick-steps-config-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");

        // A file predating `quick_steps` loads as an empty list and an unseeded
        // flag (the seed runs in `load`, against the real config path).
        std::fs::write(&path, r#"{"identities": []}"#).unwrap();
        let loaded = load_at(&path);
        assert!(loaded.quick_steps.is_empty());
        assert!(!loaded.quick_steps_seeded);

        let mut config = AppConfig::default();
        let mut a = lookout_core::QuickStep::new("q1", "Archive and read");
        a.actions = vec![lookout_core::RuleAction::Move(lookout_core::RuleTarget::Role(MailboxRole::Archive))];
        let b = lookout_core::QuickStep::new("q2", "Flag");
        config.upsert_quick_step(a.clone());
        config.upsert_quick_step(b.clone());
        assert_eq!(config.quick_steps.len(), 2);

        // Upsert replaces in place; reorder; remove.
        let mut edited = a.clone();
        edited.name = "Archive".into();
        config.upsert_quick_step(edited.clone());
        assert_eq!(config.quick_steps.len(), 2);
        assert_eq!(config.quick_steps[0].name, "Archive");
        config.move_quick_step(0, 1);
        assert_eq!(config.quick_steps[0].id, "q2");
        config.remove_quick_step("q2");
        assert_eq!(config.quick_steps.len(), 1);

        config.quick_steps_seeded = true;
        save_at(&path, &config);
        let loaded = load_at(&path);
        assert_eq!(loaded.quick_steps, config.quick_steps);
        assert!(loaded.quick_steps_seeded);

        // Out-of-range moves and unknown removals are no-ops, never a panic.
        let mut loaded = loaded;
        loaded.move_quick_step(9, 0);
        assert_eq!(loaded.quick_steps.len(), 1);
        loaded.remove_quick_step("nope");
        assert_eq!(loaded.quick_steps.len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn local_task_lists_synthesize_the_default_round_trip_and_protect_it() {
        let dir = std::env::temp_dir().join(format!("lookout-local-task-lists-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");

        // A file predating `local_task_lists` loads as an empty list, and the
        // built-in default is still synthesized.
        std::fs::write(&path, r#"{"identities": []}"#).unwrap();
        let loaded = load_at(&path);
        assert!(loaded.local_task_lists.is_empty());
        assert_eq!(loaded.local_task_lists().len(), 1);
        assert_eq!(loaded.local_task_lists()[0].id, LOCAL_DEFAULT_LIST_ID);
        assert_eq!(loaded.local_task_lists()[0].name, "Tasks");

        // A user list is added, upserted in place, and round-trips.
        let mut config = AppConfig::default();
        config.upsert_local_task_list(LocalTaskList {
            id: "work".into(),
            name: "Work".into(),
        });
        config.upsert_local_task_list(LocalTaskList {
            id: "work".into(),
            name: "Work (renamed)".into(),
        });
        assert_eq!(config.local_task_lists.len(), 1);
        let find = |config: &AppConfig, id: &str| config.local_task_lists().into_iter().find(|l| l.id == id);
        assert_eq!(find(&config, "work").unwrap().name, "Work (renamed)");
        // The default is present alongside the user list.
        assert_eq!(config.local_task_lists().len(), 2);

        // Renaming the default persists an explicit entry, and the default is
        // protected: a removal request is a no-op, so the name stays.
        config.upsert_local_task_list(LocalTaskList {
            id: LOCAL_DEFAULT_LIST_ID.into(),
            name: "Personal".into(),
        });
        assert_eq!(find(&config, LOCAL_DEFAULT_LIST_ID).unwrap().name, "Personal");
        config.remove_local_task_list(LOCAL_DEFAULT_LIST_ID);
        assert_eq!(find(&config, LOCAL_DEFAULT_LIST_ID).unwrap().name, "Personal", "the default list can't be deleted");

        // A user list can be removed; a missing one is a no-op.
        config.remove_local_task_list("work");
        assert!(find(&config, "work").is_none());
        config.remove_local_task_list("nope");

        save_at(&path, &config);
        assert_eq!(load_at(&path).local_task_lists, config.local_task_lists);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
