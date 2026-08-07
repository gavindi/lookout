use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use lookout_core::{AccountId, ContactsProvider, EmailAddress, EmailBody, EmailSummary, Mailbox, MailboxId, SystemFlagBit, Uid, UidValidity};
use rusqlite::Connection;

use crate::error::Result;

/// A per-account local SQLite cache of mailbox/message metadata, used for a
/// fast first paint on startup before the live IMAP fetch completes. Never
/// the source of truth: every value here is superseded by the next
/// `FoldersUpdated`/`MessagesUpdated` event from the live session.
///
/// Since the body of every opened message used to be re-fetched from the
/// server on each open (switching emails - even back to one already read -
/// paid a full IMAP round trip), fetched message bodies are also cached here
/// (the `bodies` table) so an already-viewed message renders without touching
/// the network. Rows hold the assembled [`EmailBody`] as JSON - what the
/// viewer actually consumes - not raw RFC 5322 bytes, because the
/// BODYSTRUCTURE-driven partial-fetch path (the normal one) downloads only
/// the text parts and never has the whole raw message. Attachment bytes are
/// not cached (they're never fetched for display); only their metadata.
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
    /// Per-account directory holding fetched attachment bytes as flat files
    /// (see `load_attachment`/`store_attachment`). Kept separate from the
    /// SQLite database because attachment payloads are arbitrary binary blobs
    /// that can be megabytes and don't need indexing - a deterministic file
    /// path serves both on-demand read-back and the (out-of-scope) cache
    /// pruning, and `clear_all_caches` wipes them along with the databases.
    attachments_dir: std::path::PathBuf,
    /// Per-account directory holding whole raw RFC 5322 messages as `.eml`
    /// flat files (see `load_raw_message`/`store_raw_message`). Same rationale
    /// as `attachments_dir`: raw messages are opaque bytes served only for
    /// export, so a deterministic path is the whole index.
    messages_dir: std::path::PathBuf,
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

/// Joins the addresses of one header field (from/to/cc) into the space-joined
/// token string the search index stores: the bare address and the display name
/// each become their own tokens, so both `ada@example.com` and `Ada Lovelace`
/// match the same message.
fn index_addresses<'a>(addrs: impl IntoIterator<Item = &'a EmailAddress>) -> String {
    let mut out = String::new();
    for a in addrs {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&a.address);
        if let Some(name) = a.name.as_deref().filter(|n| !n.trim().is_empty()) {
            out.push(' ');
            out.push_str(name);
        }
    }
    out
}

/// Writes a new search-index row for `msg`, carrying `body` as the searchable
/// body text. INSERT-only on purpose: callers either cleared the row first
/// (`replace_messages` deletes the whole mailbox before inserting) or route
/// through `index_upsert_message`, because FTS5 has no `UPDATE` - and
/// `INSERT OR REPLACE` replaces by rowid, while the implicit autoincrement
/// rowid would let re-indexing a `(mailbox_id, uid)` accumulate a duplicate.
fn index_message(conn: &rusqlite::Connection, msg: &EmailSummary, body: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO search_fts (mailbox_id, uid, subject, sender, recipients, body) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            msg.mailbox.0,
            msg.uid.0,
            msg.subject.clone().unwrap_or_default(),
            index_addresses(&msg.from),
            index_addresses(msg.to.iter().chain(&msg.cc)),
            body,
        ],
    )?;
    Ok(())
}

/// Removes the search-index row for `(mailbox_id, uid)`, if any.
fn delete_index_row(conn: &rusqlite::Connection, mailbox_id: &MailboxId, uid: Uid) -> Result<()> {
    conn.execute("DELETE FROM search_fts WHERE mailbox_id = ?1 AND uid = ?2", rusqlite::params![mailbox_id.0, uid.0])?;
    Ok(())
}

/// Rewrites the search-index row for `msg` in place (delete then insert), for
/// callers that re-index an existing message - `store_body` upgrading the
/// body text, and the one-time backfill. Idempotent even against a re-run,
/// because the delete removes exactly this message's old row.
fn index_upsert_message(conn: &rusqlite::Connection, msg: &EmailSummary, body: &str) -> Result<()> {
    delete_index_row(conn, &msg.mailbox, msg.uid)?;
    index_message(conn, msg, body)
}

/// The searchable body text of an assembled [`EmailBody`]: the plain-text
/// part, or a stripped-HTML rendering when there's no text part. Mirrors
/// `preview_from_raw`'s text-over-html preference, but returns the whole body
/// rather than a snippet.
///
/// Bodies over `FULL_BODY_INDEX_BYTES` are skipped (returning `None`, so the
/// message keeps its preview-only index row): indexing a multi-megabyte
/// marketing HTML mail costs memory and makes every `store_body` rewrite slow,
/// and its first few KB of readable text is usually in the preview anyway.
fn body_index_text(body: &EmailBody) -> Option<String> {
    let text = body.text_body.as_deref().unwrap_or("");
    let html = body.html_body.as_deref().unwrap_or("");
    let mut out = String::with_capacity(text.len() + html.len());
    out.push_str(text);
    if !html.is_empty() {
        out.push(' ');
        out.push_str(&crate::body::strip_html_for_index(html));
    }
    if out.trim().is_empty() {
        return None;
    }
    if out.len() > FULL_BODY_INDEX_BYTES {
        return None;
    }
    Some(out)
}

/// Bodies larger than this aren't re-parsed for the search index (see
/// `body_index_text`).
const FULL_BODY_INDEX_BYTES: usize = 256 * 1024;

/// Loads one cached summary by `(mailbox_id, uid)`, if present.
fn load_summary_row(conn: &rusqlite::Connection, mailbox_id: &MailboxId, uid: Uid) -> Result<Option<EmailSummary>> {
    let mut stmt = conn.prepare("SELECT data FROM messages WHERE mailbox_id = ?1 AND uid = ?2")?;
    let mut rows = stmt.query_map(rusqlite::params![mailbox_id.0, uid.0], |row| row.get::<_, String>(0))?;
    match rows.next() {
        Some(Ok(data)) => Ok(serde_json::from_str(&data).ok()),
        Some(Err(e)) => Err(e.into()),
        None => Ok(None),
    }
}

/// Converts a free-text user query into a valid FTS5 `MATCH` expression, or
/// `None` when there's nothing to search for.
///
/// Every bare word becomes a quoted phrase and the phrases are ANDed, so
/// `foo bar` means "match both words" and a `"`-quoted run of words stays a
/// single phrase (adjacent-token match). Quoting neutralizes all FTS5 query
/// syntax (`AND`/`OR`/`NOT`/`NEAR`, `*`, `^`, parens) by turning each term
/// into a literal search - a user typing `AND` searches for the word "and",
/// it can never alter the query's shape. The unicode61 tokenizer splits on
/// the same characters in the query as in the indexed text, so an address
/// query `ada@example.com` still matches the tokens `ada example com` a
/// stored address tokenizes to.
pub fn sanitize_fts_query(input: &str) -> Option<String> {
    let mut clauses: Vec<String> = Vec::new();
    let mut rest = input.trim();
    while !rest.is_empty() {
        match rest.find('"') {
            Some(0) => {
                let after = &rest[1..];
                match after.find('"') {
                    Some(end) => {
                        let phrase = &after[..end];
                        if !phrase.trim().is_empty() {
                            clauses.push(format!("\"{}\"", phrase.replace('"', " ")));
                        }
                        rest = &after[end + 1..];
                    }
                    None => {
                        // Unterminated quote: treat the remainder as bare words.
                        rest = after;
                    }
                }
            }
            Some(q) => {
                for word in rest[..q].split_whitespace() {
                    if !word.is_empty() {
                        clauses.push(format!("\"{}\"", word.replace('"', " ")));
                    }
                }
                rest = &rest[q..];
            }
            None => {
                for word in rest.split_whitespace() {
                    if !word.is_empty() {
                        clauses.push(format!("\"{}\"", word.replace('"', " ")));
                    }
                }
                rest = "";
            }
        }
    }
    if clauses.is_empty() {
        None
    } else {
        Some(clauses.join(" AND "))
    }
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
            -- The full-text search index over cached envelopes + bodies. The
            -- bundled SQLite is built with FTS5 (see libsqlite3-sys's
            -- `-DSQLITE_ENABLE_FTS5`), so this is always available. Columns
            -- are the searchable surface: subject, sender (name + address),
            -- recipients (to/cc), and body (preview, upgraded to the full
            -- cached text once a body is fetched). `mailbox_id`/`uid` are
            -- UNINDEXED so they're usable as filter keys without being
            -- tokenized. Rows are keyed by `(mailbox_id, uid)` and rewritten
            -- wholesale by the callers below (`replace_messages`/`store_body`
            -- delete-then-insert), so FTS5's autoincrement rowid never leaks
            -- duplicates.
            CREATE VIRTUAL TABLE IF NOT EXISTS search_fts USING fts5(
                mailbox_id UNINDEXED,
                uid UNINDEXED,
                subject,
                sender,
                recipients,
                body,
                tokenize = 'unicode61 remove_diacritics 2'
            );
            ",
        )?;
        // One-time envelope-cache migration. Pre-full-sync builds kept only a
        // folder's newest ~200 messages, so their `messages` rows hold a
        // *subset* of the mailbox - and the session's cache-hit path would
        // serve that subset forever without a live sync, hiding the older
        // mail a full sync would fetch. Every sync now writes the whole
        // folder, so once this version is recorded the cache is trustworthy;
        // wiping the envelope table now forces each folder to re-sync in full
        // on its next open. Snoozes, addresses, and the mailbox list are all
        // untouched.
        const ENVELOPE_CACHE_VERSION: i64 = 1;
        // One-time body-cache migration: `bodies` rows changed format from
        // raw RFC 5322 bytes to serialized `EmailBody` JSON (the partial-fetch
        // path never assembles a whole raw message), so pre-partial-fetch
        // caches' raw rows can't be served and are wiped once. Envelope rows
        // survive; their `structure` stays `None` until the next sync, which
        // is exactly the fallback path's cue.
        const BODY_CACHE_VERSION: i64 = 3;
        // Version 3: the whole-message fallback path (`parse_body`) used to
        // number attachment parts by enumerate counter ("0", "1", ...) instead
        // of their IMAP section paths, so cached bodies from those builds
        // carry part numbers no `UID FETCH BODY.PEEK[<n>]` can satisfy - a
        // save would silently hang. Wipe bodies once so the fixed builds
        // re-assemble them with real section paths. Envelope rows survive.
        let stored: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap_or(0);
        if stored < ENVELOPE_CACHE_VERSION {
            conn.execute("DELETE FROM messages", [])?;
        }
        if stored < BODY_CACHE_VERSION {
            conn.execute("DELETE FROM bodies", [])?;
        }
        if stored < BODY_CACHE_VERSION || stored < ENVELOPE_CACHE_VERSION {
            conn.pragma_update(None, "user_version", BODY_CACHE_VERSION.max(ENVELOPE_CACHE_VERSION))?;
        }

        // NOTE: the one-time FTS backfill is *not* run here. `Cache::open` is
        // also called on the UI thread (the app's read-side handle for
        // composer autocomplete), and backfilling a large pre-search cache -
        // re-parsing every cached body - could block startup for seconds.
        // `run_account_session` calls `Cache::backfill_search_index()` on the
        // worker thread instead (see session.rs), and `replace_messages`
        // keeps the index populated for everything synced under this build.

        let attachments_dir = cache_dir().join("attachments").join(sanitize_filename(account_id));
        let messages_dir = cache_dir().join("messages").join(sanitize_filename(account_id));
        Ok(Cache {
            conn: Mutex::new(conn),
            attachments_dir,
            messages_dir,
        })
    }

    /// One-time FTS backfill: populates an empty `search_fts` from the
    /// `messages` and `bodies` tables, so caches created before the search
    /// index existed become searchable without forcing a full re-sync. A
    /// body's row replaces the preview-only row once its full text is
    /// available; bodies without a matching envelope (their message was wiped
    /// by the envelope-version migration) are skipped - there's no
    /// subject/sender to index against.
    ///
    /// Cheap to call on every session start: a populated index short-circuits
    /// at the count check. Idempotent against re-runs because every row is
    /// written through `index_upsert_message` (delete-then-insert), so even a
    /// hypothetical concurrent backfill can't duplicate a `(mailbox_id, uid)`.
    /// Must run off the UI thread - see the note in `open`.
    pub fn backfill_search_index(&self) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let fts_count: i64 = tx.query_row("SELECT count(*) FROM search_fts", [], |r| r.get(0)).unwrap_or(0);
        if fts_count != 0 {
            tx.commit()?;
            return Ok(());
        }
        {
            let mut stmt = tx.prepare("SELECT data FROM messages")?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            for row in rows {
                if let Ok(msg) = serde_json::from_str::<EmailSummary>(&row?) {
                    index_upsert_message(&tx, &msg, msg.preview.as_deref().unwrap_or(""))?;
                }
            }
        }
        {
            let mut stmt = tx.prepare("SELECT mailbox_id, uid, data FROM bodies")?;
            let rows = stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, u32>(1)?, row.get::<_, Vec<u8>>(2)?)))?;
            for row in rows {
                let (mailbox, uid, data) = row?;
                let Ok(body) = serde_json::from_slice::<EmailBody>(&data) else { continue };
                let Some(text) = body_index_text(&body) else { continue };
                let mailbox_id = MailboxId(mailbox);
                let summary = match load_summary_row(&tx, &mailbox_id, Uid(uid))? {
                    Some(msg) => msg,
                    None => continue,
                };
                index_upsert_message(&tx, &summary, &text)?;
            }
        }
        tx.commit()?;
        Ok(())
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
        tx.execute("DELETE FROM search_fts WHERE mailbox_id = ?1", [&mailbox_id.0])?;
        for msg in messages {
            let data = serde_json::to_string(msg)?;
            tx.execute(
                "INSERT INTO messages (mailbox_id, uid, uidvalidity, data) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![mailbox_id.0, msg.uid.0, uidvalidity.0, data],
            )?;
            // The body column starts as the preview (the only text a fresh
            // envelope fetch carries); `store_body` upgrades it in place once
            // the full message is cached.
            index_message(&tx, msg, msg.preview.as_deref().unwrap_or(""))?;
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

    /// Loads the cached summary for one `(mailbox_id, uid)`, or `None` if it
    /// isn't cached. Used by the session's body-fetch path to learn a
    /// message's `BODYSTRUCTURE`-derived part structure without loading the
    /// whole mailbox's message set.
    pub fn load_summary(&self, mailbox_id: &MailboxId, uid: Uid) -> Result<Option<EmailSummary>> {
        let conn = self.conn.lock().unwrap();
        load_summary_row(&conn, mailbox_id, uid)
    }

    /// Returns the previously-fetched body of a message, or `None` if it
    /// isn't cached. `uidvalidity` guards the cache against serving a body
    /// for a recycled uid after its mailbox was re-created (RFC 3501
    /// §2.3.1.1): a row written under a different uidvalidity is a miss,
    /// never a stale body. Rows are the assembled [`EmailBody`] (JSON) -
    /// what the viewer consumes - since the partial-fetch path never
    /// assembles a whole raw message.
    pub fn load_body(&self, mailbox_id: &MailboxId, uid: Uid, uidvalidity: UidValidity) -> Result<Option<EmailBody>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT data FROM bodies WHERE mailbox_id = ?1 AND uid = ?2 AND uidvalidity = ?3")?;
        let mut rows = stmt.query_map(rusqlite::params![mailbox_id.0, uid.0, uidvalidity.0], |row| row.get::<_, Vec<u8>>(0))?;
        match rows.next() {
            Some(Ok(data)) => Ok(serde_json::from_slice(&data).ok()),
            Some(Err(e)) => Err(e.into()),
            None => Ok(None),
        }
    }

    /// Stores an assembled body for `uid`, replacing any earlier body for the
    /// same `(mailbox, uid)`, and upgrades the search index's body text from
    /// the preview to the message's full text.
    pub fn store_body(&self, mailbox_id: &MailboxId, uid: Uid, uidvalidity: UidValidity, body: &EmailBody) -> Result<()> {
        let data = serde_json::to_vec(body)?;
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO bodies (mailbox_id, uid, uidvalidity, data) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![mailbox_id.0, uid.0, uidvalidity.0, data],
        )?;
        // Re-index the message with its full text, but only if the envelope is
        // cached (a stray body for a wiped envelope has no subject/sender to
        // index against). The preview text rides along so previously-indexed
        // phrasing stays findable.
        if let Some(summary) = load_summary_row(&tx, mailbox_id, uid)? {
            let mut indexed = body_index_text(body).unwrap_or_default();
            if let Some(preview) = &summary.preview {
                if !indexed.is_empty() {
                    indexed.push(' ');
                }
                indexed.push_str(preview);
            }
            index_upsert_message(&tx, &summary, &indexed)?;
        }
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

    /// The per-account flat-file path an attachment's *decoded* bytes are
    /// stored at. Deterministic - derived only from the mailbox identity (via
    /// a fixed-seed hash, so two mailbox paths can never collide into one
    /// filename) and `uidvalidity`/`uid`/`part_number` - so `load_attachment`
    /// can re-find what `store_attachment` wrote without any index. `uidvalidity`
    /// joins the key so a recycled uid after a mailbox re-create (RFC 3501
    /// §2.3.1.1) can never resolve to a stale attachment, matching
    /// `load_body`'s guard.
    fn attachment_path(&self, mailbox_id: &MailboxId, uid: Uid, uidvalidity: UidValidity, part_number: &str) -> PathBuf {
        let mut hasher = DefaultHasher::new();
        mailbox_id.0.hash(&mut hasher);
        let mailbox_hash = hasher.finish();
        self.attachments_dir.join(format!("{mailbox_hash:016x}-{}-{}-{part_number}.bin", uidvalidity.0, uid.0))
    }

    /// Returns the previously-fetched bytes of one attachment part, or `None`
    /// if they aren't cached. Served straight from a flat file - no round trip
    /// through the database, since attachment payloads are opaque binary blobs.
    pub fn load_attachment(&self, mailbox_id: &MailboxId, uid: Uid, uidvalidity: UidValidity, part_number: &str) -> Result<Option<Vec<u8>>> {
        let path = self.attachment_path(mailbox_id, uid, uidvalidity, part_number);
        match std::fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Persists the *decoded* bytes of one attachment part to its flat file so
    /// a re-open (or a later session) can serve them without re-fetching. The
    /// bytes are expected to already be transfer-decoded; the encoding is a
    /// `BodyPart::transfer_encoding` concern that belongs to the fetch path.
    pub fn store_attachment(&self, mailbox_id: &MailboxId, uid: Uid, uidvalidity: UidValidity, part_number: &str, bytes: &[u8]) -> Result<()> {
        let path = self.attachment_path(mailbox_id, uid, uidvalidity, part_number);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, bytes)?;
        Ok(())
    }

    /// The per-account flat-file path a whole raw RFC 5322 message's bytes are
    /// stored at, keyed exactly like `attachment_path` (mailbox identity via
    /// fixed-seed hash + `uidvalidity`/`uid`) but with an `.eml` extension.
    fn raw_message_path(&self, mailbox_id: &MailboxId, uid: Uid, uidvalidity: UidValidity) -> PathBuf {
        let mut hasher = DefaultHasher::new();
        mailbox_id.0.hash(&mut hasher);
        let mailbox_hash = hasher.finish();
        self.messages_dir.join(format!("{mailbox_hash:016x}-{}-{}.eml", uidvalidity.0, uid.0))
    }

    /// Returns the previously-fetched whole raw message bytes (a valid RFC
    /// 5322 message, exactly what `BODY.PEEK[]` returned), or `None` if they
    /// aren't cached. Served straight from a flat file so an .eml export can
    /// be instant and offline after the first fetch.
    pub fn load_raw_message(&self, mailbox_id: &MailboxId, uid: Uid, uidvalidity: UidValidity) -> Result<Option<Vec<u8>>> {
        let path = self.raw_message_path(mailbox_id, uid, uidvalidity);
        match std::fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Persists a whole raw RFC 5322 message (as fetched with `BODY.PEEK[]`,
    /// unmodified) to its flat file. Callers may store bytes they already
    /// downloaded for another purpose (e.g. the whole-message body fallback
    /// path) at near-zero cost, or fetch on demand for an export.
    pub fn store_raw_message(&self, mailbox_id: &MailboxId, uid: Uid, uidvalidity: UidValidity, bytes: &[u8]) -> Result<()> {
        let path = self.raw_message_path(mailbox_id, uid, uidvalidity);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, bytes)?;
        Ok(())
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

    /// Full-text search over this account's cached envelopes and bodies.
    /// Runs entirely against the local index (no IMAP round trip), returning
    /// at most `limit` matches, most-relevant first, with snoozed messages
    /// excluded to match what the message list shows.
    ///
    /// Coverage is bounded by what's been synced: a folder's envelope fields
    /// (subject/sender/recipients) plus the preview are indexed the moment it
    /// syncs, and the body text joins in once a full body is fetched or
    /// prefetched. Mail that has never been synced (an unopened folder) and
    /// body text for never-fetched messages need the IMAP `SEARCH` fallback -
    /// see `AccountCommand::SearchMailbox`.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<EmailSummary>> {
        let Some(match_query) = sanitize_fts_query(query) else {
            return Ok(Vec::new());
        };
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT mailbox_id, uid FROM search_fts WHERE search_fts MATCH ?1 ORDER BY rank LIMIT ?2")?;
        let rows = stmt.query_map(rusqlite::params![match_query, limit as i64], |row| Ok((row.get::<_, String>(0)?, row.get::<_, u32>(1)?)))?;
        let mut hits = Vec::new();
        for row in rows {
            let (mailbox, uid) = row?;
            hits.push((MailboxId(mailbox), Uid(uid)));
        }
        drop(stmt);

        // Snoozed messages stay hidden from the list, so they don't surface in
        // search either. Read the active set once rather than per hit.
        let now = Utc::now().timestamp();
        let mut snoozed: HashSet<(String, u32)> = HashSet::new();
        {
            let mut stmt = conn.prepare("SELECT mailbox_id, uid FROM snoozed WHERE snoozed_until > ?1")?;
            let rows = stmt.query_map([now], |row| Ok((row.get::<_, String>(0)?, row.get::<_, u32>(1)?)))?;
            for row in rows {
                snoozed.insert(row?);
            }
        }

        let mut out = Vec::new();
        for (mailbox, uid) in hits {
            if snoozed.contains(&(mailbox.0.clone(), uid.0)) {
                continue;
            }
            // A hit whose envelope row is gone (the migration wiped the
            // messages table once, on the version bump) is skipped, not fatal.
            if let Some(msg) = load_summary_row(&conn, &mailbox, uid)? {
                out.push(msg);
            }
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

    /// Applies a keyword change to one cached summary, mirroring the `STORE`
    /// the session just issued - the same contract as `update_flags`, for the
    /// custom-flag atoms (e.g. `$Lookout-tag-<key>`) that carry color tags.
    /// Keywords are plain strings in the set, so add/remove are simple set
    /// operations on the deserialized summary.
    pub fn update_keywords(&self, mailbox_id: &MailboxId, uid: Uid, add: &[String], remove: &[String]) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT data FROM messages WHERE mailbox_id = ?1 AND uid = ?2")?;
        let mut rows = stmt.query_map(rusqlite::params![mailbox_id.0, uid.0], |row| row.get::<_, String>(0))?;
        let Some(data) = rows.next().transpose()? else {
            return Ok(false);
        };
        let Ok(mut summary) = serde_json::from_str::<EmailSummary>(&data) else {
            return Ok(false);
        };
        for keyword in add {
            summary.keywords.insert(keyword.clone());
        }
        for keyword in remove {
            summary.keywords.remove(keyword);
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

    /// Removes a single message (plus its cached body and any snooze entry)
    /// from the cache. Used right after a successful MOVE so the deleted or
    /// archived message drops out of the next `MessagesUpdated` immediately
    /// instead of waiting for the authoritative resync to re-fetch the whole
    /// window. The next `replace_messages` wipes the window anyway, so this is
    /// a display-latency optimization, never the source of truth.
    pub fn delete_message(&self, mailbox_id: &MailboxId, uid: Uid) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM messages WHERE mailbox_id = ?1 AND uid = ?2", rusqlite::params![mailbox_id.0, uid.0])?;
        tx.execute("DELETE FROM bodies WHERE mailbox_id = ?1 AND uid = ?2", rusqlite::params![mailbox_id.0, uid.0])?;
        tx.execute("DELETE FROM snoozed WHERE mailbox_id = ?1 AND uid = ?2", rusqlite::params![mailbox_id.0, uid.0])?;
        tx.execute("DELETE FROM search_fts WHERE mailbox_id = ?1 AND uid = ?2", rusqlite::params![mailbox_id.0, uid.0])?;
        tx.commit()?;
        Ok(())
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

    /// The move path's instant-update relies on this: after a `delete_message`
    /// the remaining cached set no longer contains the moved uid, its body is
    /// gone, and a delete of a non-cached uid is a harmless no-op.
    #[test]
    fn deleting_a_message_drops_it_and_its_body_from_the_cache() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let mailbox_id = MailboxId::new(&account_id, "INBOX");

        cache
            .replace_messages(&mailbox_id, UidValidity(1), &[sample_summary(&mailbox_id, 1, None), sample_summary(&mailbox_id, 2, None)])
            .unwrap();
        cache.store_body(&mailbox_id, Uid(1), UidValidity(1), &sample_body("raw one")).unwrap();
        cache.store_body(&mailbox_id, Uid(2), UidValidity(1), &sample_body("raw two")).unwrap();
        cache.snooze_message(&mailbox_id, Uid(2), Utc::now() + chrono::Duration::hours(1)).unwrap();

        cache.delete_message(&mailbox_id, Uid(1)).unwrap();

        let remaining = cache.load_messages(&mailbox_id).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].uid, Uid(2));
        assert!(!cache.has_body(&mailbox_id, Uid(1), UidValidity(1)).unwrap());
        assert!(cache.has_body(&mailbox_id, Uid(2), UidValidity(1)).unwrap());
        assert_eq!(cache.active_snoozed_uids(&mailbox_id, Utc::now()).unwrap(), HashSet::from([Uid(2)]));

        // Deleting a uid the cache doesn't know is fine - used when the moved
        // message fell outside the cached window.
        cache.delete_message(&mailbox_id, Uid(99)).unwrap();
        assert_eq!(cache.load_messages(&mailbox_id).unwrap().len(), 1);

        let path = cache_dir().join(format!("{}.sqlite3", sanitize_filename(&account_id)));
        let _ = std::fs::remove_file(path);
    }

    /// The on-demand attachment fetch depends on this: bytes stored once come
    /// back verbatim, distinct parts (and distinct messages) never collide,
    /// and a part the cache doesn't know reports a clean miss.
    #[test]
    fn round_trips_attachment_bytes_through_the_flat_file_cache() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let mailbox_id = MailboxId::new(&account_id, "INBOX");

        let pdf = b"%PDF-1.4 fake pdf bytes".to_vec();
        let png: Vec<u8> = (0u8..255).collect();
        cache.store_attachment(&mailbox_id, Uid(1), UidValidity(1), "2", &pdf).unwrap();
        cache.store_attachment(&mailbox_id, Uid(1), UidValidity(1), "3", &png).unwrap();
        cache.store_attachment(&mailbox_id, Uid(2), UidValidity(1), "2", b"other message").unwrap();

        assert_eq!(cache.load_attachment(&mailbox_id, Uid(1), UidValidity(1), "2").unwrap(), Some(pdf.clone()));
        assert_eq!(cache.load_attachment(&mailbox_id, Uid(1), UidValidity(1), "3").unwrap(), Some(png.clone()));
        assert_eq!(cache.load_attachment(&mailbox_id, Uid(2), UidValidity(1), "2").unwrap(), Some(b"other message".to_vec()));
        assert_eq!(cache.load_attachment(&mailbox_id, Uid(1), UidValidity(1), "9").unwrap(), None);

        // A second cache handle on the same account (e.g. the app's read-side
        // one) derives the same path and sees the same bytes.
        let reopened = Cache::open(&account_id).unwrap();
        assert_eq!(reopened.load_attachment(&mailbox_id, Uid(1), UidValidity(1), "2").unwrap(), Some(pdf));

        let path = cache_dir().join(format!("{}.sqlite3", sanitize_filename(&account_id)));
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_dir_all(cache_dir().join("attachments").join(sanitize_filename(&account_id)));
    }

    /// `uidvalidity` guards the attachment cache the same way it guards
    /// `bodies`: after a mailbox is re-created, a recycled uid must be a miss,
    /// not another message's attachment.
    #[test]
    fn attachment_cache_respects_uidvalidity() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let mailbox_id = MailboxId::new(&account_id, "INBOX");

        cache.store_attachment(&mailbox_id, Uid(7), UidValidity(1), "2", b"under uidvalidity 1").unwrap();
        assert_eq!(cache.load_attachment(&mailbox_id, Uid(7), UidValidity(2), "2").unwrap(), None);
        assert_eq!(
            cache.load_attachment(&mailbox_id, Uid(7), UidValidity(1), "2").unwrap(),
            Some(b"under uidvalidity 1".to_vec())
        );

        let path = cache_dir().join(format!("{}.sqlite3", sanitize_filename(&account_id)));
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_dir_all(cache_dir().join("attachments").join(sanitize_filename(&account_id)));
    }

    /// The .eml export cache: bytes stored once come back verbatim, distinct
    /// messages never collide, an unknown message reports a clean miss, and a
    /// second cache handle on the same account derives the same path.
    #[test]
    fn round_trips_raw_messages_through_the_flat_file_cache() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let mailbox_id = MailboxId::new(&account_id, "INBOX");

        let one = b"Subject: one\r\n\r\nbody one".to_vec();
        let two = b"Subject: two\r\n\r\nbody two".to_vec();
        cache.store_raw_message(&mailbox_id, Uid(1), UidValidity(1), &one).unwrap();
        cache.store_raw_message(&mailbox_id, Uid(2), UidValidity(1), &two).unwrap();

        assert_eq!(cache.load_raw_message(&mailbox_id, Uid(1), UidValidity(1)).unwrap(), Some(one.clone()));
        assert_eq!(cache.load_raw_message(&mailbox_id, Uid(2), UidValidity(1)).unwrap(), Some(two));
        assert_eq!(cache.load_raw_message(&mailbox_id, Uid(3), UidValidity(1)).unwrap(), None);

        let reopened = Cache::open(&account_id).unwrap();
        assert_eq!(reopened.load_raw_message(&mailbox_id, Uid(1), UidValidity(1)).unwrap(), Some(one));

        let path = cache_dir().join(format!("{}.sqlite3", sanitize_filename(&account_id)));
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_dir_all(cache_dir().join("messages").join(sanitize_filename(&account_id)));
    }

    /// `uidvalidity` guards the raw-message cache the same way it guards
    /// `bodies` and attachments: after a mailbox re-create, a recycled uid
    /// must be a miss, not another message's .eml.
    #[test]
    fn raw_message_cache_respects_uidvalidity() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let mailbox_id = MailboxId::new(&account_id, "INBOX");

        cache.store_raw_message(&mailbox_id, Uid(7), UidValidity(1), b"Subject: old\r\n\r\nx").unwrap();
        assert_eq!(cache.load_raw_message(&mailbox_id, Uid(7), UidValidity(2)).unwrap(), None);
        assert_eq!(
            cache.load_raw_message(&mailbox_id, Uid(7), UidValidity(1)).unwrap(),
            Some(b"Subject: old\r\n\r\nx".to_vec())
        );

        let path = cache_dir().join(format!("{}.sqlite3", sanitize_filename(&account_id)));
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_dir_all(cache_dir().join("messages").join(sanitize_filename(&account_id)));
    }

    /// The one-time upgrades that make the cache-hit path safe again: a cache
    /// written by a pre-full-sync build holds only a windowed subset, so
    /// opening it must wipe the envelope table (forcing full re-syncs), and a
    /// pre-partial-fetch build stored raw RFC 5322 bytes in `bodies`, so a
    /// format change must wipe those too. Each wipe happens exactly once,
    /// since every sync after the migration writes the whole folder (and
    /// every body fetch writes the new format).
    #[test]
    fn wiping_stale_caches_once_on_format_version_change() {
        let account_id = temp_account_id();
        let mailbox_id = MailboxId::new(&account_id, "INBOX");
        let path = cache_dir().join(format!("{}.sqlite3", sanitize_filename(&account_id)));

        // First open under the current build: migrations run, wiping rows.
        let cache = Cache::open(&account_id).unwrap();
        cache.replace_messages(&mailbox_id, UidValidity(1), &[sample_summary(&mailbox_id, 1, None)]).unwrap();
        cache.store_body(&mailbox_id, Uid(1), UidValidity(1), &sample_body("body survives")).unwrap();

        // Reopen: version already current, so the envelope rows must survive.
        let cache = Cache::open(&account_id).unwrap();
        assert_eq!(cache.load_messages(&mailbox_id).unwrap().len(), 1, "a current-version cache must not be wiped on reopen");
        assert_eq!(
            cache.load_body(&mailbox_id, Uid(1), UidValidity(1)).unwrap().map(|b| b.text_body.unwrap()),
            Some("body survives".to_string())
        );

        // Simulate a pre-everything database (version 0): the next open must
        // wipe both the envelope rows and the bodies.
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.pragma_update(None, "user_version", 0).unwrap();
        }
        cache.replace_messages(&mailbox_id, UidValidity(1), &[sample_summary(&mailbox_id, 2, None)]).unwrap();
        cache.store_body(&mailbox_id, Uid(2), UidValidity(1), &sample_body("fresh body")).unwrap();
        let cache = Cache::open(&account_id).unwrap();
        assert!(cache.load_messages(&mailbox_id).unwrap().is_empty(), "a pre-migration cache must be wiped");
        assert!(!cache.has_body(&mailbox_id, Uid(1), UidValidity(1)).unwrap(), "a pre-migration body must be wiped");

        // Simulate a version-1 database (envelope migration done, body format
        // not): envelope rows survive, bodies are wiped.
        cache.replace_messages(&mailbox_id, UidValidity(1), &[sample_summary(&mailbox_id, 3, None)]).unwrap();
        cache.store_body(&mailbox_id, Uid(3), UidValidity(1), &sample_body("new-format body")).unwrap();
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.pragma_update(None, "user_version", 1).unwrap();
        }
        cache.store_body(&mailbox_id, Uid(3), UidValidity(1), &sample_body("old-format body")).unwrap();
        let cache = Cache::open(&account_id).unwrap();
        assert_eq!(cache.load_messages(&mailbox_id).unwrap().len(), 1, "a version-1 cache's envelope rows must survive");
        assert!(!cache.has_body(&mailbox_id, Uid(3), UidValidity(1)).unwrap(), "a version-1 cache's bodies must be wiped");

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
            structure: None,
        }
    }

    fn sample_body(text: &str) -> EmailBody {
        EmailBody {
            uid: Uid(0),
            text_body: Some(text.to_string()),
            html_body: None,
            parts: Vec::new(),
            headers: Vec::new(),
            auth_results: None,
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

    /// The tag-toggle contract: an `update_keywords` add/remove round-trips
    /// through a reload (so a restart before the next sync keeps showing the
    /// tag), and it leaves the summary's other fields - flags in particular -
    /// untouched. A uid outside the cached window is a no-op, not an error.
    #[test]
    fn patches_keywords_on_a_cached_summary() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let mailbox_id = MailboxId::new(&account_id, "INBOX");
        let work = lookout_core::tag_keyword("work");
        let red = lookout_core::tag_keyword("red");

        cache.replace_messages(&mailbox_id, UidValidity(1), &[sample_summary(&mailbox_id, 1, None)]).unwrap();
        assert!(cache.update_flags(&mailbox_id, Uid(1), &[SystemFlagBit::Seen], &[]).unwrap());

        assert!(cache.update_keywords(&mailbox_id, Uid(1), &[work.clone(), red.clone()], &[]).unwrap());
        let loaded = &cache.load_messages(&mailbox_id).unwrap()[0];
        assert!(loaded.keywords.contains(&work));
        assert!(loaded.keywords.contains(&red));
        // The keyword patch must not have disturbed the flag patch.
        assert!(!loaded.is_unread());

        assert!(cache.update_keywords(&mailbox_id, Uid(1), &[], std::slice::from_ref(&red)).unwrap());
        let loaded = &cache.load_messages(&mailbox_id).unwrap()[0];
        assert!(loaded.keywords.contains(&work));
        assert!(!loaded.keywords.contains(&red));

        assert!(!cache.update_keywords(&mailbox_id, Uid(99), &[work], &[]).unwrap());

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
        let body = sample_body("Hello from the cache");

        // A never-fetched body is a miss.
        assert!(cache.load_body(&mailbox_id, Uid(7), UidValidity(3)).unwrap().is_none());

        cache.store_body(&mailbox_id, Uid(7), UidValidity(3), &body).unwrap();
        assert_eq!(cache.load_body(&mailbox_id, Uid(7), UidValidity(3)).unwrap(), Some(body));

        // A mailbox that was re-created (uidvalidity changed) reuses uids; a
        // stale row must be a miss, not a wrong body.
        assert!(cache.load_body(&mailbox_id, Uid(7), UidValidity(4)).unwrap().is_none());

        // Re-storing the same (mailbox, uid) replaces the bytes.
        let updated = sample_body("updated");
        cache.store_body(&mailbox_id, Uid(7), UidValidity(3), &updated).unwrap();
        assert_eq!(cache.load_body(&mailbox_id, Uid(7), UidValidity(3)).unwrap(), Some(updated));

        let path = cache_dir().join(format!("{}.sqlite3", sanitize_filename(&account_id)));
        let _ = std::fs::remove_file(path);
    }

    /// Builds a message with a subject, a From address and a preview, so the
    /// search tests have something to index beyond the bare `sample_summary`.
    fn searchable_summary(mailbox_id: &MailboxId, uid: u32, subject: &str, from: &str, preview: Option<&str>) -> EmailSummary {
        let mut msg = sample_summary(mailbox_id, uid, preview);
        msg.subject = Some(subject.to_string());
        msg.from = vec![lookout_core::EmailAddress::new(from)];
        msg
    }

    #[test]
    fn sanitize_fts_query_ands_bare_words_and_keeps_phrases() {
        assert_eq!(sanitize_fts_query("hello world").as_deref(), Some("\"hello\" AND \"world\""));
        assert_eq!(sanitize_fts_query("\"multi word\"").as_deref(), Some("\"multi word\""));
        assert_eq!(sanitize_fts_query("foo \"bar baz\" qux").as_deref(), Some("\"foo\" AND \"bar baz\" AND \"qux\""));
        // FTS operators become literal terms, not syntax.
        assert_eq!(sanitize_fts_query("AND NOT NEAR").as_deref(), Some("\"AND\" AND \"NOT\" AND \"NEAR\""));
        // An unterminated quote degrades to bare words rather than erroring.
        assert_eq!(sanitize_fts_query("foo \"bar").as_deref(), Some("\"foo\" AND \"bar\""));
        // Nothing to search for yields no query at all.
        assert_eq!(sanitize_fts_query(""), None);
        assert_eq!(sanitize_fts_query("   "), None);
        assert_eq!(sanitize_fts_query("\"\""), None);
    }

    /// The FTS index's core contract: subject, sender, and body-preview terms
    /// all find a message, ANDed terms narrow it, and a query that matches
    /// nothing is an empty result, not an error.
    #[test]
    fn search_matches_subject_sender_preview_and_requires_all_terms() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let mailbox_id = MailboxId::new(&account_id, "INBOX");

        cache
            .replace_messages(
                &mailbox_id,
                UidValidity(1),
                &[
                    searchable_summary(&mailbox_id, 1, "Quarterly report", "ada@example.com", Some("The numbers are attached")),
                    searchable_summary(&mailbox_id, 2, "Lunch plans", "bob@elsewhere.org", Some("Pizza at noon")),
                ],
            )
            .unwrap();

        assert_eq!(cache.search("quarterly", 10).unwrap().len(), 1);
        assert_eq!(cache.search("ADA@example.com", 10).unwrap().len(), 1);
        assert_eq!(cache.search("pizza", 10).unwrap().len(), 1);
        // An address tokenizes into `ada example com`; any of them match.
        assert_eq!(cache.search("elsewhere", 10).unwrap().len(), 1);

        // AND semantics: both terms must be present in the same message.
        assert_eq!(cache.search("quarterly report", 10).unwrap().len(), 1);
        assert_eq!(cache.search("quarterly pizza", 10).unwrap().len(), 0);

        // Case-insensitive, per the unicode61 tokenizer.
        assert_eq!(cache.search("PIZZA NOON", 10).unwrap().len(), 1);

        // No hits is a valid empty answer.
        assert!(cache.search("no-such-term", 10).unwrap().is_empty());
        // A whitespace-only query is not even sent to the index.
        assert!(cache.search("   ", 10).unwrap().is_empty());

        let path = cache_dir().join(format!("{}.sqlite3", sanitize_filename(&account_id)));
        let _ = std::fs::remove_file(path);
    }

    /// `store_body` upgrades a message's indexed body text from the preview to
    /// the full cached message, so a term that only appears in the body becomes
    /// searchable once the message has been fetched.
    #[test]
    fn store_body_makes_full_body_text_searchable() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let mailbox_id = MailboxId::new(&account_id, "INBOX");

        let msg = searchable_summary(&mailbox_id, 1, "Quarterly report", "ada@example.com", Some("The numbers"));
        cache.replace_messages(&mailbox_id, UidValidity(1), &[msg]).unwrap();

        // Only the preview is indexed before the body arrives.
        assert_eq!(cache.search("numbers", 10).unwrap().len(), 1);
        assert_eq!(cache.search("confidential", 10).unwrap().len(), 0);

        cache
            .store_body(&mailbox_id, Uid(1), UidValidity(1), &sample_body("This document is confidential."))
            .unwrap();

        assert_eq!(cache.search("confidential", 10).unwrap().len(), 1);
        // The preview term still matches after the body replaces it.
        assert_eq!(cache.search("numbers", 10).unwrap().len(), 1);

        let path = cache_dir().join(format!("{}.sqlite3", sanitize_filename(&account_id)));
        let _ = std::fs::remove_file(path);
    }

    /// Snoozed messages are excluded from results (they're hidden from the
    /// list too), and a `replace_messages` rebuild drops the old mailbox's
    /// index rows rather than accumulating stale hits.
    #[test]
    fn search_excludes_snoozed_and_rebuilds_with_the_mailbox() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let mailbox_id = MailboxId::new(&account_id, "INBOX");

        cache
            .replace_messages(
                &mailbox_id,
                UidValidity(1),
                &[
                    searchable_summary(&mailbox_id, 1, "Snoozed subject", "ada@example.com", None),
                    searchable_summary(&mailbox_id, 2, "Visible subject", "bob@elsewhere.org", None),
                ],
            )
            .unwrap();

        cache.snooze_message(&mailbox_id, Uid(1), Utc::now() + chrono::Duration::hours(1)).unwrap();
        let hits = cache.search("subject", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].uid, Uid(2));

        // Replacing the mailbox's messages wipes its index rows: the snoozed
        // message no longer matches at all, and the mailbox's old hits are gone.
        cache
            .replace_messages(
                &mailbox_id,
                UidValidity(1),
                &[searchable_summary(&mailbox_id, 2, "Visible subject", "bob@elsewhere.org", None)],
            )
            .unwrap();
        assert_eq!(cache.search("snoozed", 10).unwrap().len(), 0);
        assert_eq!(cache.search("subject", 10).unwrap().len(), 1);

        // Deleting a message drops it from the index.
        cache.delete_message(&mailbox_id, Uid(2)).unwrap();
        assert!(cache.search("subject", 10).unwrap().is_empty());

        let path = cache_dir().join(format!("{}.sqlite3", sanitize_filename(&account_id)));
        let _ = std::fs::remove_file(path);
    }

    /// A cache written before the search index existed becomes searchable on
    /// open: the backfill reads the stored envelopes (and bodies) into an
    /// initially-empty `search_fts`, and a re-open with a populated index is a
    /// no-op (no duplicate rows).
    #[test]
    fn backfill_makes_a_pre_search_cache_searchable() {
        let account_id = temp_account_id();
        let mailbox_id = MailboxId::new(&account_id, "INBOX");

        // Create a database with envelopes + a body but no search index at all,
        // exactly what a pre-search build left on disk (user_version 3: the
        // envelope and body-format migrations done, the FTS index absent).
        let path = cache_dir().join(format!("{}.sqlite3", sanitize_filename(&account_id)));
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "
                PRAGMA user_version = 3;
                CREATE TABLE messages (
                    mailbox_id TEXT NOT NULL,
                    uid INTEGER NOT NULL,
                    uidvalidity INTEGER NOT NULL,
                    data TEXT NOT NULL,
                    PRIMARY KEY (mailbox_id, uid)
                );
                CREATE TABLE bodies (
                    mailbox_id TEXT NOT NULL,
                    uid INTEGER NOT NULL,
                    uidvalidity INTEGER NOT NULL,
                    data BLOB NOT NULL,
                    PRIMARY KEY (mailbox_id, uid)
                );
                CREATE TABLE snoozed (
                    mailbox_id TEXT NOT NULL,
                    uid INTEGER NOT NULL,
                    snoozed_until INTEGER NOT NULL,
                    PRIMARY KEY (mailbox_id, uid)
                );
                ",
            )
            .unwrap();
            let msg = searchable_summary(&mailbox_id, 1, "Ancient subject", "ada@example.com", Some("Old preview"));
            conn.execute(
                "INSERT INTO messages (mailbox_id, uid, uidvalidity, data) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![mailbox_id.0, 1u32, 1u32, serde_json::to_string(&msg).unwrap()],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO bodies (mailbox_id, uid, uidvalidity, data) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![mailbox_id.0, 1u32, 1u32, serde_json::to_vec(&sample_body("buried body term")).unwrap()],
            )
            .unwrap();
        }

        let cache = Cache::open(&account_id).unwrap();
        // The backfill is a separate step from `open` (run on the session's
        // worker thread, not the UI thread's read-side open - see `open`'s
        // note): search finds nothing until it runs.
        assert!(cache.search("ancient", 10).unwrap().is_empty(), "an un-backfilled index has no rows");
        cache.backfill_search_index().unwrap();
        assert_eq!(cache.search("ancient", 10).unwrap().len(), 1, "backfilled envelope subject");
        assert_eq!(cache.search("buried", 10).unwrap().len(), 1, "backfilled full body text");

        // Re-running the backfill is a cheap no-op (the index is populated):
        // nothing doubles up.
        cache.backfill_search_index().unwrap();
        assert_eq!(cache.search("ancient", 10).unwrap().len(), 1);

        let _ = std::fs::remove_file(path);
    }
}
