//! SQLite persistence for app-level UI state that isn't mail or calendar
//! data: today, the People screen's starred contacts.
//!
//! Starred contacts are `(account, contact-identity)` pairs the user marked
//! as favourites. They're stored in their own database
//! (`$XDG_CACHE_HOME/lookout/ui-state.sqlite`) rather than in a mail account's
//! cache file for two reasons: a CardDAV-only account never gets a mail
//! `Cache` handle, and the Advanced section's "Clear all caches" action wipes
//! the mail/calendar caches - favourites are a preference, not a cache, and
//! must survive it. Best-effort like the rest of the config modules: an
//! unreadable or unwritable database just means favourites don't persist.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use lookout_core::AccountId;
use rusqlite::Connection;

/// On-disk format version. Bumping it wipes the `starred_contacts` table once,
/// mirroring `lookout-mail`'s cache convention, so a schema change can never
/// serve rows written by an older build.
const UI_STATE_VERSION: i64 = 1;

fn cache_dir() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir)
}

/// `$XDG_CACHE_HOME/lookout/ui-state.sqlite` (or the equivalent `~/.cache`
/// path when `XDG_CACHE_HOME` is unset).
pub fn db_path() -> PathBuf {
    cache_dir().join("lookout").join("ui-state.sqlite")
}

/// The UI-state database. Owned by the UI thread (GTK is single-threaded),
/// like the app's read-side mail `Cache` handle.
pub struct UiStateDb {
    conn: Connection,
}

impl UiStateDb {
    /// Opens (creating if needed) the UI-state database. Any failure is
    /// reported as an error; callers treat it as "favourites won't persist"
    /// and continue.
    pub fn open() -> Result<Self, Box<dyn std::error::Error>> {
        let path = db_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)?;
        // WAL + a busy timeout so a background writer never collides with the
        // UI thread's reads, matching the mail cache's choice.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS starred_contacts (
                account TEXT NOT NULL,
                identity TEXT NOT NULL,
                PRIMARY KEY (account, identity)
            );
            ",
        )?;
        let stored: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap_or(0);
        if stored < UI_STATE_VERSION {
            conn.execute("DELETE FROM starred_contacts", [])?;
            conn.pragma_update(None, "user_version", UI_STATE_VERSION)?;
        }
        Ok(UiStateDb { conn })
    }

    /// Every starred `(account, identity)` pair in the database, grouped by
    /// account - the persisted shape of `UiState::starred_contacts`.
    pub fn load_starred(&self) -> rusqlite::Result<HashMap<AccountId, HashSet<String>>> {
        let mut stmt = self.conn.prepare("SELECT account, identity FROM starred_contacts ORDER BY account")?;
        let rows = stmt.query_map([], |row| Ok((AccountId(row.get::<_, String>(0)?), row.get::<_, String>(1)?)))?;
        let mut out: HashMap<AccountId, HashSet<String>> = HashMap::new();
        for row in rows {
            let (account, identity) = row?;
            out.entry(account).or_default().insert(identity);
        }
        Ok(out)
    }

    /// Stars or unstars `identity` for `account`. Idempotent: re-starring an
    /// already-starred contact and unstarring one that isn't starred are both
    /// no-ops.
    pub fn set_starred(&self, account: &AccountId, identity: &str, starred: bool) -> rusqlite::Result<()> {
        if starred {
            self.conn.execute(
                "INSERT OR IGNORE INTO starred_contacts (account, identity) VALUES (?1, ?2)",
                rusqlite::params![account.0, identity],
            )?;
        } else {
            self.conn
                .execute("DELETE FROM starred_contacts WHERE account = ?1 AND identity = ?2", rusqlite::params![account.0, identity])?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A single test owns `XDG_CACHE_HOME`: the env var is process-global and
    // parallel test threads would race over it otherwise (the same rule as
    // the `XDG_CONFIG_HOME` tests in `background_image`/`tags`).
    #[test]
    fn starred_contacts_round_trip_and_version_wipe() {
        let dir = std::env::temp_dir().join(format!("lookout-ui-state-test-{}", std::process::id()));
        let dir = dir.join("round-trip");
        std::env::set_var("XDG_CACHE_HOME", &dir);
        let _ = std::fs::remove_dir_all(&dir);

        let db = UiStateDb::open().expect("fresh database should open");
        assert!(db.load_starred().unwrap().is_empty());

        let account = AccountId("/org/gnome/OnlineAccounts/Accounts/account_1".into());
        db.set_starred(&account, "ada@example.com", true).unwrap();
        db.set_starred(&account, "ada@example.com", true).unwrap();
        db.set_starred(&account, "grace@example.com", true).unwrap();

        let loaded = db.load_starred().unwrap();
        assert_eq!(loaded.get(&account).unwrap().len(), 2);

        // Unstarring is idempotent too, and drops only that identity.
        db.set_starred(&account, "ada@example.com", false).unwrap();
        db.set_starred(&account, "ada@example.com", false).unwrap();
        let loaded = db.load_starred().unwrap();
        assert_eq!(loaded.get(&account).unwrap().len(), 1);
        assert!(loaded.get(&account).unwrap().contains("grace@example.com"));

        // A second open (as a fresh session would) reads the same rows.
        let reopened = UiStateDb::open().expect("existing database should reopen");
        assert_eq!(reopened.load_starred().unwrap(), loaded);

        // Downgrading the on-disk version wipes the table exactly once.
        reopened.conn.pragma_update(None, "user_version", 0).expect("test can write its own version");
        drop(reopened);
        let upgraded = UiStateDb::open().unwrap();
        assert!(upgraded.load_starred().unwrap().is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
