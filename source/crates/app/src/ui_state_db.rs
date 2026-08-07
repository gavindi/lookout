//! SQLite persistence for app-level UI state that isn't mail or calendar
//! data: today, the People screen's starred contacts, the calendar's
//! event-reminder state, and local-only tasks.
//!
//! Starred contacts are `(account, contact-identity)` pairs the user marked
//! as favourites. They're stored in their own database
//! (`$XDG_CACHE_HOME/lookout/ui-state.sqlite`) rather than in a mail account's
//! cache file for two reasons: a CardDAV-only account never gets a mail
//! `Cache` handle, and the Advanced section's "Clear all caches" action wipes
//! the mail/calendar caches - favourites are a preference, not a cache, and
//! must survive it.
//!
//! Event-reminder state is the fire-once/snooze bookkeeping for calendar
//! reminders: for each reminder that has fired, whether it is done ("fired")
//! or deferred until a later instant ("snoozed"). It lives here because a
//! reminder is a UI concern - the calendar data itself doesn't change when a
//! reminder fires - and it must survive both the "Clear all caches" action
//! and a `user_version` bump, so it is keyed by the event's own identity
//! (`calendar_id`, `uid`, `start_utc`) and re-created fresh on every open.
//!
//! Local-only tasks are the same "UI concern, must survive cache wipes" case:
//! when no connected calendar supports tasks, the task editor falls back to a
//! `CalendarId("local")` store that lives only on this device. They're the
//! full serialized `CalendarTask` JSON, keyed by their (client-generated,
//! UUID) uid.
//!
//! Best-effort like the rest of the config modules: an unreadable or
//! unwritable database just means favourites don't persist and reminders can
//! fire again.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use lookout_core::AccountId;
use rusqlite::Connection;

/// On-disk format version. Bumping it wipes the `starred_contacts` table once,
/// mirroring `lookout-mail`'s cache convention, so a schema change can never
/// serve rows written by an older build. The `reminder_state` table is not
/// wiped by this: it is keyed by the event's own identity and re-created
/// (empty, with `CREATE TABLE IF NOT EXISTS`) on every open anyway, so there
/// is nothing stale it could serve.
const UI_STATE_VERSION: i64 = 2;

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

/// One persisted reminder-state row, the `reminder_state` table's shape.
pub struct ReminderStateRow {
    pub calendar_id: String,
    pub uid: String,
    pub start_utc: String,
    pub state: String,
    pub snooze_until_utc: Option<String>,
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
            CREATE TABLE IF NOT EXISTS reminder_state (
                calendar_id TEXT NOT NULL,
                uid TEXT NOT NULL,
                start_utc TEXT NOT NULL,
                state TEXT NOT NULL,
                snooze_until_utc TEXT,
                PRIMARY KEY (calendar_id, uid, start_utc)
            );
            CREATE TABLE IF NOT EXISTS local_tasks (
                uid TEXT PRIMARY KEY,
                data TEXT NOT NULL
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

    /// Every persisted reminder-state row, ordered by the primary key so the
    /// calendar can walk occurrences deterministically.
    pub fn load_reminder_state(&self) -> rusqlite::Result<Vec<ReminderStateRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT calendar_id, uid, start_utc, state, snooze_until_utc
             FROM reminder_state
             ORDER BY calendar_id, uid, start_utc",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(ReminderStateRow {
                calendar_id: row.get(0)?,
                uid: row.get(1)?,
                start_utc: row.get(2)?,
                state: row.get(3)?,
                snooze_until_utc: row.get(4)?,
            })
        })?;
        rows.collect()
    }

    /// Records a reminder's fire-once/snooze state, replacing any previous row
    /// for the same event (`calendar_id`, `uid`, `start_utc`). Idempotent:
    /// re-recording the same state is a no-op row-wise. `state` is "fired" or
    /// "snoozed"; `snooze_until_utc` is the RFC 3339 UTC instant the snooze
    /// ends, meaningful only when `state` is "snoozed".
    pub fn set_reminder_state(&self, calendar_id: &str, uid: &str, start_utc: &str, state: &str, snooze_until_utc: Option<&str>) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO reminder_state (calendar_id, uid, start_utc, state, snooze_until_utc)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![calendar_id, uid, start_utc, state, snooze_until_utc],
        )?;
        Ok(())
    }

    /// Forgets a reminder's persisted state. Used when a stale occurrence is
    /// pruned so it can never resurface from a previous fire.
    pub fn clear_reminder(&self, calendar_id: &str, uid: &str, start_utc: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "DELETE FROM reminder_state WHERE calendar_id = ?1 AND uid = ?2 AND start_utc = ?3",
            rusqlite::params![calendar_id, uid, start_utc],
        )?;
        Ok(())
    }

    /// Every locally-stored task (the `CalendarId("local")` fallback store),
    /// ordered by uid. Unparseable rows (a schema/format change mid-flight)
    /// are skipped with a warning rather than failing the whole load.
    pub fn load_local_tasks(&self) -> rusqlite::Result<Vec<lookout_core::CalendarTask>> {
        let mut stmt = self.conn.prepare("SELECT data FROM local_tasks ORDER BY uid")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            let data: String = row?;
            match serde_json::from_str(&data) {
                Ok(task) => out.push(task),
                Err(e) => tracing::warn!("skipping unparseable local task row: {e}"),
            }
        }
        Ok(out)
    }

    /// Stores (replacing any same-uid row) one local task.
    pub fn save_local_task(&self, task: &lookout_core::CalendarTask) -> rusqlite::Result<()> {
        let data = serde_json::to_string(task).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        self.conn
            .execute("INSERT OR REPLACE INTO local_tasks (uid, data) VALUES (?1, ?2)", rusqlite::params![task.uid.0, data])?;
        Ok(())
    }

    /// Removes one local task by its uid. A missing row is not an error.
    pub fn delete_local_task(&self, uid: &str) -> rusqlite::Result<()> {
        self.conn.execute("DELETE FROM local_tasks WHERE uid = ?1", [uid])?;
        Ok(())
    }
}

/// `XDG_CACHE_HOME` is process-global, so every test that touches it - this
/// module's and `reminders`'s (which opens a `UiStateDb` of its own) - must
/// take this lock first: a unique directory per test alone doesn't stop one
/// thread's `set_var` from redirecting another's open (or `remove_dir_all`)
/// into the wrong tree, and SQLite then reports "database is locked" against
/// the other test's live connection. The single-test `XDG_CONFIG_HOME`
/// modules (`background_image`/`tags`) get away without a lock only because
/// they have nothing to race against.
#[cfg(test)]
pub(crate) static CACHE_HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    // A single test owns `XDG_CACHE_HOME`: the env var is process-global and
    // parallel test threads would race over it otherwise (the same rule as
    // the `XDG_CONFIG_HOME` tests in `background_image`/`tags`).
    #[test]
    fn starred_contacts_round_trip_and_version_wipe() {
        let _guard = CACHE_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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

    // Same `XDG_CACHE_HOME` rule as above: each reminder test takes the
    // module lock and gets its own subdirectory.
    #[test]
    fn reminder_state_round_trip_and_snooze() {
        let _guard = CACHE_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("lookout-ui-state-test-{}", std::process::id()));
        let dir = dir.join("reminders");
        std::env::set_var("XDG_CACHE_HOME", &dir);
        let _ = std::fs::remove_dir_all(&dir);

        let db = UiStateDb::open().expect("fresh database should open");
        assert!(db.load_reminder_state().unwrap().is_empty());

        // Fired → the row is visible.
        db.set_reminder_state("cal-1", "event-1", "2026-08-07T09:00:00Z", "fired", None).unwrap();
        let loaded = db.load_reminder_state().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].calendar_id, "cal-1");
        assert_eq!(loaded[0].uid, "event-1");
        assert_eq!(loaded[0].start_utc, "2026-08-07T09:00:00Z");
        assert_eq!(loaded[0].state, "fired");
        assert_eq!(loaded[0].snooze_until_utc, None);

        // Snoozing the same event replaces the row (INSERT OR REPLACE), and
        // the row count stays at one.
        db.set_reminder_state("cal-1", "event-1", "2026-08-07T09:00:00Z", "snoozed", Some("2026-08-07T09:10:00Z"))
            .unwrap();
        let loaded = db.load_reminder_state().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].state, "snoozed");
        assert_eq!(loaded[0].snooze_until_utc.as_deref(), Some("2026-08-07T09:10:00Z"));

        // Re-setting without a snooze deadline clears the stale one.
        db.set_reminder_state("cal-1", "event-1", "2026-08-07T09:00:00Z", "fired", None).unwrap();
        let loaded = db.load_reminder_state().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].state, "fired");
        assert_eq!(loaded[0].snooze_until_utc, None);

        // Clear removes exactly this row; the rest survive.
        db.set_reminder_state("cal-1", "event-2", "2026-08-08T09:00:00Z", "fired", None).unwrap();
        db.clear_reminder("cal-1", "event-1", "2026-08-07T09:00:00Z").unwrap();
        let loaded = db.load_reminder_state().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].uid, "event-2");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reminder_state_survives_reopen_and_version_wipe() {
        let _guard = CACHE_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("lookout-ui-state-test-{}", std::process::id()));
        let dir = dir.join("reminders-reopen");
        std::env::set_var("XDG_CACHE_HOME", &dir);
        let _ = std::fs::remove_dir_all(&dir);

        let db = UiStateDb::open().expect("fresh database should open");
        db.set_reminder_state("cal-1", "event-1", "2026-08-07T09:00:00Z", "fired", None).unwrap();
        db.set_starred(&AccountId("account-1".into()), "ada@example.com", true).unwrap();

        // A fresh session reads the same reminder rows.
        let reopened = UiStateDb::open().expect("existing database should reopen");
        let loaded = reopened.load_reminder_state().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].uid, "event-1");
        assert_eq!(loaded[0].state, "fired");

        // Downgrading the on-disk version wipes only `starred_contacts`;
        // `reminder_state` is created fresh on open and never wiped, so the
        // reminder row must survive.
        reopened.conn.pragma_update(None, "user_version", 1).expect("test can write its own version");
        drop(reopened);
        let upgraded = UiStateDb::open().unwrap();
        assert!(upgraded.load_starred().unwrap().is_empty());
        let loaded = upgraded.load_reminder_state().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].uid, "event-1");
        assert_eq!(loaded[0].state, "fired");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn local_tasks_round_trip_reopen_and_wipe_survival() {
        let _guard = CACHE_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("lookout-ui-state-test-{}", std::process::id()));
        let dir = dir.join("local-tasks");
        std::env::set_var("XDG_CACHE_HOME", &dir);
        let _ = std::fs::remove_dir_all(&dir);

        let db = UiStateDb::open().expect("fresh database should open");
        assert!(db.load_local_tasks().unwrap().is_empty());

        let task = |uid: &str, summary: &str| lookout_core::CalendarTask {
            uid: lookout_core::TaskUid(uid.to_string()),
            calendar_id: lookout_core::CalendarId("local".to_string()),
            summary: Some(summary.to_string()),
            description: None,
            due: None,
            start: None,
            completed: None,
            status: lookout_core::TaskStatus::default(),
            priority: lookout_core::TaskPriority::default(),
            percent_complete: None,
            categories: Vec::new(),
            href: None,
            etag: None,
        };
        db.save_local_task(&task("t-1", "First")).unwrap();
        db.save_local_task(&task("t-2", "Second")).unwrap();
        // Same uid replaces rather than duplicates.
        db.save_local_task(&task("t-1", "First edited")).unwrap();

        let loaded = db.load_local_tasks().unwrap();
        assert_eq!(loaded.len(), 2);
        assert!(loaded.iter().any(|t| t.summary.as_deref() == Some("First edited")));

        // Deletion removes only that row; a missing row is not an error.
        db.delete_local_task("t-1").unwrap();
        db.delete_local_task("t-1").unwrap();
        let loaded = db.load_local_tasks().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].summary.as_deref(), Some("Second"));

        // Local tasks live in the UI-state database, so a version wipe (the
        // "Clear all caches"-adjacent convention) must NOT touch them.
        let reopened = UiStateDb::open().unwrap();
        reopened.conn.pragma_update(None, "user_version", 1).expect("test can write its own version");
        drop(reopened);
        let upgraded = UiStateDb::open().unwrap();
        let loaded = upgraded.load_local_tasks().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].summary.as_deref(), Some("Second"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
