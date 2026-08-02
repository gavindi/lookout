use std::collections::HashSet;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use lookout_core::{AccountId, EmailSummary, Mailbox, MailboxId, Uid, UidValidity};
use rusqlite::Connection;

use crate::error::Result;

/// A per-account local SQLite cache of mailbox/message metadata, used for a
/// fast first paint on startup before the live IMAP fetch completes. Never
/// the source of truth: every value here is superseded by the next
/// `FoldersUpdated`/`MessagesUpdated` event from the live session. Full
/// bodies/attachments are deliberately not cached here (Phase 1 fetches
/// bodies on demand); a flat-file `.eml` cache is a Phase 2 addition.
///
/// The connection is `Mutex`-wrapped purely to make `Cache: Sync` (and so
/// `&Cache: Send`) - `rusqlite::Connection` itself isn't `Sync` because its
/// internal statement cache uses a `RefCell`, which otherwise poisons the
/// `Send`-ness of the whole `run_account_session` future wherever a `&Cache`
/// is held across an `.await` point. There's never actual cross-thread
/// contention (one account session owns its cache), so the lock is
/// uncontended in practice.
pub struct Cache {
    conn: Mutex<Connection>,
}

fn cache_dir() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("lookout").join("mail")
}

/// Removes every cached per-account SQLite database under
/// `$XDG_CACHE_HOME/lookout/mail/` so the next session starts fresh from the
/// server. Safe to call while account sessions are live: each session keeps
/// its own already-open connection (POSIX unlink doesn't disturb an open fd),
/// and the cache is only a fast-first-paint hint anyway - so only the on-disk
/// files are dropped, and the in-memory/live data keeps working as-is.
pub fn clear_all_caches() -> Result<()> {
    let dir = cache_dir();
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    Ok(())
}

/// GOA account ids are D-Bus object paths (e.g.
/// `/org/gnome/OnlineAccounts/Accounts/account_1234`); sanitize into a bare
/// filename.
fn sanitize_filename(account_id: &AccountId) -> String {
    account_id
        .0
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

impl Cache {
    /// Opens (creating if needed) the cache database for `account_id` under
    /// `$XDG_CACHE_HOME/lookout/mail/`.
    pub fn open(account_id: &AccountId) -> Result<Self> {
        let dir = cache_dir();
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}.sqlite3", sanitize_filename(account_id)));
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS mailboxes (
                mailbox_id TEXT PRIMARY KEY,
                account_id TEXT NOT NULL,
                data TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS messages (
                mailbox_id TEXT NOT NULL,
                uid INTEGER NOT NULL,
                uidvalidity INTEGER NOT NULL,
                data TEXT NOT NULL,
                PRIMARY KEY (mailbox_id, uid)
            );
            CREATE INDEX IF NOT EXISTS messages_by_mailbox ON messages (mailbox_id);
            CREATE TABLE IF NOT EXISTS snoozed (
                mailbox_id TEXT NOT NULL,
                uid INTEGER NOT NULL,
                snoozed_until INTEGER NOT NULL,
                PRIMARY KEY (mailbox_id, uid)
            );
            ",
        )?;
        Ok(Cache { conn: Mutex::new(conn) })
    }

    /// Replaces the full cached mailbox list for this account (the account's
    /// folder list is small and always fetched in full, so a wholesale
    /// replace is simpler and cheap compared to diffing).
    pub fn replace_mailboxes(&self, account_id: &AccountId, mailboxes: &[Mailbox]) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM mailboxes WHERE account_id = ?1", [&account_id.0])?;
        for mailbox in mailboxes {
            let data = serde_json::to_string(mailbox)?;
            tx.execute(
                "INSERT INTO mailboxes (mailbox_id, account_id, data) VALUES (?1, ?2, ?3)",
                rusqlite::params![mailbox.id.0, account_id.0, data],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn load_mailboxes(&self, account_id: &AccountId) -> Result<Vec<Mailbox>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT data FROM mailboxes WHERE account_id = ?1")?;
        let rows = stmt.query_map([&account_id.0], |row| row.get::<_, String>(0))?;
        let mut mailboxes = Vec::new();
        for row in rows {
            if let Ok(mailbox) = serde_json::from_str::<Mailbox>(&row?) {
                mailboxes.push(mailbox);
            }
        }
        Ok(mailboxes)
    }

    /// Replaces the cached envelope window for one mailbox. Like
    /// `replace_mailboxes`, this mirrors `sync_mailbox`'s own
    /// bounded-re-fetch-not-diff strategy (see that function's doc comment)
    /// rather than trying to merge incrementally.
    pub fn replace_messages(&self, mailbox_id: &MailboxId, uidvalidity: UidValidity, messages: &[EmailSummary]) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM messages WHERE mailbox_id = ?1", [&mailbox_id.0])?;
        for msg in messages {
            let data = serde_json::to_string(msg)?;
            tx.execute(
                "INSERT INTO messages (mailbox_id, uid, uidvalidity, data) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![mailbox_id.0, msg.uid.0, uidvalidity.0, data],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn load_messages(&self, mailbox_id: &MailboxId) -> Result<Vec<EmailSummary>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT data FROM messages WHERE mailbox_id = ?1")?;
        let rows = stmt.query_map([&mailbox_id.0], |row| row.get::<_, String>(0))?;
        let mut messages = Vec::new();
        for row in rows {
            if let Ok(msg) = serde_json::from_str::<EmailSummary>(&row?) {
                messages.push(msg);
            }
        }
        Ok(messages)
    }

    /// Records that `uid` (in `mailbox_id`) should be hidden from
    /// `MessagesUpdated` until `until` - purely client-side state, IMAP has
    /// no native snooze concept. `INSERT OR REPLACE` so re-snoozing an
    /// already-snoozed message just updates its wake time.
    pub fn snooze_message(&self, mailbox_id: &MailboxId, uid: Uid, until: DateTime<Utc>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO snoozed (mailbox_id, uid, snoozed_until) VALUES (?1, ?2, ?3)",
            rusqlite::params![mailbox_id.0, uid.0, until.timestamp()],
        )?;
        Ok(())
    }

    /// Returns every uid in `mailbox_id` still snoozed as of `now`, having
    /// first opportunistically deleted rows whose snooze time has already
    /// passed (cheap cleanup piggybacked on the read every caller already
    /// does before building `MessagesUpdated`).
    pub fn active_snoozed_uids(&self, mailbox_id: &MailboxId, now: DateTime<Utc>) -> Result<HashSet<Uid>> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM snoozed WHERE snoozed_until <= ?1", rusqlite::params![now.timestamp()])?;
        let mut stmt = conn.prepare("SELECT uid FROM snoozed WHERE mailbox_id = ?1")?;
        let rows = stmt.query_map([&mailbox_id.0], |row| row.get::<_, u32>(0))?;
        let mut uids = HashSet::new();
        for row in rows {
            uids.insert(Uid(row?));
        }
        Ok(uids)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lookout_core::MailboxRole;

    fn temp_account_id() -> AccountId {
        AccountId(format!("/test/cache_{}", uuid::Uuid::new_v4()))
    }

    fn sample_mailbox(account_id: &AccountId, name: &str) -> Mailbox {
        Mailbox {
            id: MailboxId::new(account_id, name),
            account_id: account_id.clone(),
            name: name.to_string(),
            parent: None,
            delimiter: '/',
            role: MailboxRole::Custom,
            uidvalidity: UidValidity(1),
            uidnext: 1,
            highest_modseq: None,
            total: 0,
            unread: 0,
            flags: vec![],
            subscribed: true,
        }
    }

    #[test]
    fn round_trips_mailboxes_through_the_cache() {
        // Uses a unique account id (and therefore a unique sqlite file
        // under the real XDG cache dir) so parallel test runs don't collide;
        // this is acceptable for a fast, disk-backed unit test and mirrors
        // how the cache is actually keyed in production.
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let mailboxes = vec![sample_mailbox(&account_id, "INBOX"), sample_mailbox(&account_id, "Archive")];

        cache.replace_mailboxes(&account_id, &mailboxes).unwrap();
        let loaded = cache.load_mailboxes(&account_id).unwrap();
        assert_eq!(loaded.len(), 2);
        assert!(loaded.iter().any(|m| m.name == "INBOX"));
        assert!(loaded.iter().any(|m| m.name == "Archive"));

        // Replacing again should wholesale-replace, not accumulate.
        cache.replace_mailboxes(&account_id, &mailboxes[..1]).unwrap();
        assert_eq!(cache.load_mailboxes(&account_id).unwrap().len(), 1);

        let path = cache_dir().join(format!("{}.sqlite3", sanitize_filename(&account_id)));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn round_trips_snooze_state_and_excludes_expired_entries() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let mailbox_id = MailboxId::new(&account_id, "INBOX");
        let now = Utc::now();

        cache.snooze_message(&mailbox_id, Uid(1), now + chrono::Duration::hours(1)).unwrap();
        cache.snooze_message(&mailbox_id, Uid(2), now - chrono::Duration::hours(1)).unwrap();

        let active = cache.active_snoozed_uids(&mailbox_id, now).unwrap();
        assert_eq!(active, HashSet::from([Uid(1)]));

        // Re-snoozing an already-snoozed message updates its wake time
        // rather than erroring or duplicating the row.
        cache.snooze_message(&mailbox_id, Uid(1), now - chrono::Duration::hours(1)).unwrap();
        assert!(cache.active_snoozed_uids(&mailbox_id, now).unwrap().is_empty());

        let path = cache_dir().join(format!("{}.sqlite3", sanitize_filename(&account_id)));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn sanitizes_dbus_object_paths_into_bare_filenames() {
        let id = AccountId("/org/gnome/OnlineAccounts/Accounts/account_1234".to_string());
        let sanitized = sanitize_filename(&id);
        assert!(!sanitized.contains('/'));
        assert_eq!(sanitized, "_org_gnome_OnlineAccounts_Accounts_account_1234");
    }
}
