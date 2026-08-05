use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use lookout_core::{AccountId, ContactsProvider, EmailSummary, EmailAddress, Mailbox, MailboxId, SystemFlagBit, Uid, UidValidity};
use rusqlite::Connection;

use crate::error::Result;

/// A per-account local SQLite cache of mailbox/message metadata, used for a
/// fast first paint on startup before the live IMAP fetch completes. Never
/// the source of truth: every value here is superseded by the next
/// `FoldersUpdated`/`MessagesUpdated` event from the live session.
///
/// Since the body of every opened message used to be re-fetched from the
/// server on each open (switching emails - even back to one already read -
/// paid a full IMAP round trip), fetched raw message bytes are also cached
/// here (the `bodies` table) so an already-viewed message renders without
/// touching the network. Attachment bytes are cached along with the body for
/// free; nothing fetches them separately yet.
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

/// Returns the cache directory and a list of `(filename, size_bytes)` for each
/// SQLite database file in it. Used by the config view to show per-file storage
/// breakdowns.
pub fn cache_info() -> (std::path::PathBuf, Vec<(String, u64)>) {
    let dir = cache_dir();
    let entries = if dir.exists() {
        std::fs::read_dir(&dir)
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|ext| ext == "sqlite3"))
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                let size = e.metadata().ok()?.len();
                Some((name, size))
            })
            .collect()
    } else {
        Vec::new()
    };
    (dir, entries)
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
        // WAL + a busy timeout because this file now has two readers: the
        // account session writing synced envelopes, and the UI thread
        // querying `addresses` for composer autocomplete. Under the default
        // rollback journal those collide as `SQLITE_BUSY`; WAL lets a reader
        // proceed against the last committed snapshot while a write is in
        // flight. `journal_mode` is persistent (stored in the file header),
        // so this also upgrades databases created before it was set.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
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
            CREATE TABLE IF NOT EXISTS bodies (
                mailbox_id TEXT NOT NULL,
                uid INTEGER NOT NULL,
                uidvalidity INTEGER NOT NULL,
                data BLOB NOT NULL,
                PRIMARY KEY (mailbox_id, uid)
            );
            CREATE INDEX IF NOT EXISTS bodies_by_mailbox ON bodies (mailbox_id);
            CREATE TABLE IF NOT EXISTS addresses (
                address TEXT PRIMARY KEY,
                name TEXT,
                seen_count INTEGER NOT NULL,
                last_seen INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS addresses_by_count ON addresses (seen_count DESC);
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

    /// Returns `true` if the cache holds any message summaries for `mailbox_id`,
    /// without deserializing them. Used by the session actor to skip a
    /// redundant IMAP sync when the data is already cached.
    pub fn has_messages(&self, mailbox_id: &MailboxId) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT 1 FROM messages WHERE mailbox_id = ?1 LIMIT 1")?;
        let mut rows = stmt.query_map([&mailbox_id.0], |_| Ok(()))?;
        Ok(rows.next().is_some())
    }

    /// Returns the raw RFC 5322 bytes of a previously-fetched message body,
    /// or `None` if it isn't cached. `uidvalidity` guards the cache against
    /// serving a body for a recycled uid after its mailbox was re-created
    /// (RFC 3501 §2.3.1.1): a row written under a different uidvalidity is a
    /// miss, never a stale body. Callers re-parse the bytes with
    /// `parse_body`, which is cheap relative to the IMAP fetch this avoids.
    pub fn load_body(&self, mailbox_id: &MailboxId, uid: Uid, uidvalidity: UidValidity) -> Result<Option<Vec<u8>>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT data FROM bodies WHERE mailbox_id = ?1 AND uid = ?2 AND uidvalidity = ?3")?;
        let mut rows = stmt.query_map(rusqlite::params![mailbox_id.0, uid.0, uidvalidity.0], |row| row.get::<_, Vec<u8>>(0))?;
        match rows.next() {
            Some(Ok(raw)) => Ok(Some(raw)),
            Some(Err(e)) => Err(e.into()),
            None => Ok(None),
        }
    }

    /// Stores the raw bytes of a fetched message body for `uid`, replacing
    /// any earlier body for the same `(mailbox, uid)`.
    pub fn store_body(&self, mailbox_id: &MailboxId, uid: Uid, uidvalidity: UidValidity, raw: &[u8]) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO bodies (mailbox_id, uid, uidvalidity, data) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![mailbox_id.0, uid.0, uidvalidity.0, raw],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Returns `true` if a body for `(mailbox_id, uid, uidvalidity)` exists
    /// in the on-disk cache, without loading the (potentially large) payload.
    pub fn has_body(&self, mailbox_id: &MailboxId, uid: Uid, uidvalidity: UidValidity) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT 1 FROM bodies WHERE mailbox_id = ?1 AND uid = ?2 AND uidvalidity = ?3")?;
        let mut rows = stmt.query_map(rusqlite::params![mailbox_id.0, uid.0, uidvalidity.0], |_| Ok(()))?;
        Ok(rows.next().is_some())
    }

    /// The list-row snippets already cached for `mailbox_id`, keyed by uid.
    ///
    /// Previews cost a body fetch, but `sync_mailbox` re-fetches its whole
    /// envelope window on every IDLE wake and `replace_messages` wipes the
    /// mailbox's rows each time. Reading them back before that wipe is what
    /// makes a preview stick: without it every resync would blank every
    /// snippet and then re-fetch the lot.
    pub fn load_previews(&self, mailbox_id: &MailboxId) -> Result<HashMap<Uid, String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT data FROM messages WHERE mailbox_id = ?1")?;
        let rows = stmt.query_map([&mailbox_id.0], |row| row.get::<_, String>(0))?;
        let mut previews = HashMap::new();
        for row in rows {
            // Rows cached before previews existed simply deserialize with
            // `preview: None` and get backfilled by the next sync.
            if let Ok(msg) = serde_json::from_str::<EmailSummary>(&row?) {
                if let Some(preview) = msg.preview {
                    previews.insert(msg.uid, preview);
                }
            }
        }
        Ok(previews)
    }

    /// Harvests every `From`/`To`/`Cc` address in `messages` into the address
    /// book the composer's recipient autocomplete reads from. Called on each
    /// sync, so the suggestions grow with whatever mail has actually been
    /// seen - there is no contacts source to draw on until Phase 4's CardDAV
    /// work lands.
    ///
    /// Addresses are keyed lowercased (the same person shouldn't appear twice
    /// for a capitalisation difference) while `name` keeps the first
    /// non-empty display name seen, so a later envelope that carries only a
    /// bare address doesn't erase a name already learned.
    pub fn record_addresses(&self, messages: &[EmailSummary]) -> Result<()> {
        let now = Utc::now().timestamp();
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO addresses (address, name, seen_count, last_seen) VALUES (?1, ?2, 1, ?3)
                 ON CONFLICT(address) DO UPDATE SET
                     seen_count = seen_count + 1,
                     last_seen = ?3,
                     name = COALESCE(NULLIF(name, ''), ?2)",
            )?;
            for msg in messages {
                for addr in msg.from.iter().chain(&msg.to).chain(&msg.cc) {
                    let address = addr.address.trim().to_lowercase();
                    if address.is_empty() {
                        continue;
                    }
                    let name = addr.name.as_deref().unwrap_or("").trim();
                    stmt.execute(rusqlite::params![address, name, now])?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Address-book completions for `prefix`, most-corresponded-with first.
    /// Matches against the address and the display name both, since "gav" and
    /// "Gavin" should each find the same person. An empty prefix returns the
    /// top entries outright, which is what makes a freshly focused recipient
    /// field able to offer anything at all.
    pub fn search_addresses(&self, prefix: &str, limit: usize) -> Result<Vec<lookout_core::EmailAddress>> {
        let conn = self.conn.lock().unwrap();
        // `escape` so a user typing `%` or `_` searches for those characters
        // rather than LIKE's wildcards.
        let pattern = format!("{}%", prefix.trim().to_lowercase().replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
        let mut stmt = conn.prepare(
            "SELECT address, name FROM addresses
             WHERE address LIKE ?1 ESCAPE '\\' OR lower(name) LIKE ?1 ESCAPE '\\'
             ORDER BY seen_count DESC, last_seen DESC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![pattern, limit as i64], |row| {
            Ok(lookout_core::EmailAddress {
                address: row.get::<_, String>(0)?,
                name: row.get::<_, Option<String>>(1)?.filter(|n| !n.trim().is_empty()),
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Applies a flag change to one cached summary, mirroring the `STORE`
    /// the session just issued against the server. Returns `false` if the
    /// message isn't in the cached window (nothing to update).
    ///
    /// The cached row is patched in place rather than re-fetched: a
    /// mark-as-read is a single-uid change the client already knows the
    /// outcome of, and the next `sync_mailbox` overwrites the whole window
    /// from the server anyway - so this only has to hold until then.
    pub fn update_flags(&self, mailbox_id: &MailboxId, uid: Uid, add: &[SystemFlagBit], remove: &[SystemFlagBit]) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT data FROM messages WHERE mailbox_id = ?1 AND uid = ?2")?;
        let mut rows = stmt.query_map(rusqlite::params![mailbox_id.0, uid.0], |row| row.get::<_, String>(0))?;
        let Some(data) = rows.next().transpose()? else {
            return Ok(false);
        };
        let Ok(mut summary) = serde_json::from_str::<EmailSummary>(&data) else {
            return Ok(false);
        };
        for flag in add {
            summary.flags.insert(*flag);
        }
        for flag in remove {
            summary.flags.remove(flag);
        }
        let data = serde_json::to_string(&summary)?;
        drop(rows);
        drop(stmt);
        conn.execute(
            "UPDATE messages SET data = ?1 WHERE mailbox_id = ?2 AND uid = ?3",
            rusqlite::params![data, mailbox_id.0, uid.0],
        )?;
        Ok(true)
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

impl ContactsProvider for Cache {
    fn search_contacts(&self, prefix: &str, limit: usize) -> Vec<EmailAddress> {
        self.search_addresses(prefix, limit).unwrap_or_default()
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

    fn sample_summary(mailbox_id: &MailboxId, uid: u32, preview: Option<&str>) -> EmailSummary {
        EmailSummary {
            uid: Uid(uid),
            mailbox: mailbox_id.clone(),
            message_id: None,
            in_reply_to: None,
            references: Vec::new(),
            thread_key: lookout_core::ThreadKey(String::new()),
            subject: Some(format!("subject {uid}")),
            from: Vec::new(),
            to: Vec::new(),
            cc: Vec::new(),
            date: Utc::now(),
            flags: Default::default(),
            keywords: Default::default(),
            size: 0,
            has_attachment: false,
            preview: preview.map(|p| p.to_string()),
        }
    }

    /// The stickiness `sync_mailbox` depends on: a preview written once is
    /// readable back after the wholesale `replace_messages` wipe, so a resync
    /// doesn't blank every snippet and re-fetch them all.
    #[test]
    fn round_trips_previews_and_omits_messages_without_one() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let mailbox_id = MailboxId::new(&account_id, "INBOX");

        let messages = vec![
            sample_summary(&mailbox_id, 1, Some("Truffle Security Co. says it scanned...")),
            sample_summary(&mailbox_id, 2, None),
        ];
        cache.replace_messages(&mailbox_id, UidValidity(1), &messages).unwrap();

        let previews = cache.load_previews(&mailbox_id).unwrap();
        assert_eq!(previews.len(), 1);
        assert_eq!(previews.get(&Uid(1)).map(String::as_str), Some("Truffle Security Co. says it scanned..."));
        assert!(!previews.contains_key(&Uid(2)));

        let path = cache_dir().join(format!("{}.sqlite3", sanitize_filename(&account_id)));
        let _ = std::fs::remove_file(path);
    }

    /// What makes a mark-as-read stick between the `STORE` and the next full
    /// sync: the patched row must survive a reload, and a uid outside the
    /// cached window must report "nothing updated" rather than erroring.
    #[test]
    fn patches_flags_on_a_cached_summary() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let mailbox_id = MailboxId::new(&account_id, "INBOX");

        cache.replace_messages(&mailbox_id, UidValidity(1), &[sample_summary(&mailbox_id, 1, None)]).unwrap();
        assert!(cache.load_messages(&mailbox_id).unwrap()[0].is_unread());

        assert!(cache.update_flags(&mailbox_id, Uid(1), &[SystemFlagBit::Seen, SystemFlagBit::Flagged], &[]).unwrap());
        let loaded = &cache.load_messages(&mailbox_id).unwrap()[0];
        assert!(!loaded.is_unread());
        assert!(loaded.is_starred());

        assert!(cache.update_flags(&mailbox_id, Uid(1), &[], &[SystemFlagBit::Flagged]).unwrap());
        let loaded = &cache.load_messages(&mailbox_id).unwrap()[0];
        assert!(!loaded.is_unread());
        assert!(!loaded.is_starred());

        // A uid that isn't in the cached window is a no-op, not an error.
        assert!(!cache.update_flags(&mailbox_id, Uid(99), &[SystemFlagBit::Seen], &[]).unwrap());

        let path = cache_dir().join(format!("{}.sqlite3", sanitize_filename(&account_id)));
        let _ = std::fs::remove_file(path);
    }

    /// The composer's autocomplete contract: addresses accumulate across
    /// syncs, repeat correspondents rank first, and a prefix matches the
    /// display name as readily as the address.
    #[test]
    fn records_and_searches_the_address_book() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let mailbox_id = MailboxId::new(&account_id, "INBOX");

        let addressed = |uid: u32, from: (&str, Option<&str>), to: &[&str]| {
            let mut msg = sample_summary(&mailbox_id, uid, None);
            msg.from = vec![lookout_core::EmailAddress {
                name: from.1.map(str::to_string),
                address: from.0.to_string(),
            }];
            msg.to = to.iter().map(|a| lookout_core::EmailAddress::new(*a)).collect();
            msg
        };

        cache
            .record_addresses(&[
                addressed(1, ("Ada@Example.com", Some("Ada Lovelace")), &["bob@example.com"]),
                addressed(2, ("ada@example.com", None), &[]),
                addressed(3, ("carol@elsewhere.org", None), &[]),
            ])
            .unwrap();

        // Case-folded to one entry, seen twice, and the name learned from the
        // first envelope survives the second (which carried none).
        let ada = cache.search_addresses("ada", 10).unwrap();
        assert_eq!(ada.len(), 1);
        assert_eq!(ada[0].address, "ada@example.com");
        assert_eq!(ada[0].name.as_deref(), Some("Ada Lovelace"));

        // A prefix matches the display name too, not just the address.
        let by_name = cache.search_addresses("lovel", 10).unwrap();
        assert!(by_name.is_empty(), "prefix match is anchored at the start of the name");
        assert_eq!(cache.search_addresses("ada l", 10).unwrap().len(), 1);

        // Ranked by how often each correspondent appears.
        let all = cache.search_addresses("", 10).unwrap();
        assert_eq!(all.first().map(|a| a.address.as_str()), Some("ada@example.com"));
        assert_eq!(all.len(), 3);
        assert_eq!(cache.search_addresses("", 2).unwrap().len(), 2);

        // A later sync adds to the book rather than replacing it.
        cache.record_addresses(&[addressed(4, ("dave@example.com", None), &[])]).unwrap();
        assert_eq!(cache.search_addresses("", 10).unwrap().len(), 4);

        // LIKE metacharacters are searched for literally, not as wildcards.
        assert!(cache.search_addresses("%", 10).unwrap().is_empty());

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

    #[test]
    fn round_trips_message_bodies_through_the_cache() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let mailbox_id = MailboxId::new(&account_id, "INBOX");
        let raw = b"From: a@b.c\r\n\r\nHello".to_vec();

        // A never-fetched body is a miss.
        assert!(cache.load_body(&mailbox_id, Uid(7), UidValidity(3)).unwrap().is_none());

        cache.store_body(&mailbox_id, Uid(7), UidValidity(3), &raw).unwrap();
        assert_eq!(cache.load_body(&mailbox_id, Uid(7), UidValidity(3)).unwrap(), Some(raw));

        // A mailbox that was re-created (uidvalidity changed) reuses uids; a
        // stale row must be a miss, not a wrong body.
        assert!(cache.load_body(&mailbox_id, Uid(7), UidValidity(4)).unwrap().is_none());

        // Re-storing the same (mailbox, uid) replaces the bytes.
        cache.store_body(&mailbox_id, Uid(7), UidValidity(3), b"updated".as_slice()).unwrap();
        assert_eq!(cache.load_body(&mailbox_id, Uid(7), UidValidity(3)).unwrap(), Some(b"updated".to_vec()));

        let path = cache_dir().join(format!("{}.sqlite3", sanitize_filename(&account_id)));
        let _ = std::fs::remove_file(path);
    }
}
