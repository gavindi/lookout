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
//! Trusted senders are the reading pane's external-content trust flow: which
//! senders may load remote content, and at what trust level. `(account,
//! entry)` pairs like starred contacts - an entry is a lowercased address
//! (`ada@example.com`) or an `@domain` pattern - with a `TrustLevel` encoded
//! as its integer column. A preference like favourites, so it must survive
//! both the "Clear all caches" action and a `user_version` bump.
//!
//! Folder-expansion state is the folder pane's collapsed/expanded memory:
//! which subfolders the user left expanded, and which account groups (and the
//! Favorites section) they left collapsed. Like favourites it's a preference,
//! not a cache - it must survive "Clear all caches" and a `user_version`
//! bump - and it's two unbounded sets of compound keys (`{account}:{path}`
//! mailbox ids, and bare account ids plus the Favorites sentinel), which is
//! why it lives here rather than in a GSettings strv. The stored shape is the
//! two sets `rebuild_folder_tree` already carries across rebuilds, so
//! persisting is a straight capture of the session's expansion state.
//!
//! The app opens it once, as a [`UiStateStore`]: reads stay synchronous on the
//! UI thread (they happen at startup and in a few dialogs), while every write
//! is queued, in order, to a dedicated `ui-state-db` thread so a commit or a
//! lock wait never stalls the GTK main loop. [`UiStateStore::flush`] is the
//! barrier for the rare read that must see a write just made, and for quit.
//!
//! Best-effort like the rest of the config modules: an unreadable or
//! unwritable database just means favourites don't persist and reminders can
//! fire again.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use lookout_core::{AccountId, MailboxId};
use rusqlite::Connection;

/// The `kind` column values of the `folder_expansion_state` table: an
/// expanded subfolder row keyed by its `MailboxId`, or a collapsed account
/// group (or the Favorites section) keyed by its `AccountId`.
const EXPANDED_FOLDER: &str = "expanded_folder";
const COLLAPSED_GROUP: &str = "collapsed_group";

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

/// One connection to the UI-state database. The app holds a
/// [`UiStateStore`], which owns two of these: one the UI thread reads
/// through, and one its writer thread commits through.
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

/// Which number a folder row shows beside its name (the folder pane's
/// "⋯" menu → Change folder count).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum FolderCountMode {
    /// The unread count, bold in the accent colour - every folder's default.
    #[default]
    Unread,
    /// The total message count, muted.
    Total,
}

impl FolderCountMode {
    fn as_str(self) -> &'static str {
        match self {
            FolderCountMode::Unread => "unread",
            FolderCountMode::Total => "total",
        }
    }

    fn parse(value: &str) -> Self {
        match value {
            "total" => FolderCountMode::Total,
            _ => FolderCountMode::Unread,
        }
    }
}

/// A folder's local display preferences, set from the folder pane's "⋯"
/// menu: an icon colour (`#rrggbb`, or none) and which count it shows. Purely
/// client-side - IMAP has no folder colour - and keyed by `MailboxId`, so the
/// UI re-keys them when a folder is renamed or moved.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct FolderPrefs {
    pub color: Option<String>,
    pub count_mode: FolderCountMode,
}

impl FolderPrefs {
    /// Whether these are the defaults - a folder with default prefs needs no
    /// row at all.
    pub fn is_default(&self) -> bool {
        *self == FolderPrefs::default()
    }
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
        // WAL + a busy timeout so the store's writer thread never collides with
        // the UI thread's reads, matching the mail cache's choice. WAL makes
        // `synchronous=NORMAL` crash-safe (a commit may roll back on power
        // loss, never corrupt), and it drops the per-commit fsync.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
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
            CREATE TABLE IF NOT EXISTS trusted_senders (
                account TEXT NOT NULL,
                entry TEXT NOT NULL,
                level INTEGER NOT NULL DEFAULT 1,
                PRIMARY KEY (account, entry)
            );
            CREATE TABLE IF NOT EXISTS blocked_senders (
                account TEXT NOT NULL,
                entry TEXT NOT NULL,
                PRIMARY KEY (account, entry)
            );
            CREATE TABLE IF NOT EXISTS ignored_threads (
                account TEXT NOT NULL,
                thread TEXT NOT NULL,
                label TEXT NOT NULL DEFAULT '',
                PRIMARY KEY (account, thread)
            );
            CREATE TABLE IF NOT EXISTS folder_expansion_state (
                kind TEXT NOT NULL,
                key TEXT NOT NULL,
                PRIMARY KEY (kind, key)
            );
            CREATE TABLE IF NOT EXISTS folder_prefs (
                mailbox_id TEXT PRIMARY KEY,
                color TEXT,
                count_mode TEXT NOT NULL DEFAULT 'unread'
            );
            CREATE TABLE IF NOT EXISTS recent_folders (
                account TEXT NOT NULL,
                position INTEGER NOT NULL,
                mailbox_id TEXT NOT NULL,
                PRIMARY KEY (account, position)
            );
            CREATE TABLE IF NOT EXISTS recent_searches (
                position INTEGER PRIMARY KEY,
                query TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS my_day_tasks (
                day TEXT NOT NULL,
                calendar_id TEXT NOT NULL,
                uid TEXT NOT NULL,
                PRIMARY KEY (day, calendar_id, uid)
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

    /// Every persisted trusted-sender row, ordered by account then entry -
    /// the persisted shape of the reading pane's external-content trust flow
    /// (`UiState::trusted_senders`). `level` is the integer encoding of
    /// [`lookout_core::TrustLevel`]; unknown encodings resolve to the
    /// conservative `Images` level.
    pub fn load_trusted_senders(&self) -> rusqlite::Result<Vec<(AccountId, String, lookout_core::TrustLevel)>> {
        let mut stmt = self.conn.prepare("SELECT account, entry, level FROM trusted_senders ORDER BY account, entry")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                AccountId(row.get::<_, String>(0)?),
                row.get::<_, String>(1)?,
                lookout_core::TrustLevel::from_i64(row.get::<_, i64>(2)?),
            ))
        })?;
        rows.collect()
    }

    /// Stores or replaces the trust level for `(account, entry)`. Idempotent:
    /// re-setting the same entry with the same level is a no-op row-wise.
    pub fn set_trusted_sender(&self, account: &AccountId, entry: &str, level: lookout_core::TrustLevel) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO trusted_senders (account, entry, level) VALUES (?1, ?2, ?3)",
            rusqlite::params![account.0, entry, level.as_i64()],
        )?;
        Ok(())
    }

    /// Forgets one trusted-sender entry. A missing row is not an error.
    pub fn remove_trusted_sender(&self, account: &AccountId, entry: &str) -> rusqlite::Result<()> {
        self.conn
            .execute("DELETE FROM trusted_senders WHERE account = ?1 AND entry = ?2", rusqlite::params![account.0, entry])?;
        Ok(())
    }

    /// Every persisted blocked-sender entry, ordered by account then entry -
    /// the persisted shape of `UiState::blocked_senders`. Entries are
    /// lowercased addresses (`spammer@example.com`) or `@domain` patterns.
    pub fn load_blocked_senders(&self) -> rusqlite::Result<Vec<(AccountId, String)>> {
        let mut stmt = self.conn.prepare("SELECT account, entry FROM blocked_senders ORDER BY account, entry")?;
        let rows = stmt.query_map([], |row| Ok((AccountId(row.get::<_, String>(0)?), row.get::<_, String>(1)?)))?;
        rows.collect()
    }

    /// Adds or replaces a blocked-sender entry for `account`. Idempotent.
    pub fn set_blocked_sender(&self, account: &AccountId, entry: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO blocked_senders (account, entry) VALUES (?1, ?2)",
            rusqlite::params![account.0, entry],
        )?;
        Ok(())
    }

    /// Forgets one blocked-sender entry. A missing row is not an error.
    pub fn remove_blocked_sender(&self, account: &AccountId, entry: &str) -> rusqlite::Result<()> {
        self.conn
            .execute("DELETE FROM blocked_senders WHERE account = ?1 AND entry = ?2", rusqlite::params![account.0, entry])?;
        Ok(())
    }

    /// Every ignored conversation for every account (item 2.8 / M-20): the
    /// `(account, thread key, display label)` triples whose arriving mail is
    /// moved to Deleted Items. Thread keys are `lookout_core::ThreadKey`
    /// strings (a root Message-ID, or a `subject:` fallback); the label is the
    /// conversation's subject, kept for the management dialog.
    pub fn load_ignored_threads(&self) -> rusqlite::Result<Vec<(AccountId, String, String)>> {
        let mut stmt = self.conn.prepare("SELECT account, thread, label FROM ignored_threads ORDER BY account, thread")?;
        let rows = stmt.query_map([], |row| Ok((AccountId(row.get::<_, String>(0)?), row.get::<_, String>(1)?, row.get::<_, String>(2)?)))?;
        rows.collect()
    }

    /// Remembers an ignored conversation for `account`, with `label` shown in
    /// the management dialog. Idempotent.
    pub fn set_ignored_thread(&self, account: &AccountId, thread: &str, label: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO ignored_threads (account, thread, label) VALUES (?1, ?2, ?3)",
            rusqlite::params![account.0, thread, label],
        )?;
        Ok(())
    }

    /// Stops ignoring one conversation. A missing row is not an error.
    pub fn remove_ignored_thread(&self, account: &AccountId, thread: &str) -> rusqlite::Result<()> {
        self.conn
            .execute("DELETE FROM ignored_threads WHERE account = ?1 AND thread = ?2", rusqlite::params![account.0, thread])?;
        Ok(())
    }

    /// The folder pane's persisted expansion memory: the subfolders the user
    /// left expanded (`MailboxId` keys), and the account groups - plus the
    /// Favorites section, under its reserved sentinel key - they left
    /// collapsed (`AccountId` keys). The two sets `rebuild_folder_tree`'s
    /// session restore already carries, so a fresh session can re-seed the
    /// first build from them. Rows written by a future build that names a
    /// folder or account no longer present are simply never matched by the
    /// restore walk, so they are harmless rather than an error.
    pub fn load_folder_expansion(&self) -> rusqlite::Result<(HashSet<MailboxId>, HashSet<AccountId>)> {
        let mut stmt = self.conn.prepare("SELECT kind, key FROM folder_expansion_state")?;
        let rows = stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
        let mut expanded = HashSet::new();
        let mut collapsed = HashSet::new();
        for row in rows {
            let (kind, key) = row?;
            match kind.as_str() {
                EXPANDED_FOLDER => {
                    expanded.insert(MailboxId(key));
                }
                COLLAPSED_GROUP => {
                    collapsed.insert(AccountId(key));
                }
                _ => {}
            }
        }
        Ok((expanded, collapsed))
    }

    /// Replaces the folder pane's persisted expansion memory wholesale. The
    /// whole state is always known (the restore capture runs over the live
    /// tree), so replace semantics - wipe, then insert - keep the table
    /// exactly equal to the current session state with no diffing; writing an
    /// unchanged state back is a no-op row-wise. `expanded` names the
    /// subfolders whose rows are expanded; `collapsed` names the account
    /// groups (and the Favorites sentinel) left collapsed.
    pub fn set_folder_expansion(&self, expanded: &HashSet<MailboxId>, collapsed: &HashSet<AccountId>) -> rusqlite::Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM folder_expansion_state", [])?;
        {
            let mut stmt = tx.prepare("INSERT OR IGNORE INTO folder_expansion_state (kind, key) VALUES (?1, ?2)")?;
            for mailbox in expanded {
                stmt.execute(rusqlite::params![EXPANDED_FOLDER, mailbox.0])?;
            }
            for account in collapsed {
                stmt.execute(rusqlite::params![COLLAPSED_GROUP, account.0])?;
            }
        }
        tx.commit()
    }

    /// Every folder's stored display preferences.
    pub fn load_folder_prefs(&self) -> rusqlite::Result<HashMap<MailboxId, FolderPrefs>> {
        let mut stmt = self.conn.prepare("SELECT mailbox_id, color, count_mode FROM folder_prefs")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                MailboxId(row.get::<_, String>(0)?),
                FolderPrefs {
                    color: row.get::<_, Option<String>>(1)?,
                    count_mode: FolderCountMode::parse(&row.get::<_, String>(2)?),
                },
            ))
        })?;
        rows.collect()
    }

    /// Replaces the stored folder preferences wholesale with `prefs` (the
    /// session's in-memory map is the source of truth, as with the expansion
    /// state above). Default entries are skipped rather than stored.
    pub fn set_folder_prefs(&self, prefs: &HashMap<MailboxId, FolderPrefs>) -> rusqlite::Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM folder_prefs", [])?;
        {
            let mut stmt = tx.prepare("INSERT INTO folder_prefs (mailbox_id, color, count_mode) VALUES (?1, ?2, ?3)")?;
            for (mailbox, pref) in prefs.iter().filter(|(_, pref)| !pref.is_default()) {
                stmt.execute(rusqlite::params![mailbox.0, pref.color, pref.count_mode.as_str()])?;
            }
        }
        tx.commit()
    }

    /// Each account's recently used message-move/copy targets, most recent
    /// first - the message context menu's Move ▸ / Copy ▸ shortcuts (item
    /// 1.17). Persisted as a preference: the version wipe leaves it alone.
    pub fn load_recent_folders(&self) -> rusqlite::Result<HashMap<AccountId, Vec<MailboxId>>> {
        let mut stmt = self.conn.prepare("SELECT account, mailbox_id FROM recent_folders ORDER BY account, position")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut out: HashMap<AccountId, Vec<MailboxId>> = HashMap::new();
        for row in rows {
            let (account, mailbox) = row?;
            out.entry(AccountId(account)).or_default().push(MailboxId(mailbox));
        }
        Ok(out)
    }

    /// Replaces `account`'s recent-folder list with `folders` (most recent
    /// first); an empty list removes it.
    pub fn set_recent_folders(&self, account: &AccountId, folders: &[MailboxId]) -> rusqlite::Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM recent_folders WHERE account = ?1", [&account.0])?;
        for (position, mailbox) in folders.iter().enumerate() {
            tx.execute(
                "INSERT INTO recent_folders (account, position, mailbox_id) VALUES (?1, ?2, ?3)",
                rusqlite::params![account.0, position as i64, mailbox.0],
            )?;
        }
        tx.commit()
    }

    /// The recently searched queries (M-18, roadmap 2.3), newest first - shown
    /// in the search box's history popover. Persisted as a preference: the
    /// version wipe leaves it alone. There is one global list, not one per
    /// account.
    pub fn load_recent_searches(&self) -> rusqlite::Result<Vec<String>> {
        let mut stmt = self.conn.prepare("SELECT query FROM recent_searches ORDER BY position")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect()
    }

    /// Replaces the recent-search history wholesale with `queries` (newest
    /// first). The app caps the list before calling.
    pub fn set_recent_searches(&self, queries: &[String]) -> rusqlite::Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM recent_searches", [])?;
        {
            let mut stmt = tx.prepare("INSERT INTO recent_searches (position, query) VALUES (?1, ?2)")?;
            for (position, query) in queries.iter().enumerate() {
                stmt.execute(rusqlite::params![position as i64, query])?;
            }
        }
        tx.commit()
    }

    /// Prunes every folder-expansion row belonging to `account`: its account
    /// group row and every mailbox row under its id prefix. Called when an
    /// account is removed so the table can't accumulate stale rows for
    /// folders that no longer exist. A missing row is not an error.
    pub fn remove_account_folder_expansion(&self, account: &AccountId) -> rusqlite::Result<()> {
        self.conn.execute(
            "DELETE FROM folder_expansion_state WHERE (kind = ?1 AND key = ?2) OR (kind = ?3 AND key LIKE ?4 || ':%')",
            rusqlite::params![COLLAPSED_GROUP, account.0, EXPANDED_FOLDER, account.0],
        )?;
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

    /// One day's My Day pin set (item 4.9 / T-02): the `(calendar_id, uid)`
    /// pairs the user pinned for that date, in pin order. `day` is an ISO
    /// `YYYY-MM-DD` string in the display zone.
    pub fn load_my_day(&self, day: &str) -> rusqlite::Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare("SELECT calendar_id, uid FROM my_day_tasks WHERE day = ?1 ORDER BY rowid")?;
        let rows = stmt.query_map([day], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
        rows.collect()
    }

    /// Replaces one day's My Day pin set wholesale with `keys` - the in-memory
    /// toggle state is the source of truth, so a replace keeps the table equal
    /// to it with no diffing.
    pub fn set_my_day(&self, day: &str, keys: &[(String, String)]) -> rusqlite::Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM my_day_tasks WHERE day = ?1", [day])?;
        {
            let mut stmt = tx.prepare("INSERT OR IGNORE INTO my_day_tasks (day, calendar_id, uid) VALUES (?1, ?2, ?3)")?;
            for (calendar_id, uid) in keys {
                stmt.execute(rusqlite::params![day, calendar_id, uid])?;
            }
        }
        tx.commit()
    }

    /// Drops every My Day pin set older than `day` (an ISO date). Called on
    /// open so the table can't grow without bound as days pass; a missing row
    /// is not an error.
    pub fn prune_my_day(&self, day: &str) -> rusqlite::Result<()> {
        self.conn.execute("DELETE FROM my_day_tasks WHERE day < ?1", [day])?;
        Ok(())
    }
}

/// How long [`UiStateStore::flush`] waits for the writer to catch up before
/// giving up. The queue only ever holds a handful of small transactions, so
/// this is only reached when the disk itself is stuck.
const FLUSH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

type WriteOp = Box<dyn FnOnce(&UiStateDb) -> rusqlite::Result<()> + Send>;

enum Job {
    /// A write, and the warning to log (with the error) if it fails.
    Write { what: &'static str, op: WriteOp },
    /// Answered once every job queued before it has run.
    Flush(std::sync::mpsc::SyncSender<()>),
}

/// The UI-state database as the app holds it: a read connection the UI
/// thread queries directly, and a writer thread that applies every change in
/// the order it was made. Writes are fire-and-forget - a failure is logged on
/// the writer thread, as the call sites used to - so the GTK main loop never
/// waits on a commit, a WAL checkpoint or another connection's lock.
///
/// One instance is shared (`Rc<UiStateStore>`) by `UiState`, the calendar's
/// local tasks and My Day, and the reminder engine.
pub struct UiStateStore {
    reader: UiStateDb,
    tx: Option<std::sync::mpsc::Sender<Job>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl UiStateStore {
    /// Opens the read connection and the writer's connection (both here, on
    /// the calling thread, so they resolve the same path) and starts the
    /// writer thread. Either failing is an error; callers treat it as
    /// "nothing persists this session".
    pub fn open() -> Result<Self, Box<dyn std::error::Error>> {
        let reader = UiStateDb::open()?;
        let writer = UiStateDb::open()?;
        let (tx, rx) = std::sync::mpsc::channel::<Job>();
        let worker = std::thread::Builder::new().name("ui-state-db".into()).spawn(move || {
            for job in rx {
                match job {
                    Job::Write { what, op } => {
                        if let Err(e) = op(&writer) {
                            tracing::warn!("{what}: {e}");
                        }
                    }
                    Job::Flush(done) => {
                        let _ = done.send(());
                    }
                }
            }
        })?;
        Ok(UiStateStore {
            reader,
            tx: Some(tx),
            worker: Some(worker),
        })
    }

    /// The UI thread's read connection. Reads see every write the writer has
    /// committed; call [`Self::flush`] first if one was only just queued.
    pub fn reader(&self) -> &UiStateDb {
        &self.reader
    }

    /// Waits (up to [`FLUSH_TIMEOUT`]) until every write queued so far has
    /// been committed. Returns at once if the writer thread is gone.
    pub fn flush(&self) {
        let Some(tx) = &self.tx else { return };
        let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
        if tx.send(Job::Flush(done_tx)).is_err() {
            return;
        }
        if done_rx.recv_timeout(FLUSH_TIMEOUT).is_err() {
            tracing::warn!("UI-state writes still pending after {FLUSH_TIMEOUT:?}");
        }
    }

    fn queue(&self, what: &'static str, op: impl FnOnce(&UiStateDb) -> rusqlite::Result<()> + Send + 'static) {
        let sent = self.tx.as_ref().is_some_and(|tx| tx.send(Job::Write { what, op: Box::new(op) }).is_ok());
        if !sent {
            tracing::warn!("{what}: the UI-state writer has stopped");
        }
    }

    /// Queues [`UiStateDb::set_starred`].
    pub fn set_starred(&self, account: &AccountId, identity: &str, starred: bool) {
        let (account, identity) = (account.clone(), identity.to_string());
        self.queue("starred contact won't persist", move |db| db.set_starred(&account, &identity, starred));
    }

    /// Queues [`UiStateDb::set_trusted_sender`].
    pub fn set_trusted_sender(&self, account: &AccountId, entry: &str, level: lookout_core::TrustLevel) {
        let (account, entry) = (account.clone(), entry.to_string());
        self.queue("trusted sender won't persist", move |db| db.set_trusted_sender(&account, &entry, level));
    }

    /// Queues [`UiStateDb::remove_trusted_sender`].
    pub fn remove_trusted_sender(&self, account: &AccountId, entry: &str) {
        let (account, entry) = (account.clone(), entry.to_string());
        self.queue("trusted sender removal won't persist", move |db| db.remove_trusted_sender(&account, &entry));
    }

    /// Queues [`UiStateDb::set_blocked_sender`].
    pub fn set_blocked_sender(&self, account: &AccountId, entry: &str) {
        let (account, entry) = (account.clone(), entry.to_string());
        self.queue("blocked sender won't persist", move |db| db.set_blocked_sender(&account, &entry));
    }

    /// Queues [`UiStateDb::remove_blocked_sender`].
    pub fn remove_blocked_sender(&self, account: &AccountId, entry: &str) {
        let (account, entry) = (account.clone(), entry.to_string());
        self.queue("blocked sender removal won't persist", move |db| db.remove_blocked_sender(&account, &entry));
    }

    /// Queues [`UiStateDb::set_ignored_thread`].
    pub fn set_ignored_thread(&self, account: &AccountId, thread: &str, label: &str) {
        let (account, thread, label) = (account.clone(), thread.to_string(), label.to_string());
        self.queue("ignored conversation won't persist", move |db| db.set_ignored_thread(&account, &thread, &label));
    }

    /// Queues [`UiStateDb::remove_ignored_thread`].
    pub fn remove_ignored_thread(&self, account: &AccountId, thread: &str) {
        let (account, thread) = (account.clone(), thread.to_string());
        self.queue("ignored conversation removal won't persist", move |db| db.remove_ignored_thread(&account, &thread));
    }

    /// Queues [`UiStateDb::set_folder_expansion`].
    pub fn set_folder_expansion(&self, expanded: &HashSet<MailboxId>, collapsed: &HashSet<AccountId>) {
        let (expanded, collapsed) = (expanded.clone(), collapsed.clone());
        self.queue("folder expansion state won't persist", move |db| db.set_folder_expansion(&expanded, &collapsed));
    }

    /// Queues [`UiStateDb::set_folder_prefs`].
    pub fn set_folder_prefs(&self, prefs: &HashMap<MailboxId, FolderPrefs>) {
        let prefs = prefs.clone();
        self.queue("folder colours and count modes won't persist", move |db| db.set_folder_prefs(&prefs));
    }

    /// Queues [`UiStateDb::set_recent_folders`].
    pub fn set_recent_folders(&self, account: &AccountId, folders: &[MailboxId]) {
        let (account, folders) = (account.clone(), folders.to_vec());
        self.queue("recent folders won't persist", move |db| db.set_recent_folders(&account, &folders));
    }

    /// Queues [`UiStateDb::set_recent_searches`].
    pub fn set_recent_searches(&self, queries: &[String]) {
        let queries = queries.to_vec();
        self.queue("failed to persist recent searches", move |db| db.set_recent_searches(&queries));
    }

    /// Queues [`UiStateDb::remove_account_folder_expansion`].
    pub fn remove_account_folder_expansion(&self, account: &AccountId) {
        let account = account.clone();
        self.queue("failed to prune the removed account's folder expansion state", move |db| {
            db.remove_account_folder_expansion(&account)
        });
    }

    /// Queues [`UiStateDb::set_reminder_state`].
    pub fn set_reminder_state(&self, calendar_id: &str, uid: &str, start_utc: &str, state: &str, snooze_until_utc: Option<&str>) {
        let (calendar_id, uid, start_utc, state) = (calendar_id.to_string(), uid.to_string(), start_utc.to_string(), state.to_string());
        let snooze_until_utc = snooze_until_utc.map(str::to_string);
        self.queue("could not persist reminder state", move |db| {
            db.set_reminder_state(&calendar_id, &uid, &start_utc, &state, snooze_until_utc.as_deref())
        });
    }

    /// Queues [`UiStateDb::clear_reminder`].
    pub fn clear_reminder(&self, calendar_id: &str, uid: &str, start_utc: &str) {
        let (calendar_id, uid, start_utc) = (calendar_id.to_string(), uid.to_string(), start_utc.to_string());
        self.queue("could not clear reminder state", move |db| db.clear_reminder(&calendar_id, &uid, &start_utc));
    }

    /// Queues [`UiStateDb::save_local_task`].
    pub fn save_local_task(&self, task: &lookout_core::CalendarTask) {
        let task = task.clone();
        self.queue("local task won't persist", move |db| db.save_local_task(&task));
    }

    /// Queues [`UiStateDb::delete_local_task`].
    pub fn delete_local_task(&self, uid: &str) {
        let uid = uid.to_string();
        self.queue("local task deletion won't persist", move |db| db.delete_local_task(&uid));
    }

    /// Queues [`UiStateDb::set_my_day`].
    pub fn set_my_day(&self, day: &str, keys: &[(String, String)]) {
        let (day, keys) = (day.to_string(), keys.to_vec());
        self.queue("My Day pins won't persist", move |db| db.set_my_day(&day, &keys));
    }

    /// Queues [`UiStateDb::prune_my_day`].
    pub fn prune_my_day(&self, day: &str) {
        let day = day.to_string();
        self.queue("could not prune old My Day pins", move |db| db.prune_my_day(&day));
    }
}

impl Drop for UiStateStore {
    /// Closes the queue and waits for the writer to drain it, so a normal
    /// teardown never loses a queued write.
    fn drop(&mut self) {
        self.tx.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
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

    // The reserved key under which the folder pane's Favorites section is
    // recorded as a collapsed group - the same sentinel `window.rs`'s
    // `FAVORITES_GROUP_KEY` uses.
    const FAVORITES_SENTINEL_TEST: &str = "\u{0}favorites";

    // A single test owns `XDG_CACHE_HOME`: the env var is process-global and
    // parallel test threads would race over it otherwise (the same rule as
    // the `XDG_CONFIG_HOME` tests in `background_image`/`tags`).
    #[test]
    fn open_uses_wal_and_normal_sync() {
        let _guard = CACHE_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("lookout-ui-state-test-{}", std::process::id())).join("pragmas");
        std::env::set_var("XDG_CACHE_HOME", &dir);
        let _ = std::fs::remove_dir_all(&dir);

        let db = UiStateDb::open().expect("fresh database should open");
        let journal: String = db.conn.query_row("PRAGMA journal_mode", [], |r| r.get(0)).unwrap();
        let synchronous: i64 = db.conn.query_row("PRAGMA synchronous", [], |r| r.get(0)).unwrap();
        assert_eq!(journal, "wal");
        assert_eq!(synchronous, 1, "NORMAL: WAL commits without an fsync each");
    }

    #[test]
    fn store_writes_land_in_order_after_flush() {
        let _guard = CACHE_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("lookout-ui-state-test-{}", std::process::id())).join("store-order");
        std::env::set_var("XDG_CACHE_HOME", &dir);
        let _ = std::fs::remove_dir_all(&dir);

        let store = UiStateStore::open().expect("fresh store should open");
        let account = AccountId("acc".into());
        store.set_starred(&account, "ada", true);
        store.set_starred(&account, "grace", true);
        store.set_starred(&account, "ada", false);
        store.set_recent_folders(&account, &[MailboxId("acc:Old".into())]);
        store.set_recent_folders(&account, &[MailboxId("acc:New".into()), MailboxId("acc:Old".into())]);
        store.flush();

        let starred = store.reader().load_starred().unwrap();
        assert_eq!(starred[&account], HashSet::from(["grace".to_string()]), "the unstar queued after the star wins");
        assert_eq!(
            store.reader().load_recent_folders().unwrap()[&account],
            [MailboxId("acc:New".into()), MailboxId("acc:Old".into())],
            "the later wholesale replace wins"
        );
    }

    #[test]
    fn dropping_the_store_commits_queued_writes() {
        let _guard = CACHE_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("lookout-ui-state-test-{}", std::process::id())).join("store-drop");
        std::env::set_var("XDG_CACHE_HOME", &dir);
        let _ = std::fs::remove_dir_all(&dir);

        let store = UiStateStore::open().expect("fresh store should open");
        store.set_recent_searches(&["from:ada".to_string()]);
        drop(store);
        assert_eq!(UiStateDb::open().unwrap().load_recent_searches().unwrap(), ["from:ada"]);
    }

    #[test]
    fn folder_prefs_round_trip_and_skip_defaults() {
        let _guard = CACHE_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("lookout-ui-state-test-{}", std::process::id())).join("folder-prefs");
        std::env::set_var("XDG_CACHE_HOME", &dir);
        let _ = std::fs::remove_dir_all(&dir);

        let db = UiStateDb::open().expect("fresh database should open");
        assert!(db.load_folder_prefs().unwrap().is_empty());
        let colored = MailboxId("acc:Work".into());
        let total = MailboxId("acc:Archive".into());
        let plain = MailboxId("acc:Plain".into());
        let prefs = HashMap::from([
            (
                colored.clone(),
                FolderPrefs {
                    color: Some("#e01b24".into()),
                    count_mode: FolderCountMode::Unread,
                },
            ),
            (
                total.clone(),
                FolderPrefs {
                    color: None,
                    count_mode: FolderCountMode::Total,
                },
            ),
            (plain, FolderPrefs::default()),
        ]);
        db.set_folder_prefs(&prefs).unwrap();
        let loaded = UiStateDb::open().unwrap().load_folder_prefs().unwrap();
        assert_eq!(loaded.len(), 2, "a folder left at the defaults stores no row");
        assert_eq!(loaded[&colored].color.as_deref(), Some("#e01b24"));
        assert_eq!(loaded[&total].count_mode, FolderCountMode::Total);
        db.set_folder_prefs(&HashMap::new()).unwrap();
        assert!(db.load_folder_prefs().unwrap().is_empty(), "replace semantics");
    }

    #[test]
    fn ignored_threads_round_trip() {
        let _guard = CACHE_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("lookout-ui-state-test-{}", std::process::id())).join("ignored-threads");
        std::env::set_var("XDG_CACHE_HOME", &dir);
        let _ = std::fs::remove_dir_all(&dir);

        let db = UiStateDb::open().expect("fresh database should open");
        assert!(db.load_ignored_threads().unwrap().is_empty());
        let account = AccountId("acc".into());
        db.set_ignored_thread(&account, "root@x", "Project kickoff").unwrap();
        // Re-ignoring the same thread replaces its label (idempotent).
        db.set_ignored_thread(&account, "root@x", "Project kickoff v2").unwrap();
        db.set_ignored_thread(&account, "other@y", "Other thread").unwrap();

        let loaded = UiStateDb::open().unwrap().load_ignored_threads().unwrap();
        assert_eq!(loaded.len(), 2);
        assert!(loaded.contains(&(account.clone(), "root@x".into(), "Project kickoff v2".into())));

        db.remove_ignored_thread(&account, "root@x").unwrap();
        assert_eq!(UiStateDb::open().unwrap().load_ignored_threads().unwrap().len(), 1);
    }

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
            rrule: None,
            created: None,
            completed: None,
            status: lookout_core::TaskStatus::default(),
            priority: lookout_core::TaskPriority::default(),
            percent_complete: None,
            reminder_minutes_before: None,
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

    #[test]
    fn trusted_senders_round_trip_levels_and_wipe_survival() {
        let _guard = CACHE_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("lookout-ui-state-test-{}", std::process::id()));
        let dir = dir.join("trusted-senders");
        std::env::set_var("XDG_CACHE_HOME", &dir);
        let _ = std::fs::remove_dir_all(&dir);

        let db = UiStateDb::open().expect("fresh database should open");
        assert!(db.load_trusted_senders().unwrap().is_empty());

        let account = AccountId("/org/gnome/OnlineAccounts/Accounts/account_1".into());
        db.set_trusted_sender(&account, "ada@example.com", lookout_core::TrustLevel::Images).unwrap();
        // Re-setting the same entry with a higher level replaces the row
        // (INSERT OR REPLACE) rather than duplicating it.
        db.set_trusted_sender(&account, "ada@example.com", lookout_core::TrustLevel::AllContent).unwrap();
        db.set_trusted_sender(&account, "@news.example", lookout_core::TrustLevel::Images).unwrap();

        let loaded = db.load_trusted_senders().unwrap();
        assert_eq!(loaded.len(), 2);
        assert!(loaded.contains(&(account.clone(), "ada@example.com".to_string(), lookout_core::TrustLevel::AllContent)));
        assert!(loaded.contains(&(account.clone(), "@news.example".to_string(), lookout_core::TrustLevel::Images)));

        // Removal drops exactly that entry; a missing row is not an error.
        db.remove_trusted_sender(&account, "ada@example.com").unwrap();
        db.remove_trusted_sender(&account, "ada@example.com").unwrap();
        let loaded = db.load_trusted_senders().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].1, "@news.example");

        // A fresh session reads the same rows, and an unknown level encoding
        // degrades to the conservative Images level instead of failing.
        let reopened = UiStateDb::open().expect("existing database should reopen");
        assert_eq!(reopened.load_trusted_senders().unwrap(), loaded);
        reopened.conn.execute("UPDATE trusted_senders SET level = 99 WHERE entry = '@news.example'", []).unwrap();
        let loaded = reopened.load_trusted_senders().unwrap();
        assert_eq!(loaded[0].2, lookout_core::TrustLevel::Images);

        // Trust is a preference like favourites: the version-wipe path must
        // not touch it (only `starred_contacts` is wiped).
        reopened.conn.pragma_update(None, "user_version", 1).expect("test can write its own version");
        drop(reopened);
        let upgraded = UiStateDb::open().unwrap();
        let loaded = upgraded.load_trusted_senders().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].1, "@news.example");
        assert_eq!(loaded[0].2, lookout_core::TrustLevel::Images);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn blocked_senders_round_trip_and_wipe_survival() {
        let _guard = CACHE_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("lookout-ui-state-test-{}", std::process::id())).join("blocked-senders");
        std::env::set_var("XDG_CACHE_HOME", &dir);
        let _ = std::fs::remove_dir_all(&dir);

        let db = UiStateDb::open().expect("fresh database should open");
        assert!(db.load_blocked_senders().unwrap().is_empty());

        let account = AccountId("/org/gnome/OnlineAccounts/Accounts/account_1".into());
        db.set_blocked_sender(&account, "spammer@example.com").unwrap();
        // Re-adding the same entry is idempotent (INSERT OR REPLACE).
        db.set_blocked_sender(&account, "spammer@example.com").unwrap();
        db.set_blocked_sender(&account, "@ads.example").unwrap();

        let loaded = db.load_blocked_senders().unwrap();
        assert_eq!(loaded.len(), 2);
        assert!(loaded.contains(&(account.clone(), "spammer@example.com".to_string())));
        assert!(loaded.contains(&(account.clone(), "@ads.example".to_string())));

        // Removal drops exactly that entry; a missing row is not an error.
        db.remove_blocked_sender(&account, "spammer@example.com").unwrap();
        db.remove_blocked_sender(&account, "spammer@example.com").unwrap();
        let loaded = db.load_blocked_senders().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].1, "@ads.example");

        // Blocking is a preference: the version-wipe path must not touch it.
        let reopened = UiStateDb::open().expect("existing database should reopen");
        assert_eq!(reopened.load_blocked_senders().unwrap(), loaded);
        reopened.conn.pragma_update(None, "user_version", 1).expect("test can write its own version");
        drop(reopened);
        let upgraded = UiStateDb::open().unwrap();
        let loaded = upgraded.load_blocked_senders().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].1, "@ads.example");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recent_folders_round_trip_replace_and_wipe_survival() {
        let _guard = CACHE_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("lookout-ui-state-test-{}", std::process::id())).join("recent-folders");
        std::env::set_var("XDG_CACHE_HOME", &dir);
        let _ = std::fs::remove_dir_all(&dir);

        let db = UiStateDb::open().expect("fresh database should open");
        assert!(db.load_recent_folders().unwrap().is_empty());
        let work = AccountId("work".into());
        let home = AccountId("home".into());
        let projects = MailboxId("work:Projects".into());
        let receipts = MailboxId("work:Receipts".into());
        db.set_recent_folders(&work, &[receipts.clone(), projects.clone()]).unwrap();
        db.set_recent_folders(&home, &[MailboxId("home:Family".into())]).unwrap();
        let loaded = db.load_recent_folders().unwrap();
        assert_eq!(loaded[&work], vec![receipts.clone(), projects.clone()], "order is kept");
        assert_eq!(loaded[&home], vec![MailboxId("home:Family".into())]);

        // Replace semantics, per account; an empty list removes the account.
        db.set_recent_folders(&work, std::slice::from_ref(&projects)).unwrap();
        db.set_recent_folders(&home, &[]).unwrap();
        let loaded = db.load_recent_folders().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[&work], vec![projects.clone()]);

        // A preference: the version wipe leaves it alone.
        db.conn.pragma_update(None, "user_version", 1).expect("test can write its own version");
        drop(db);
        let upgraded = UiStateDb::open().unwrap();
        assert_eq!(upgraded.load_recent_folders().unwrap()[&work], vec![projects]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recent_searches_round_trip_replace_and_wipe_survival() {
        let _guard = CACHE_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("lookout-ui-state-test-{}", std::process::id())).join("recent-searches");
        std::env::set_var("XDG_CACHE_HOME", &dir);
        let _ = std::fs::remove_dir_all(&dir);

        let db = UiStateDb::open().expect("fresh database should open");
        assert!(db.load_recent_searches().unwrap().is_empty());
        db.set_recent_searches(&["from:ada".to_string(), "has:attachment report".to_string()]).unwrap();
        assert_eq!(db.load_recent_searches().unwrap(), vec!["from:ada".to_string(), "has:attachment report".to_string()]);

        // Replace semantics.
        db.set_recent_searches(&["is:unread".to_string()]).unwrap();
        assert_eq!(db.load_recent_searches().unwrap(), vec!["is:unread".to_string()]);

        // A preference: the version wipe leaves it alone.
        db.conn.pragma_update(None, "user_version", 1).expect("test can write its own version");
        drop(db);
        let upgraded = UiStateDb::open().unwrap();
        assert_eq!(upgraded.load_recent_searches().unwrap(), vec!["is:unread".to_string()]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn folder_expansion_round_trip_replace_and_wipe_survival() {
        let _guard = CACHE_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("lookout-ui-state-test-{}", std::process::id()));
        let dir = dir.join("folder-expansion");
        std::env::set_var("XDG_CACHE_HOME", &dir);
        let _ = std::fs::remove_dir_all(&dir);

        let db = UiStateDb::open().expect("fresh database should open");
        let (empty_expanded, empty_collapsed) = db.load_folder_expansion().unwrap();
        assert!(empty_expanded.is_empty());
        assert!(empty_collapsed.is_empty());

        let account_a = AccountId("/org/gnome/OnlineAccounts/Accounts/account_1".into());
        let account_b = AccountId("/org/gnome/OnlineAccounts/Accounts/account_2".into());
        let expanded: HashSet<MailboxId> = HashSet::from([MailboxId(format!("{}:Projects", account_a.0)), MailboxId(format!("{}:Projects/Active", account_a.0))]);
        let collapsed: HashSet<AccountId> = HashSet::from([account_b.clone(), AccountId(FAVORITES_SENTINEL_TEST.into())]);
        db.set_folder_expansion(&expanded, &collapsed).unwrap();

        let (loaded_expanded, loaded_collapsed) = db.load_folder_expansion().unwrap();
        assert_eq!(loaded_expanded, expanded);
        assert_eq!(loaded_collapsed, collapsed);

        // Replace semantics: a smaller write drops the rows the new state
        // doesn't name rather than accumulating them.
        let fewer_expanded: HashSet<MailboxId> = HashSet::from([MailboxId(format!("{}:Projects", account_a.0))]);
        db.set_folder_expansion(&fewer_expanded, &HashSet::new()).unwrap();
        let (loaded_expanded, loaded_collapsed) = db.load_folder_expansion().unwrap();
        assert_eq!(loaded_expanded, fewer_expanded);
        assert!(loaded_collapsed.is_empty());

        // A fresh session (restart) reads the same rows.
        let reopened = UiStateDb::open().expect("existing database should reopen");
        assert_eq!(reopened.load_folder_expansion().unwrap(), (fewer_expanded.clone(), HashSet::new()));

        // Re-adding both sets, then pruning one account removes exactly its
        // group row and the mailbox rows under its id prefix.
        reopened.set_folder_expansion(&expanded, &collapsed).unwrap();
        reopened.remove_account_folder_expansion(&account_a).unwrap();
        let (loaded_expanded, loaded_collapsed) = reopened.load_folder_expansion().unwrap();
        assert!(loaded_expanded.is_empty(), "account A's mailbox rows are pruned");
        assert_eq!(loaded_collapsed, HashSet::from([account_b.clone(), AccountId(FAVORITES_SENTINEL_TEST.into())]));
        reopened.remove_account_folder_expansion(&account_b).unwrap();
        let (loaded_expanded, loaded_collapsed) = reopened.load_folder_expansion().unwrap();
        assert!(loaded_expanded.is_empty());
        assert_eq!(
            loaded_collapsed,
            HashSet::from([AccountId(FAVORITES_SENTINEL_TEST.into())]),
            "the Favorites sentinel survives account pruning"
        );

        // Expansion memory is a preference like favourites: the version-wipe
        // path must not touch it (only `starred_contacts` is wiped).
        reopened.set_folder_expansion(&fewer_expanded, &HashSet::new()).unwrap();
        reopened.conn.pragma_update(None, "user_version", 1).expect("test can write its own version");
        drop(reopened);
        let upgraded = UiStateDb::open().unwrap();
        let (loaded_expanded, loaded_collapsed) = upgraded.load_folder_expansion().unwrap();
        assert_eq!(loaded_expanded, fewer_expanded);
        assert!(loaded_collapsed.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn my_day_round_trip_replace_prune_and_wipe_survival() {
        let _guard = CACHE_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("lookout-ui-state-test-{}", std::process::id())).join("my-day");
        std::env::set_var("XDG_CACHE_HOME", &dir);
        let _ = std::fs::remove_dir_all(&dir);

        let db = UiStateDb::open().expect("fresh database should open");
        assert!(db.load_my_day("2026-10-16").unwrap().is_empty());

        db.set_my_day("2026-10-16", &[("local".into(), "t1".into()), ("todo:l1".into(), "t2".into())]).unwrap();
        db.set_my_day("2026-10-15", &[("local".into(), "old".into())]).unwrap();
        let today = db.load_my_day("2026-10-16").unwrap();
        assert_eq!(today, vec![("local".into(), "t1".into()), ("todo:l1".into(), "t2".into())], "pin order is kept");

        // Replace semantics: a smaller set drops the missing pins.
        db.set_my_day("2026-10-16", &[("local".into(), "t1".into())]).unwrap();
        assert_eq!(db.load_my_day("2026-10-16").unwrap(), vec![("local".into(), "t1".into())]);

        // Pruning drops older days but leaves today and later.
        db.set_my_day("2026-10-20", &[("local".into(), "future".into())]).unwrap();
        db.prune_my_day("2026-10-16").unwrap();
        assert!(db.load_my_day("2026-10-15").unwrap().is_empty(), "yesterday is pruned");
        assert_eq!(db.load_my_day("2026-10-16").unwrap().len(), 1);
        assert_eq!(db.load_my_day("2026-10-20").unwrap().len(), 1);

        // A preference-like table: the version wipe must not touch it.
        let reopened = UiStateDb::open().unwrap();
        reopened.conn.pragma_update(None, "user_version", 1).expect("test can write its own version");
        drop(reopened);
        let upgraded = UiStateDb::open().unwrap();
        assert_eq!(upgraded.load_my_day("2026-10-16").unwrap(), vec![("local".into(), "t1".into())]);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
