/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::Mutex;

use chrono::{DateTime, Local, Timelike, Utc};
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
/// is held across an `.await` point. The lock now also serves the session's
/// blocking-pool dispatch (see `session::cache_op`): each cache call runs on
/// a `spawn_blocking` thread, so the guard is genuinely contended there -
/// but only briefly, per operation, and it keeps SQLite work off the shared
/// async worker where a heavy write would stall every account's session.
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

/// Cap on the accumulated address book. The `addresses` table is deliberately
/// cumulative - addresses must survive after the envelopes that introduced
/// them leave the cache, or the composer's autocomplete would forget people -
/// but nothing ever pruned it, so it grew without bound. 20k covers a
/// lifetime of correspondents while keeping the autocomplete and the
/// dashboard's "most contacted" queries fast.
const ADDRESSES_CAP: usize = 20_000;

/// The fixed-seed hash of a mailbox id, used to derive every flat-file name
/// (attachment `.bin` sidecars and raw `.eml` exports). Shared by the store/
/// load paths and the purge sweep so the purge derives exactly the filenames
/// the store paths wrote.
fn mailbox_filename_hash(mailbox_id: &MailboxId) -> String {
    let mut hasher = DefaultHasher::new();
    mailbox_id.0.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Writes `bytes` to `path` atomically: write to a sibling temp file, then
/// rename over the target. A crash or a concurrent read mid-write can never
/// observe a truncated `.bin`/`.eml` file this way (`load_attachment`/
/// `load_raw_message` would otherwise be able to read a partial write and
/// cache a corrupt attachment). Mirrors the tmp+rename idiom already used
/// for the OAuth token stores (`microsoft_oauth.rs`, `google_tasks.rs`) and
/// the autostart file (`background.rs`); the pid suffix keeps concurrent
/// writers (different blocking-pool threads) from colliding on the same
/// temp name.
fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut tmp_name = path.as_os_str().to_os_string();
    tmp_name.push(format!(".tmp{}", std::process::id()));
    let tmp = PathBuf::from(tmp_name);
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
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

/// Removes one account's cached database and flat-file stores (on account
/// removal). A missing path is not an error - the account may never have
/// synced. Same "safe while the session is live" reasoning as
/// [`clear_all_caches`]: POSIX unlink doesn't disturb an already-open fd, so
/// this is safe to call even mid-teardown of the account's session actor.
pub fn remove_account_cache(account_id: &AccountId) -> Result<()> {
    let name = sanitize_filename(account_id);
    let db_path = cache_dir().join(format!("{name}.sqlite3"));
    match std::fs::remove_file(&db_path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    for sidecar_ext in ["sqlite3-wal", "sqlite3-shm"] {
        let _ = std::fs::remove_file(cache_dir().join(format!("{name}.{sidecar_ext}")));
    }
    let _ = std::fs::remove_dir_all(cache_dir().join("attachments").join(&name));
    let _ = std::fs::remove_dir_all(cache_dir().join("messages").join(&name));
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
/// body text. INSERT-only on purpose: callers route through
/// `index_upsert_message` (delete-then-insert), because FTS5 has no `UPDATE` -
/// and `INSERT OR REPLACE` replaces by rowid, while the implicit autoincrement
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
    // Size check before the expensive HTML strip: `strip_html_for_index`
    // parses the whole markup, and a body whose plain-text part alone is
    // already over the limit could never produce an index row - the text is
    // always included verbatim, so the result would exceed the limit too.
    if out.len() > FULL_BODY_INDEX_BYTES {
        return None;
    }
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
        // `synchronous=FULL` (the default) fsyncs on every autocommit
        // commit, and this file is rewritten wholesale by every mailbox sync.
        // The data is disposable - it's rebuilt from the server - so NORMAL
        // (sync only at WAL checkpoints, not per commit) is the right
        // durability point: a crash loses at worst the last sync's window,
        // which the next sync repopulates anyway.
        conn.pragma_update(None, "synchronous", "NORMAL")?;
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
            -- tokenized. Rows are keyed by `(mailbox_id, uid)` and every
            -- write goes through delete-then-insert (`index_upsert_message`,
            -- used by `replace_messages` for changed envelopes and by
            -- `store_body`), so FTS5's autoincrement rowid never leaks
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
        const BODY_CACHE_VERSION: i64 = 4;
        // Version 3: the whole-message fallback path (`parse_body`) used to
        // number attachment parts by enumerate counter ("0", "1", ...) instead
        // of their IMAP section paths, so cached bodies from those builds
        // carry part numbers no `UID FETCH BODY.PEEK[<n>]` can satisfy - a
        // save would silently hang. Wipe bodies once so the fixed builds
        // re-assemble them with real section paths. Envelope rows survive.
        // Version 4: `EmailBody` grew the `calendar_ics` field (the iMIP
        // `text/calendar` payload). Serde would deserialize old rows with
        // `calendar_ics: None`, but those rows were fetched *before* the
        // calendar part was requested - a cached invitation would never show
        // its banner. Wipe bodies once so every message is re-fetched with
        // the calendar part included.
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

    /// Keeps the cached envelope window for one mailbox in sync with `messages`
    /// - the mailbox's full current set, as assembled by `sync_mailbox` (itself
    ///   an incremental fetch: unchanged UIDs' `ENVELOPE`/`BODYSTRUCTURE` come
    ///   straight from this same cache rather than the network - see that
    ///   function's doc comment).
    ///
    /// The write is diff-based rather than wholesale: UIDs present are upserted
    /// (new rows inserted, changed envelopes updated in place), UIDs absent
    /// (expunged server-side, or an emptied mailbox) are deleted along with
    /// their search-index rows, and the index is re-indexed only for messages
    /// whose envelope actually changed. In steady state a sync changes little,
    /// so a wake that changed nothing rewrites nothing - instead of the former
    /// `DELETE FROM messages` + `DELETE FROM search_fts` + full re-INSERT that
    /// cost O(mailbox) on every IDLE wake.
    pub fn replace_messages(&self, mailbox_id: &MailboxId, uidvalidity: UidValidity, messages: &[EmailSummary]) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        // The stored membership, keyed by each row's `uidvalidity` - a UID
        // expunged here needs that validity to purge its `bodies` row and
        // flat files, both of which are validity-keyed. Key columns only, no
        // JSON parses, so the diff stays cheap relative to the data it's
        // diffing against.
        let mut stored: HashMap<u32, u32> = HashMap::new();
        {
            let mut stmt = tx.prepare("SELECT uid, uidvalidity FROM messages WHERE mailbox_id = ?1")?;
            let rows = stmt.query_map([&mailbox_id.0], |row| Ok((row.get::<_, u32>(0)?, row.get::<_, u32>(1)?)))?;
            for row in rows {
                let (uid, validity) = row?;
                stored.insert(uid, validity);
            }
        }
        // Upsert the new set. The `WHERE data IS NOT excluded.data` guard makes
        // the statement report zero changed rows when the stored envelope is
        // already byte-identical, so the search index is only rewritten for
        // messages that actually changed - and, crucially, a message's full-body
        // index text (the `store_body` upgrade) survives a re-sync of an
        // unchanged envelope instead of being downgraded back to the preview.
        let mut upsert = tx.prepare_cached(
            "INSERT INTO messages (mailbox_id, uid, uidvalidity, data) VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(mailbox_id, uid) DO UPDATE SET uidvalidity = excluded.uidvalidity, data = excluded.data \
             WHERE messages.data IS NOT excluded.data",
        )?;
        for msg in messages {
            let data = serde_json::to_string(msg)?;
            let modified = upsert.execute(rusqlite::params![mailbox_id.0, msg.uid.0, uidvalidity.0, data])?;
            if modified > 0 {
                // A new row, or a genuinely changed envelope (flags, preview, ...).
                // The envelope change never carries the body text `store_body`
                // indexed, so keep the existing index row's text - a flag-only
                // rewrite must not downgrade a full-body index back to the
                // preview. New messages index their preview.
                let existing_body: Option<String> = tx
                    .query_row(
                        "SELECT body FROM search_fts WHERE mailbox_id = ?1 AND uid = ?2",
                        rusqlite::params![mailbox_id.0, msg.uid.0],
                        |row| row.get(0),
                    )
                    .ok();
                let body = match existing_body.as_deref() {
                    Some(existing) if !existing.is_empty() => existing.to_string(),
                    _ => msg.preview.as_deref().unwrap_or("").to_string(),
                };
                index_upsert_message(&tx, msg, &body)?;
            }
        }
        // Delete the UIDs absent from the new set - expunged server-side, or
        // the whole-mailbox clear (`EmptyMailbox` passes an empty set) - and
        // their search rows, bodies, snooze entries, and flat files with
        // them. Bodies and the flat files are validity-keyed, so the stored
        // validity rides along.
        let present: HashSet<u32> = messages.iter().map(|m| m.uid.0).collect();
        let mut purged: HashSet<(u32, u32)> = HashSet::new();
        for (&uid, &stored_validity) in &stored {
            if !present.contains(&uid) {
                delete_index_row(&tx, mailbox_id, Uid(uid))?;
                tx.execute("DELETE FROM messages WHERE mailbox_id = ?1 AND uid = ?2", rusqlite::params![mailbox_id.0, uid])?;
                tx.execute("DELETE FROM bodies WHERE mailbox_id = ?1 AND uid = ?2", rusqlite::params![mailbox_id.0, uid])?;
                tx.execute("DELETE FROM snoozed WHERE mailbox_id = ?1 AND uid = ?2", rusqlite::params![mailbox_id.0, uid])?;
                purged.insert((uid, stored_validity));
            }
        }
        // One flat-file sweep covering the whole purge set (see that method):
        // a whole-mailbox clear removes every stored row.
        self.purge_message_files(mailbox_id, &purged);
        drop(upsert);
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

    /// Loads cached summaries for `mailbox_id`, keyed by UID, but only the
    /// ones cached under `uidvalidity` - a mismatch (the mailbox was
    /// recreated, RFC 3501 §2.3.1.1) means a cached UID no longer names the
    /// same message, so those rows must never be reused as-is. Used by
    /// `sync_mailbox`'s incremental refresh to tell which UIDs already have
    /// an ENVELOPE/BODYSTRUCTURE worth keeping - those never change once a
    /// message exists, so only a UID missing here needs a full re-fetch.
    pub fn load_messages_by_uid(&self, mailbox_id: &MailboxId, uidvalidity: UidValidity) -> Result<std::collections::HashMap<Uid, EmailSummary>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT data FROM messages WHERE mailbox_id = ?1 AND uidvalidity = ?2")?;
        let rows = stmt.query_map(rusqlite::params![mailbox_id.0, uidvalidity.0], |row| row.get::<_, String>(0))?;
        let mut messages = std::collections::HashMap::new();
        for row in rows {
            if let Ok(msg) = serde_json::from_str::<EmailSummary>(&row?) {
                messages.insert(msg.uid, msg);
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

    /// The batch counterpart of `load_summary` - one connection lock and one
    /// prepared statement reused per uid instead of one `load_summary` call
    /// per uid. Used by the session's coalesced on-demand body fetch to learn
    /// every queued message's `BODYSTRUCTURE`-derived part structure in a
    /// single pass. A uid with no cached summary is simply absent from the
    /// result, exactly as `load_summary` reports it as `None`.
    pub fn load_summaries(&self, mailbox_id: &MailboxId, uids: &[Uid]) -> Result<HashMap<Uid, EmailSummary>> {
        if uids.is_empty() {
            return Ok(HashMap::new());
        }
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT data FROM messages WHERE mailbox_id = ?1 AND uid = ?2")?;
        let mut out = HashMap::new();
        for &uid in uids {
            let mut rows = stmt.query_map(rusqlite::params![mailbox_id.0, uid.0], |row| row.get::<_, String>(0))?;
            if let Some(Ok(data)) = rows.next() {
                if let Ok(summary) = serde_json::from_str::<EmailSummary>(&data) {
                    out.insert(uid, summary);
                }
            }
        }
        Ok(out)
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

    /// Stores assembled bodies for a whole prefetch batch in one transaction -
    /// the batch counterpart of `store_body`, which cost one transaction per
    /// message. Replaces any earlier bodies for the same `(mailbox, uid)`
    /// pairs and upgrades each row's search-index text from the preview to
    /// the full text, exactly as `store_body` does per message.
    pub fn store_bodies(&self, mailbox_id: &MailboxId, uidvalidity: UidValidity, bodies: &[(Uid, EmailBody)]) -> Result<()> {
        if bodies.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare("INSERT OR REPLACE INTO bodies (mailbox_id, uid, uidvalidity, data) VALUES (?1, ?2, ?3, ?4)")?;
            for (uid, body) in bodies {
                let data = serde_json::to_vec(body)?;
                stmt.execute(rusqlite::params![mailbox_id.0, uid.0, uidvalidity.0, data])?;
            }
        }
        for (uid, body) in bodies {
            // Re-index the message with its full text, but only if the
            // envelope is cached (a stray body for a wiped envelope has no
            // subject/sender to index against), mirroring `store_body`.
            if let Some(summary) = load_summary_row(&tx, mailbox_id, *uid)? {
                let mut indexed = body_index_text(body).unwrap_or_default();
                if let Some(preview) = &summary.preview {
                    if !indexed.is_empty() {
                        indexed.push(' ');
                    }
                    indexed.push_str(preview);
                }
                index_upsert_message(&tx, &summary, &indexed)?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Returns `true` if a body for `(mailbox_id, uid, uidvalidity)` exists
    /// in the on-disk cache, without loading the (potentially large) payload.
    pub fn has_body(&self, mailbox_id: &MailboxId, uid: Uid, uidvalidity: UidValidity) -> Result<bool> {
        Ok(self.has_bodies(mailbox_id, std::slice::from_ref(&uid), uidvalidity)?.contains(&uid))
    }

    /// Returns the subset of `uids` that already have a cached body for
    /// `uidvalidity`. The prefetch pass filters its envelope batch against
    /// this before queuing body downloads; the per-uid `has_body` variant
    /// cost one SELECT per UID on a first sync of a big folder.
    pub fn has_bodies(&self, mailbox_id: &MailboxId, uids: &[Uid], uidvalidity: UidValidity) -> Result<HashSet<Uid>> {
        if uids.is_empty() {
            return Ok(HashSet::new());
        }
        let conn = self.conn.lock().unwrap();
        let wanted: HashSet<u32> = uids.iter().map(|u| u.0).collect();
        let mut stmt = conn.prepare("SELECT uid FROM bodies WHERE mailbox_id = ?1 AND uidvalidity = ?2")?;
        let rows = stmt.query_map(rusqlite::params![mailbox_id.0, uidvalidity.0], |row| row.get::<_, u32>(0))?;
        let mut found = HashSet::new();
        for row in rows {
            let uid = row?;
            if wanted.contains(&uid) {
                found.insert(Uid(uid));
            }
        }
        Ok(found)
    }

    /// The batch counterpart of `load_body` - one prepared statement reused
    /// per uid instead of one `load_body` call per uid. Used by the
    /// session's coalesced on-demand body fetch to serve whatever's already
    /// cached for a batch of requested messages with no network round trip
    /// at all. A uid with nothing cached (or cached under a different
    /// `uidvalidity`) is simply absent from the result.
    pub fn load_bodies(&self, mailbox_id: &MailboxId, uids: &[Uid], uidvalidity: UidValidity) -> Result<HashMap<Uid, EmailBody>> {
        if uids.is_empty() {
            return Ok(HashMap::new());
        }
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT data FROM bodies WHERE mailbox_id = ?1 AND uid = ?2 AND uidvalidity = ?3")?;
        let mut out = HashMap::new();
        for &uid in uids {
            let mut rows = stmt.query_map(rusqlite::params![mailbox_id.0, uid.0, uidvalidity.0], |row| row.get::<_, Vec<u8>>(0))?;
            if let Some(Ok(data)) = rows.next() {
                if let Ok(body) = serde_json::from_slice::<EmailBody>(&data) {
                    out.insert(uid, body);
                }
            }
        }
        Ok(out)
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
        self.attachments_dir
            .join(format!("{}-{}-{}-{part_number}.bin", mailbox_filename_hash(mailbox_id), uidvalidity.0, uid.0))
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
        write_atomic(&path, bytes)
    }

    /// The per-account flat-file path a whole raw RFC 5322 message's bytes are
    /// stored at, keyed exactly like `attachment_path` (mailbox identity via
    /// fixed-seed hash + `uidvalidity`/`uid`) but with an `.eml` extension.
    fn raw_message_path(&self, mailbox_id: &MailboxId, uid: Uid, uidvalidity: UidValidity) -> PathBuf {
        self.messages_dir.join(format!("{}-{}-{}.eml", mailbox_filename_hash(mailbox_id), uidvalidity.0, uid.0))
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
        write_atomic(&path, bytes)
    }

    /// Removes the flat-file cache for every `(uid, uidvalidity)` pair in
    /// `purged` - any attachment `.bin` sidecar (one file per fetched part)
    /// and the raw `.eml` export - in a single directory sweep. The
    /// `uidvalidity` is the value each message's envelope row was stored
    /// under, so a recycled uid under a newer validity is never pruned.
    ///
    /// Best-effort: missing files and unlink failures are ignored. The
    /// surrounding transaction has already deleted the DB rows; a leftover
    /// file is cosmetic cache garbage that the opportunistic sweep (a Phase 5
    /// item) and a UIDVALIDITY change both clean up eventually.
    fn purge_message_files(&self, mailbox_id: &MailboxId, purged: &HashSet<(u32, u32)>) {
        if purged.is_empty() {
            return;
        }
        let hash = mailbox_filename_hash(mailbox_id);
        // Attachment sidecars: `{hash}-{uidvalidity}-{uid}-{part_number}.bin`.
        // The part number is a variable suffix not known here, so match each
        // file's first three `-`-separated fields against the purge set - a
        // whole-mailbox clear removes every row, and a read_dir per uid would
        // be O(n²).
        if let Ok(entries) = std::fs::read_dir(&self.attachments_dir) {
            for entry in entries.flatten() {
                let Some(name) = entry.file_name().to_str().map(str::to_owned) else { continue };
                if !name.ends_with(".bin") {
                    continue;
                }
                let mut fields = name.split('-');
                let file_hash = fields.next();
                let validity = fields.next().and_then(|s| s.parse::<u32>().ok());
                let uid = fields.next().and_then(|s| s.parse::<u32>().ok());
                if file_hash == Some(hash.as_str()) && matches!((validity, uid), (Some(v), Some(u)) if purged.contains(&(u, v))) {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        // Raw `.eml` exports: exact deterministic paths, no scan needed.
        for &(uid, validity) in purged {
            let _ = std::fs::remove_file(self.raw_message_path(mailbox_id, Uid(uid), UidValidity(validity)));
        }
    }

    /// Deletes every attachment `.bin`/`.eml` flat file whose `(mailbox, uid,
    /// uidvalidity)` has no matching `messages` row anywhere in this
    /// account's cache. The backstop for files `purge_message_files` never
    /// got a chance to catch - one orphaned before that purge path covered
    /// `delete_messages`, or one left behind by a crash between a flat-file
    /// write and its `messages` row landing. One `read_dir` pass per
    /// flat-file directory (attachments, then raw messages), each file's
    /// name split and looked up in a set built from a single query - not an
    /// O(files) `read_dir` per uid.
    ///
    /// Best-effort like `purge_message_files`: a name that doesn't parse (or
    /// a `write_atomic` temp file mid-write, which never matches the `.bin`/
    /// `.eml` suffix in the first place) is left alone, and unlink failures
    /// are ignored - worst case a stale file survives to the next sweep, or
    /// a live file is deleted and re-fetched from the server on next use.
    /// Called once per session start (see `session.rs`), never inline with
    /// a sync.
    fn sweep_orphaned_files(&self) {
        let known: HashSet<(String, u32, u32)> = {
            let conn = self.conn.lock().unwrap();
            let Ok(mut stmt) = conn.prepare("SELECT DISTINCT mailbox_id, uidvalidity, uid FROM messages") else {
                return;
            };
            let Ok(rows) = stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, u32>(1)?, row.get::<_, u32>(2)?))) else {
                return;
            };
            rows.flatten()
                .map(|(mailbox_id, validity, uid)| (mailbox_filename_hash(&MailboxId(mailbox_id)), validity, uid))
                .collect()
        };
        for (dir, ext) in [(&self.attachments_dir, ".bin"), (&self.messages_dir, ".eml")] {
            let Ok(entries) = std::fs::read_dir(dir) else { continue };
            for entry in entries.flatten() {
                let Some(name) = entry.file_name().to_str().map(str::to_owned) else { continue };
                if !name.ends_with(ext) {
                    continue;
                }
                // Strip the extension before splitting: `.bin`'s stem ends
                // in `-<part_number>` but `.eml`'s ends bare (`...-<uid>`),
                // so splitting the raw filename would swallow the `.eml`
                // suffix into the uid field and fail to parse.
                let stem = &name[..name.len() - ext.len()];
                let mut fields = stem.split('-');
                let Some(hash) = fields.next() else { continue };
                let validity = fields.next().and_then(|s| s.parse::<u32>().ok());
                let uid = fields.next().and_then(|s| s.parse::<u32>().ok());
                let is_orphan = matches!((validity, uid), (Some(v), Some(u)) if !known.contains(&(hash.to_string(), v, u)));
                if is_orphan {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }

    /// One-time migration plus periodic reclaim, meant to be called once per
    /// session start on the blocking pool (the same fire-and-forget slot
    /// `session.rs` already uses for `backfill_search_index`) - never from
    /// `Cache::open` itself, so an account with a large existing cache never
    /// pays this cost before the first connect attempt.
    ///
    /// `auto_vacuum` only takes effect via a full `VACUUM` on a database
    /// that already has tables (SQLite's own rule), so an install upgrading
    /// from a pre-incremental-vacuum cache pays that `VACUUM` exactly once;
    /// every `Cache` opened afterward reads `auto_vacuum` back as
    /// `INCREMENTAL` and skips straight to the cheap `PRAGMA
    /// incremental_vacuum`, which only moves already-freed pages (from
    /// expunged messages, purged bodies, `delete_messages`' and
    /// `sweep_orphaned_files`'s flat-file purges, ...) to the end of the
    /// file so the OS can reclaim them - no full-file rewrite.
    pub fn run_maintenance(&self) -> Result<()> {
        self.sweep_orphaned_files();
        let conn = self.conn.lock().unwrap();
        const INCREMENTAL: i64 = 2;
        let mode: i64 = conn.query_row("PRAGMA auto_vacuum", [], |row| row.get(0))?;
        if mode != INCREMENTAL {
            conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
            conn.execute("VACUUM", [])?;
        }
        conn.execute("PRAGMA incremental_vacuum", [])?;
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
    ///
    /// The table only ever grows (addresses deliberately survive the envelopes
    /// that introduced them), so once it passes [`ADDRESSES_CAP`] the
    /// lowest-ranked overflow is pruned in the same transaction - the
    /// least-contacted addresses (fewest lifetime appearances, ties broken by
    /// most-recent contact) go first, so the composer's autocomplete and the
    /// dashboard's "most contacted" feed keep their top ranks.
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
        // Bound the table once it passes the cap. The COUNT is cheap next to
        // the batch's upserts on the blocking pool; the prune itself is the
        // rare path.
        let count: i64 = tx.query_row("SELECT COUNT(*) FROM addresses", [], |row| row.get(0))?;
        if count > ADDRESSES_CAP as i64 {
            tx.execute(
                "DELETE FROM addresses WHERE address IN (
                     SELECT address FROM addresses
                     ORDER BY seen_count DESC, last_seen DESC
                     LIMIT -1 OFFSET ?1
                 )",
                rusqlite::params![ADDRESSES_CAP as i64],
            )?;
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

    /// The top `limit` correspondents by lifetime appearances across the
    /// cached envelopes (from/to/cc combined), most-contacted first, with
    /// each person's count - the Lookout dashboard's "People most contacted"
    /// feed. Reuses the `addresses` table the composer's autocomplete ranks,
    /// so no extra indexing is needed: `addresses_by_count` already orders by
    /// `seen_count` (ties broken by `last_seen`, matching the autocomplete).
    pub fn top_addresses(&self, limit: usize) -> Result<Vec<(EmailAddress, i64)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT address, name, seen_count FROM addresses
             ORDER BY seen_count DESC, last_seen DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], |row| {
            Ok((
                EmailAddress {
                    address: row.get::<_, String>(0)?,
                    name: row.get::<_, Option<String>>(1)?.filter(|n| !n.trim().is_empty()),
                },
                row.get::<_, i64>(2)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Counts every cached message's date by hour of day, bucketed in the
    /// user's *local* timezone - the Lookout dashboard's "Emails by time of
    /// day" feed. `since` optionally limits the window to messages at or
    /// after that instant (`None` covers everything the cache holds). The
    /// bucketing happens in Rust rather than SQL `strftime` so the hours
    /// stay accurate across DST changes; SQL only extracts the stored
    /// RFC 3339 strings and filters them lexicographically, which is valid
    /// because every cached date serializes in UTC with a trailing `Z`.
    /// Rows whose date can't be parsed are skipped.
    pub fn hour_histogram(&self, since: Option<DateTime<Utc>>) -> Result<[i64; 24]> {
        let conn = self.conn.lock().unwrap();
        // The cut is passed as the stored format (RFC 3339 UTC), which sorts
        // lexicographically like the serialized dates themselves.
        let (sql, since_string) = match since {
            Some(since) => (
                "SELECT json_extract(data, '$.date') FROM messages WHERE json_extract(data, '$.date') >= ?1",
                Some(since.to_rfc3339()),
            ),
            None => ("SELECT json_extract(data, '$.date') FROM messages", None),
        };
        let mut stmt = conn.prepare(sql)?;
        let params: Vec<String> = since_string.into_iter().collect();
        let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |row| row.get::<_, String>(0))?;
        let mut histogram = [0i64; 24];
        for row in rows {
            let Ok(date_string) = row else { continue };
            let Ok(date) = DateTime::parse_from_rfc3339(&date_string) else { continue };
            histogram[date.with_timezone(&Local).hour() as usize] += 1;
        }
        Ok(histogram)
    }

    /// Full-text search over this account's cached envelopes and bodies.    /// Runs entirely against the local index (no IMAP round trip), returning
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

        // Single FTS→messages JOIN instead of one SELECT + JSON parse per
        // hit: a keystroke-driven search can surface hundreds of hits, and
        // the UI thread runs this on every keystroke. Hits whose envelope row
        // is gone (e.g. the migration wiped the messages table once) fall out
        // of the JOIN rather than being skipped one by one.
        let mut stmt = conn.prepare(
            "SELECT messages.data FROM search_fts \
             JOIN messages ON messages.mailbox_id = search_fts.mailbox_id \
              AND messages.uid = search_fts.uid \
             WHERE search_fts MATCH ?1 ORDER BY rank LIMIT ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![match_query, limit as i64], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        let mut data_by_key: Vec<(String, u32, String)> = Vec::new();
        for row in rows {
            let data = row?;
            let Ok(msg) = serde_json::from_str::<EmailSummary>(&data) else {
                continue;
            };
            data_by_key.push((msg.mailbox.0.clone(), msg.uid.0, data));
        }

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

        for (mailbox, uid, data) in data_by_key {
            if snoozed.contains(&(mailbox, uid)) {
                continue;
            }
            out.push(serde_json::from_str(&data)?);
        }
        Ok(out)
    }

    /// The newest cached messages across every mailbox of this
    /// account, most-recent-first - the assistant's "recent mail" feed (the
    /// Lookout tab's chat can summarize the inbox without touching the
    /// network, since it only sees what's already been synced). The stored
    /// dates are RFC 3339 UTC with a trailing `Z`, so the SQL order is
    /// lexicographic; rows whose date can't be parsed are skipped. Snoozed
    /// messages stay hidden, matching what the message list and `search`
    /// show, so the result can fall short of `limit` when the newest rows
    /// are snoozed.
    pub fn recent_messages(&self, limit: usize) -> Result<Vec<EmailSummary>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT data FROM messages \
             ORDER BY json_extract(data, '$.date') DESC \
             LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit as i64], |row| row.get::<_, String>(0))?;
        let mut data_by_key: Vec<(String, u32, String)> = Vec::new();
        for row in rows {
            let Ok(data) = row else { continue };
            let Ok(msg) = serde_json::from_str::<EmailSummary>(&data) else {
                continue;
            };
            data_by_key.push((msg.mailbox.0.clone(), msg.uid.0, data));
        }

        // Same active-snooze set as `search`, so a snoozed message doesn't
        // surface in the assistant's feed either.
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
        for (mailbox, uid, data) in data_by_key {
            if snoozed.contains(&(mailbox, uid)) {
                continue;
            }
            out.push(serde_json::from_str(&data)?);
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

    /// Applies a flag change to every cached summary in `uids` at once,
    /// mirroring a joined `STORE` against the server. Same contract as
    /// `update_flags` per uid: returns `false` if any uid is missing from the
    /// cached window, so the caller's all-or-resync fallback still triggers.
    ///
    /// All the row reads and writes run inside one transaction (and prepared
    /// statements are reused), so the per-uid variant's autocommit-per-message
    /// cost is paid once for the whole batch instead of once per message.
    pub fn update_flags_many(&self, mailbox_id: &MailboxId, uids: &[Uid], add: &[SystemFlagBit], remove: &[SystemFlagBit]) -> Result<bool> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let mut all_found = true;
        {
            let mut select = tx.prepare("SELECT data FROM messages WHERE mailbox_id = ?1 AND uid = ?2")?;
            let mut update = tx.prepare("UPDATE messages SET data = ?1 WHERE mailbox_id = ?2 AND uid = ?3")?;
            for uid in uids {
                let mut rows = select.query_map(rusqlite::params![mailbox_id.0, uid.0], |row| row.get::<_, String>(0))?;
                let Some(data) = rows.next().transpose()? else {
                    all_found = false;
                    continue;
                };
                let Ok(mut summary) = serde_json::from_str::<EmailSummary>(&data) else {
                    all_found = false;
                    continue;
                };
                for flag in add {
                    summary.flags.insert(*flag);
                }
                for flag in remove {
                    summary.flags.remove(flag);
                }
                let data = serde_json::to_string(&summary)?;
                update.execute(rusqlite::params![data, mailbox_id.0, uid.0])?;
            }
        }
        tx.commit()?;
        Ok(all_found)
    }

    /// Patches in a fetched preview snippet for each uid in `previews`,
    /// without the whole-mailbox diff/upsert/expunge-purge `replace_messages`
    /// does - `fetch_previews` calls this for the up-to-`PREVIEW_FETCH_LIMIT`
    /// messages it just fetched a snippet for, not the whole mailbox.
    ///
    /// Deliberately does not touch `search_fts`: unlike `replace_messages`,
    /// which re-indexes a changed row to preserve or seed the search index,
    /// these messages are only ever missing a preview because they've never
    /// been indexed with a body either - they stay unsearchable-by-content
    /// until the next full sync's `replace_messages` catches them up. A
    /// bounded staleness window (one sync cycle), not a permanent gap, and
    /// the same asymmetry `update_flags_many`/`update_keywords` already have.
    ///
    /// Same all-or-resync contract as `update_flags_many`: returns `false` if
    /// any uid is missing from the cached window (e.g. expunged between the
    /// fetch and this write) - the caller only logs it, since a missing
    /// preview for a message that no longer exists needs no reconciliation.
    pub fn update_previews(&self, mailbox_id: &MailboxId, previews: &HashMap<Uid, String>) -> Result<bool> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let mut all_found = true;
        {
            let mut select = tx.prepare("SELECT data FROM messages WHERE mailbox_id = ?1 AND uid = ?2")?;
            let mut update = tx.prepare("UPDATE messages SET data = ?1 WHERE mailbox_id = ?2 AND uid = ?3")?;
            for (uid, preview) in previews {
                let mut rows = select.query_map(rusqlite::params![mailbox_id.0, uid.0], |row| row.get::<_, String>(0))?;
                let Some(data) = rows.next().transpose()? else {
                    all_found = false;
                    continue;
                };
                let Ok(mut summary) = serde_json::from_str::<EmailSummary>(&data) else {
                    all_found = false;
                    continue;
                };
                summary.preview = Some(preview.clone());
                let data = serde_json::to_string(&summary)?;
                update.execute(rusqlite::params![data, mailbox_id.0, uid.0])?;
            }
        }
        tx.commit()?;
        Ok(all_found)
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

    /// Applies a keyword change to every cached summary in `uids` at once -
    /// the batch counterpart of `update_keywords`, same all-or-nothing
    /// contract as `update_flags_many` and the same one-transaction shape.
    pub fn update_keywords_many(&self, mailbox_id: &MailboxId, uids: &[Uid], add: &[String], remove: &[String]) -> Result<bool> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let mut all_found = true;
        {
            let mut select = tx.prepare("SELECT data FROM messages WHERE mailbox_id = ?1 AND uid = ?2")?;
            let mut update = tx.prepare("UPDATE messages SET data = ?1 WHERE mailbox_id = ?2 AND uid = ?3")?;
            for uid in uids {
                let mut rows = select.query_map(rusqlite::params![mailbox_id.0, uid.0], |row| row.get::<_, String>(0))?;
                let Some(data) = rows.next().transpose()? else {
                    all_found = false;
                    continue;
                };
                let Ok(mut summary) = serde_json::from_str::<EmailSummary>(&data) else {
                    all_found = false;
                    continue;
                };
                for keyword in add {
                    summary.keywords.insert(keyword.clone());
                }
                for keyword in remove {
                    summary.keywords.remove(keyword);
                }
                let data = serde_json::to_string(&summary)?;
                update.execute(rusqlite::params![data, mailbox_id.0, uid.0])?;
            }
        }
        tx.commit()?;
        Ok(all_found)
    }

    /// Removes a single message (plus its cached body and any snooze entry)
    /// from the cache. Used right after a successful MOVE so the deleted or
    /// archived message drops out of the next `MessagesUpdated` immediately
    /// instead of waiting for the authoritative resync to re-fetch the whole
    /// window. The next `replace_messages` wipes the window anyway, so this is
    /// a display-latency optimization, never the source of truth.
    pub fn delete_message(&self, mailbox_id: &MailboxId, uid: Uid) -> Result<()> {
        self.delete_messages(mailbox_id, &[uid])
    }

    /// Removes several messages (plus their cached bodies, snooze entries,
    /// search-index rows, and flat files) in one transaction - the batch
    /// counterpart of `delete_message`, used by the move paths so a
    /// multi-message move pays one commit instead of one per message.
    ///
    /// Unlike `replace_messages`'s expunge diff, this is the only path that
    /// deletes a message outside of a resync (a MOVE deletes it from the
    /// cache immediately, before the next sync would otherwise notice it's
    /// gone) - so it must purge that message's flat files itself; once the
    /// `messages` row is gone, `replace_messages`'s stored-vs-present diff
    /// can no longer see the uid to purge it for us.
    pub fn delete_messages(&self, mailbox_id: &MailboxId, uids: &[Uid]) -> Result<()> {
        if uids.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let mut purged: HashSet<(u32, u32)> = HashSet::new();
        {
            let mut validity_of = tx.prepare_cached("SELECT uidvalidity FROM messages WHERE mailbox_id = ?1 AND uid = ?2")?;
            let mut messages = tx.prepare_cached("DELETE FROM messages WHERE mailbox_id = ?1 AND uid = ?2")?;
            let mut bodies = tx.prepare_cached("DELETE FROM bodies WHERE mailbox_id = ?1 AND uid = ?2")?;
            let mut snoozed = tx.prepare_cached("DELETE FROM snoozed WHERE mailbox_id = ?1 AND uid = ?2")?;
            let mut fts = tx.prepare_cached("DELETE FROM search_fts WHERE mailbox_id = ?1 AND uid = ?2")?;
            for uid in uids {
                if let Ok(validity) = validity_of.query_row(rusqlite::params![mailbox_id.0, uid.0], |row| row.get::<_, u32>(0)) {
                    purged.insert((uid.0, validity));
                }
                messages.execute(rusqlite::params![mailbox_id.0, uid.0])?;
                bodies.execute(rusqlite::params![mailbox_id.0, uid.0])?;
                snoozed.execute(rusqlite::params![mailbox_id.0, uid.0])?;
                fts.execute(rusqlite::params![mailbox_id.0, uid.0])?;
            }
        }
        self.purge_message_files(mailbox_id, &purged);
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

    /// Records a snooze for every uid in `uids` in one transaction - the batch
    /// counterpart of `snooze_message` for `SnoozeMessages`.
    pub fn snooze_messages(&self, mailbox_id: &MailboxId, uids: &[Uid], until: DateTime<Utc>) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare("INSERT OR REPLACE INTO snoozed (mailbox_id, uid, snoozed_until) VALUES (?1, ?2, ?3)")?;
            for uid in uids {
                stmt.execute(rusqlite::params![mailbox_id.0, uid.0, until.timestamp()])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Returns every uid in `mailbox_id` still snoozed as of `now`. Pure
    /// read - safe to call synchronously off the GTK main thread (a plain
    /// indexed SELECT, no write lock contention). Expiry is filtered here
    /// directly rather than relying on a prior `purge_expired_snoozed` call,
    /// so correctness doesn't depend on that cleanup having run recently.
    pub fn active_snoozed_uids(&self, mailbox_id: &MailboxId, now: DateTime<Utc>) -> Result<HashSet<Uid>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT uid FROM snoozed WHERE mailbox_id = ?1 AND snoozed_until > ?2")?;
        let rows = stmt.query_map(rusqlite::params![mailbox_id.0, now.timestamp()], |row| row.get::<_, u32>(0))?;
        let mut uids = HashSet::new();
        for row in rows {
            uids.insert(Uid(row?));
        }
        Ok(uids)
    }

    /// Deletes every snoozed-message row (account-wide) whose wake time has
    /// passed. Cheap housekeeping meant to run alongside cache work already
    /// off the GTK thread; kept separate from `active_snoozed_uids` so that
    /// read can be called synchronously from the UI thread.
    pub fn purge_expired_snoozed(&self, now: DateTime<Utc>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM snoozed WHERE snoozed_until <= ?1", rusqlite::params![now.timestamp()])?;
        Ok(())
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

    #[test]
    fn purge_expired_snoozed_removes_only_expired_rows() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let mailbox_id = MailboxId::new(&account_id, "INBOX");
        let now = Utc::now();

        cache.snooze_message(&mailbox_id, Uid(1), now + chrono::Duration::hours(1)).unwrap();
        cache.snooze_message(&mailbox_id, Uid(2), now - chrono::Duration::hours(1)).unwrap();

        cache.purge_expired_snoozed(now).unwrap();

        let conn = cache.conn.lock().unwrap();
        let remaining: i64 = conn.query_row("SELECT COUNT(*) FROM snoozed", [], |row| row.get(0)).unwrap();
        assert_eq!(remaining, 1);
        let remaining_uid: u32 = conn.query_row("SELECT uid FROM snoozed", [], |row| row.get(0)).unwrap();
        assert_eq!(remaining_uid, 1);
        drop(conn);

        let path = cache_dir().join(format!("{}.sqlite3", sanitize_filename(&account_id)));
        let _ = std::fs::remove_file(path);
    }

    /// The `SnoozeMessages` contract: one `snooze_messages` call records every
    /// listed uid, and re-snoozing any of them updates the wake time instead
    /// of erroring or duplicating rows.
    #[test]
    fn snoozes_a_batch_of_messages_at_once() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let mailbox_id = MailboxId::new(&account_id, "INBOX");
        let now = Utc::now();

        cache.snooze_messages(&mailbox_id, &[Uid(1), Uid(2), Uid(3)], now + chrono::Duration::hours(1)).unwrap();
        assert_eq!(cache.active_snoozed_uids(&mailbox_id, now).unwrap(), HashSet::from([Uid(1), Uid(2), Uid(3)]));

        cache.snooze_messages(&mailbox_id, &[Uid(1), Uid(4)], now - chrono::Duration::hours(1)).unwrap();
        assert_eq!(cache.active_snoozed_uids(&mailbox_id, now).unwrap(), HashSet::from([Uid(2), Uid(3)]));

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
        cache.store_attachment(&mailbox_id, Uid(1), UidValidity(1), "2", b"gone pdf").unwrap();
        cache.store_raw_message(&mailbox_id, Uid(1), UidValidity(1), b"Subject: gone\r\n\r\nx").unwrap();
        cache.store_attachment(&mailbox_id, Uid(2), UidValidity(1), "2", b"kept pdf").unwrap();

        cache.delete_message(&mailbox_id, Uid(1)).unwrap();

        let remaining = cache.load_messages(&mailbox_id).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].uid, Uid(2));
        assert!(!cache.has_body(&mailbox_id, Uid(1), UidValidity(1)).unwrap());
        assert!(cache.has_body(&mailbox_id, Uid(2), UidValidity(1)).unwrap());
        assert_eq!(cache.active_snoozed_uids(&mailbox_id, Utc::now()).unwrap(), HashSet::from([Uid(2)]));
        assert!(
            cache.load_attachment(&mailbox_id, Uid(1), UidValidity(1), "2").unwrap().is_none(),
            "the deleted message's attachment .bin is purged, not left orphaned"
        );
        assert!(
            cache.load_raw_message(&mailbox_id, Uid(1), UidValidity(1)).unwrap().is_none(),
            "the deleted message's .eml is purged, not left orphaned"
        );
        assert_eq!(
            cache.load_attachment(&mailbox_id, Uid(2), UidValidity(1), "2").unwrap(),
            Some(b"kept pdf".to_vec()),
            "the surviving message's attachment is untouched"
        );

        // Deleting a uid the cache doesn't know is fine - used when the moved
        // message fell outside the cached window.
        cache.delete_message(&mailbox_id, Uid(99)).unwrap();
        assert_eq!(cache.load_messages(&mailbox_id).unwrap().len(), 1);

        let path = cache_dir().join(format!("{}.sqlite3", sanitize_filename(&account_id)));
        let _ = std::fs::remove_file(path);
    }

    /// The batch counterpart of `deleting_a_message_drops_it_and_its_body_from_the_cache`:
    /// one `delete_messages` call drops every listed uid's envelope, body and
    /// snooze entry, and mixed-in unknown uids are harmless no-ops.
    #[test]
    fn deleting_messages_drops_them_in_one_batch() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let mailbox_id = MailboxId::new(&account_id, "INBOX");

        cache
            .replace_messages(
                &mailbox_id,
                UidValidity(1),
                &[
                    sample_summary(&mailbox_id, 1, None),
                    sample_summary(&mailbox_id, 2, None),
                    sample_summary(&mailbox_id, 3, None),
                ],
            )
            .unwrap();
        cache.store_body(&mailbox_id, Uid(1), UidValidity(1), &sample_body("one")).unwrap();
        cache.store_body(&mailbox_id, Uid(3), UidValidity(1), &sample_body("three")).unwrap();
        cache.snooze_message(&mailbox_id, Uid(3), Utc::now() + chrono::Duration::hours(1)).unwrap();
        cache.store_attachment(&mailbox_id, Uid(1), UidValidity(1), "2", b"gone pdf").unwrap();
        cache.store_raw_message(&mailbox_id, Uid(3), UidValidity(1), b"Subject: gone\r\n\r\nx").unwrap();

        cache.delete_messages(&mailbox_id, &[Uid(1), Uid(3), Uid(99)]).unwrap();

        let remaining = cache.load_messages(&mailbox_id).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].uid, Uid(2));
        assert!(!cache.has_body(&mailbox_id, Uid(1), UidValidity(1)).unwrap());
        assert!(!cache.has_body(&mailbox_id, Uid(3), UidValidity(1)).unwrap());
        assert!(cache.active_snoozed_uids(&mailbox_id, Utc::now()).unwrap().is_empty());
        assert!(
            cache.load_attachment(&mailbox_id, Uid(1), UidValidity(1), "2").unwrap().is_none(),
            "a batch-deleted message's attachment .bin is purged, not left orphaned"
        );
        assert!(
            cache.load_raw_message(&mailbox_id, Uid(3), UidValidity(1)).unwrap().is_none(),
            "a batch-deleted message's .eml is purged, not left orphaned"
        );

        let path = cache_dir().join(format!("{}.sqlite3", sanitize_filename(&account_id)));
        let _ = std::fs::remove_file(path);
    }

    /// `run_maintenance`'s sweep is the backstop for flat files that have no
    /// `messages` row at all - an attachment/raw message stored for a uid
    /// that was never (or is no longer) part of the cached mailbox, the case
    /// `purge_message_files`'s resync diff and `delete_messages`' explicit
    /// purge can't reach because neither one ever ran for that uid. A file
    /// backed by a real row must survive the same sweep.
    #[test]
    fn run_maintenance_sweeps_flat_files_with_no_matching_row() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let mailbox_id = MailboxId::new(&account_id, "INBOX");

        cache.replace_messages(&mailbox_id, UidValidity(1), &[sample_summary(&mailbox_id, 1, None)]).unwrap();
        cache.store_attachment(&mailbox_id, Uid(1), UidValidity(1), "2", b"kept pdf").unwrap();
        cache.store_raw_message(&mailbox_id, Uid(1), UidValidity(1), b"Subject: kept\r\n\r\nx").unwrap();

        // Uid 99 has flat files but was never given a `messages` row -
        // exactly what a crash between the flat-file write and the row
        // landing (or a pre-fix `delete_messages`) would leave behind.
        cache.store_attachment(&mailbox_id, Uid(99), UidValidity(1), "2", b"orphan pdf").unwrap();
        cache.store_raw_message(&mailbox_id, Uid(99), UidValidity(1), b"Subject: orphan\r\n\r\nx").unwrap();

        cache.run_maintenance().unwrap();

        assert_eq!(
            cache.load_attachment(&mailbox_id, Uid(1), UidValidity(1), "2").unwrap(),
            Some(b"kept pdf".to_vec()),
            "a flat file backed by a real row survives the sweep"
        );
        assert!(cache.load_raw_message(&mailbox_id, Uid(1), UidValidity(1)).unwrap().is_some());
        assert!(
            cache.load_attachment(&mailbox_id, Uid(99), UidValidity(1), "2").unwrap().is_none(),
            "an attachment with no matching messages row is swept"
        );
        assert!(
            cache.load_raw_message(&mailbox_id, Uid(99), UidValidity(1)).unwrap().is_none(),
            "a raw message with no matching messages row is swept"
        );

        let conn = cache.conn.lock().unwrap();
        let mode: i64 = conn.query_row("PRAGMA auto_vacuum", [], |row| row.get(0)).unwrap();
        assert_eq!(mode, 2, "run_maintenance migrates the database to incremental auto_vacuum");
        drop(conn);

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
            has_calendar: false,
            preview: preview.map(|p| p.to_string()),
            structure: None,
        }
    }

    fn sample_body(text: &str) -> EmailBody {
        EmailBody {
            uid: Uid(0),
            text_body: Some(text.to_string()),
            html_body: None,
            calendar_ics: None,
            parts: Vec::new(),
            headers: Vec::new(),
            auth_results: None,
        }
    }

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

    #[test]
    fn update_previews_patches_only_the_given_uids_and_survives_reload() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let mailbox_id = MailboxId::new(&account_id, "INBOX");

        let messages = vec![
            sample_summary(&mailbox_id, 1, None),
            sample_summary(&mailbox_id, 2, None),
            sample_summary(&mailbox_id, 3, Some("already had one")),
        ];
        cache.replace_messages(&mailbox_id, UidValidity(1), &messages).unwrap();

        let previews = HashMap::from([(Uid(1), "new snippet for 1".to_string())]);
        assert!(cache.update_previews(&mailbox_id, &previews).unwrap());

        let reloaded = cache.load_messages_by_uid(&mailbox_id, UidValidity(1)).unwrap();
        assert_eq!(reloaded[&Uid(1)].preview.as_deref(), Some("new snippet for 1"));
        // Untouched uids keep exactly what they had, including their other
        // fields (subject is uid-derived by `sample_summary`, so this also
        // confirms the patch didn't clobber the row with a different uid's
        // data).
        assert_eq!(reloaded[&Uid(2)].preview, None);
        assert_eq!(reloaded[&Uid(2)].subject.as_deref(), Some("subject 2"));
        assert_eq!(reloaded[&Uid(3)].preview.as_deref(), Some("already had one"));

        let path = cache_dir().join(format!("{}.sqlite3", sanitize_filename(&account_id)));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn update_previews_reports_a_uid_missing_from_the_cached_window() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let mailbox_id = MailboxId::new(&account_id, "INBOX");

        let messages = vec![sample_summary(&mailbox_id, 1, None)];
        cache.replace_messages(&mailbox_id, UidValidity(1), &messages).unwrap();

        // Uid 99 was never synced into this mailbox (e.g. expunged between
        // the preview fetch and this write).
        let previews = HashMap::from([(Uid(1), "ok".to_string()), (Uid(99), "gone".to_string())]);
        assert!(!cache.update_previews(&mailbox_id, &previews).unwrap());

        let reloaded = cache.load_messages_by_uid(&mailbox_id, UidValidity(1)).unwrap();
        assert_eq!(reloaded[&Uid(1)].preview.as_deref(), Some("ok"));
        assert_eq!(reloaded.len(), 1, "no row should have been created for the missing uid");

        let path = cache_dir().join(format!("{}.sqlite3", sanitize_filename(&account_id)));
        let _ = std::fs::remove_file(path);
    }

    /// The stickiness `sync_mailbox`'s incremental refresh depends on: a
    /// cached summary (envelope, structure, preview - everything but flags)
    /// is readable back keyed by uid, so a UID already known under the
    /// current `uidvalidity` never needs its `ENVELOPE`/`BODYSTRUCTURE`
    /// refetched - only a cheap `FLAGS` lookup. A uidvalidity mismatch (the
    /// mailbox was recreated) must yield nothing, the same guarantee
    /// `load_body`/`load_attachment` give elsewhere - a stale UID can never
    /// be reused as if it named the same message.
    #[test]
    fn load_messages_by_uid_respects_uidvalidity() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let mailbox_id = MailboxId::new(&account_id, "INBOX");

        let messages = vec![sample_summary(&mailbox_id, 1, Some("preview one")), sample_summary(&mailbox_id, 2, None)];
        cache.replace_messages(&mailbox_id, UidValidity(7), &messages).unwrap();

        let by_uid = cache.load_messages_by_uid(&mailbox_id, UidValidity(7)).unwrap();
        assert_eq!(by_uid.len(), 2);
        assert_eq!(by_uid.get(&Uid(1)).and_then(|m| m.preview.as_deref()), Some("preview one"));
        assert_eq!(by_uid.get(&Uid(2)).and_then(|m| m.subject.as_deref()), Some("subject 2"));

        // A different uidvalidity - as if the mailbox had been recreated -
        // must not resolve any of the same UIDs to this cached data.
        assert!(cache.load_messages_by_uid(&mailbox_id, UidValidity(8)).unwrap().is_empty());

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
        assert!(loaded.is_pinned());

        assert!(cache.update_flags(&mailbox_id, Uid(1), &[], &[SystemFlagBit::Flagged]).unwrap());
        let loaded = &cache.load_messages(&mailbox_id).unwrap()[0];
        assert!(!loaded.is_unread());
        assert!(!loaded.is_pinned());

        // A uid that isn't in the cached window is a no-op, not an error.
        assert!(!cache.update_flags(&mailbox_id, Uid(99), &[SystemFlagBit::Seen], &[]).unwrap());

        let path = cache_dir().join(format!("{}.sqlite3", sanitize_filename(&account_id)));
        let _ = std::fs::remove_file(path);
    }

    /// The joined `STORE` contract: `update_flags_many` patches every listed
    /// uid in one transaction, leaves an untargeted uid alone, and reports
    /// `false` when any uid is outside the cached window so the session's
    /// all-or-resync fallback still fires.
    #[test]
    fn patches_flags_on_many_cached_summaries_at_once() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let mailbox_id = MailboxId::new(&account_id, "INBOX");

        cache
            .replace_messages(
                &mailbox_id,
                UidValidity(1),
                &[
                    sample_summary(&mailbox_id, 1, None),
                    sample_summary(&mailbox_id, 2, None),
                    sample_summary(&mailbox_id, 3, None),
                ],
            )
            .unwrap();

        assert!(cache
            .update_flags_many(&mailbox_id, &[Uid(1), Uid(2)], &[SystemFlagBit::Seen, SystemFlagBit::Flagged], &[])
            .unwrap());
        let loaded = cache.load_messages(&mailbox_id).unwrap();
        assert!(!loaded.iter().find(|m| m.uid == Uid(1)).unwrap().is_unread());
        assert!(!loaded.iter().find(|m| m.uid == Uid(2)).unwrap().is_unread());
        assert!(loaded.iter().find(|m| m.uid == Uid(3)).unwrap().is_unread());
        assert!(loaded.iter().find(|m| m.uid == Uid(1)).unwrap().is_pinned());
        assert!(!loaded.iter().find(|m| m.uid == Uid(3)).unwrap().is_pinned());

        // One uid outside the cached window fails the whole batch.
        assert!(!cache.update_flags_many(&mailbox_id, &[Uid(2), Uid(99)], &[SystemFlagBit::Flagged], &[]).unwrap());

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

    /// The batch counterpart of `patches_keywords_on_a_cached_summary`:
    /// keywords applied to many uids at once land on exactly the targeted
    /// uids, leave the others (and their flags) untouched, and a uid outside
    /// the cached window fails the whole batch.
    #[test]
    fn patches_keywords_on_many_cached_summaries_at_once() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let mailbox_id = MailboxId::new(&account_id, "INBOX");
        let work = lookout_core::tag_keyword("work");
        let red = lookout_core::tag_keyword("red");

        cache
            .replace_messages(&mailbox_id, UidValidity(1), &[sample_summary(&mailbox_id, 1, None), sample_summary(&mailbox_id, 2, None)])
            .unwrap();
        cache.update_flags_many(&mailbox_id, &[Uid(1)], &[SystemFlagBit::Seen], &[]).unwrap();

        assert!(cache.update_keywords_many(&mailbox_id, &[Uid(1), Uid(2)], &[work.clone(), red.clone()], &[]).unwrap());
        let loaded = cache.load_messages(&mailbox_id).unwrap();
        assert!(loaded.iter().find(|m| m.uid == Uid(1)).unwrap().keywords.contains(&work));
        assert!(loaded.iter().find(|m| m.uid == Uid(2)).unwrap().keywords.contains(&red));
        assert!(!loaded.iter().find(|m| m.uid == Uid(1)).unwrap().is_unread(), "the keyword patch must not disturb flags");

        assert!(cache.update_keywords_many(&mailbox_id, &[Uid(2)], &[], std::slice::from_ref(&red)).unwrap());
        let loaded = cache.load_messages(&mailbox_id).unwrap();
        assert!(!loaded.iter().find(|m| m.uid == Uid(2)).unwrap().keywords.contains(&red));

        assert!(!cache.update_keywords_many(&mailbox_id, &[Uid(1), Uid(99)], &[work], &[]).unwrap());

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

    /// The `addresses` cap: once the accumulated book passes `ADDRESSES_CAP`,
    /// the lowest-ranked overflow (fewest lifetime appearances, ties broken
    /// by least-recent contact) is pruned, and the top ranks - what the
    /// autocomplete and the dashboard actually show - survive intact.
    #[test]
    fn record_addresses_caps_the_accumulated_book() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let mailbox_id = MailboxId::new(&account_id, "INBOX");

        // Bulk-load past the cap in one call: ADDRESSES_CAP + 100 distinct
        // correspondents.
        let messages: Vec<EmailSummary> = (0..=ADDRESSES_CAP + 100)
            .map(|i| {
                let mut msg = sample_summary(&mailbox_id, i as u32, None);
                msg.from = vec![lookout_core::EmailAddress::new(format!("person{i}@example.com"))];
                msg
            })
            .collect();
        cache.record_addresses(&messages).unwrap();

        // The book is bounded at the cap, not at cap + 100.
        let all = cache.search_addresses("", ADDRESSES_CAP + 1000).unwrap();
        assert_eq!(all.len(), ADDRESSES_CAP, "the overflow is pruned down to the cap");

        // A hot correspondent - recorded many times - ranks at the top and
        // survives the prune.
        let hot = (0..50)
            .map(|i| {
                let mut msg = sample_summary(&mailbox_id, 10_000 + i as u32, None);
                msg.from = vec![lookout_core::EmailAddress::new("hot@example.com")];
                msg
            })
            .collect::<Vec<_>>();
        cache.record_addresses(&hot).unwrap();
        let top = cache.top_addresses(3).unwrap();
        assert_eq!(top[0].0.address, "hot@example.com", "the most-contacted address ranks first");
        assert_eq!(top[0].1, 50);

        let path = cache_dir().join(format!("{}.sqlite3", sanitize_filename(&account_id)));
        let _ = std::fs::remove_file(path);
    }

    /// The dashboard's "most contacted" feed: top addresses ranked by
    /// lifetime appearance count, with the count alongside, ties broken by
    /// recency.
    #[test]
    fn top_addresses_ranks_by_lifetime_appearances_with_counts() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let mailbox_id = MailboxId::new(&account_id, "INBOX");

        let addressed = |uid: u32, from: &str, to: &[&str]| {
            let mut msg = sample_summary(&mailbox_id, uid, None);
            msg.from = vec![lookout_core::EmailAddress::new(from)];
            msg.to = to.iter().map(|a| lookout_core::EmailAddress::new(*a)).collect();
            msg
        };

        cache
            .record_addresses(&[
                addressed(1, "ada@example.com", &["bob@example.com", "carol@example.com"]),
                addressed(2, "bob@example.com", &["ada@example.com"]),
                addressed(3, "dave@example.com", &[]),
            ])
            .unwrap();

        let top = cache.top_addresses(2).unwrap();
        assert_eq!(top.len(), 2);
        assert_eq!(top[0].0.address, "ada@example.com", "appears in from and to");
        assert_eq!(top[0].1, 2);
        assert_eq!(top[1].0.address, "bob@example.com");
        assert_eq!(top[1].1, 2);

        let top = cache.top_addresses(10).unwrap();
        assert_eq!(top.len(), 4, "every recorded address, most-contacted first");
        assert_eq!(top[3].0.address, "dave@example.com");

        let path = cache_dir().join(format!("{}.sqlite3", sanitize_filename(&account_id)));
        let _ = std::fs::remove_file(path);
    }

    /// The dashboard's "emails by time of day" feed: cached envelope dates
    /// are bucketed by the *local* hour of day, and the `since` window only
    /// counts newer messages.
    #[test]
    fn hour_histogram_buckets_by_local_hour_and_honors_the_window() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let mailbox_id = MailboxId::new(&account_id, "INBOX");

        let dated = |uid: u32, utc_hour: u32| {
            let mut msg = sample_summary(&mailbox_id, uid, None);
            msg.date = DateTime::parse_from_rfc3339(&format!("2026-08-06T{utc_hour:02}:30:00Z")).unwrap().with_timezone(&Utc);
            msg
        };

        cache.replace_messages(&mailbox_id, UidValidity(1), &[dated(1, 2), dated(2, 2), dated(3, 9)]).unwrap();

        let histogram = cache.hour_histogram(None).unwrap();
        let expected_local_hour = |utc_hour: u32| {
            DateTime::parse_from_rfc3339(&format!("2026-08-06T{utc_hour:02}:30:00Z"))
                .unwrap()
                .with_timezone(&Local)
                .hour() as usize
        };
        assert_eq!(histogram.iter().sum::<i64>(), 3, "every cached message is counted");
        assert_eq!(histogram[expected_local_hour(2)], 2);
        assert_eq!(histogram[expected_local_hour(9)], 1);

        let windowed = cache.hour_histogram(Some(DateTime::parse_from_rfc3339("2026-08-06T09:30:00Z").unwrap().into())).unwrap();
        assert_eq!(windowed.iter().sum::<i64>(), 1, "the window is inclusive of the cut instant");

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

    /// `store_bodies` caches a whole prefetch batch in one transaction and
    /// upgrades each row's search index to its full text, exactly as
    /// `store_body` does per message.
    #[test]
    fn store_bodies_upgrades_the_whole_batchs_index() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let mailbox_id = MailboxId::new(&account_id, "INBOX");

        let msgs: Vec<EmailSummary> = [1u32, 2]
            .into_iter()
            .map(|uid| searchable_summary(&mailbox_id, uid, &format!("Subject {uid}"), "ada@example.com", Some("preview")))
            .collect();
        cache.replace_messages(&mailbox_id, UidValidity(1), &msgs).unwrap();

        // Only the previews are indexed before the bodies arrive.
        assert_eq!(cache.search("preview", 10).unwrap().len(), 2);
        assert_eq!(cache.search("confidential", 10).unwrap().len(), 0);

        let bodies = vec![(Uid(1), sample_body("confidential earnings")), (Uid(2), sample_body("confidential roadmap"))];
        cache.store_bodies(&mailbox_id, UidValidity(1), &bodies).unwrap();

        assert_eq!(cache.search("confidential", 10).unwrap().len(), 2);
        // The preview terms still match after the bodies replace them.
        assert_eq!(cache.search("preview", 10).unwrap().len(), 2);
        // Each body round-trips out of the cache.
        assert!(cache.load_body(&mailbox_id, Uid(1), UidValidity(1)).unwrap().is_some());
        assert!(cache.load_body(&mailbox_id, Uid(2), UidValidity(1)).unwrap().is_some());
        // An empty batch is a cheap no-op.
        cache.store_bodies(&mailbox_id, UidValidity(1), &[]).unwrap();

        let path = cache_dir().join(format!("{}.sqlite3", sanitize_filename(&account_id)));
        let _ = std::fs::remove_file(path);
    }

    /// The prefetch pass's contract: `has_bodies` answers for a whole envelope
    /// batch in one query - reporting exactly the wanted uids that have cached
    /// bodies, ignoring others' bodies, and honoring `uidvalidity`.
    #[test]
    fn reports_which_wanted_uids_have_cached_bodies() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let mailbox_id = MailboxId::new(&account_id, "INBOX");

        cache.store_body(&mailbox_id, Uid(1), UidValidity(1), &sample_body("one")).unwrap();
        cache.store_body(&mailbox_id, Uid(2), UidValidity(1), &sample_body("two")).unwrap();
        cache.store_body(&mailbox_id, Uid(3), UidValidity(2), &sample_body("other validity")).unwrap();

        // Only the wanted subset comes back; a different uidvalidity never
        // leaks in.
        let have = cache.has_bodies(&mailbox_id, &[Uid(1), Uid(3), Uid(9)], UidValidity(1)).unwrap();
        assert_eq!(have, HashSet::from([Uid(1)]));

        // Empty want-list is a cheap empty answer, not a full-table scan.
        assert!(cache.has_bodies(&mailbox_id, &[], UidValidity(1)).unwrap().is_empty());

        let path = cache_dir().join(format!("{}.sqlite3", sanitize_filename(&account_id)));
        let _ = std::fs::remove_file(path);
    }

    /// The session's coalesced on-demand body fetch relies on this: every
    /// wanted uid's summary comes back keyed for lookup, a uid with nothing
    /// cached is simply absent rather than an error, and an unrelated
    /// mailbox's rows never leak in.
    #[test]
    fn load_summaries_returns_exactly_the_requested_subset() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let mailbox_id = MailboxId::new(&account_id, "INBOX");
        let other_mailbox = MailboxId::new(&account_id, "Archive");

        cache
            .replace_messages(
                &mailbox_id,
                UidValidity(1),
                &[
                    sample_summary(&mailbox_id, 1, Some("preview one")),
                    sample_summary(&mailbox_id, 2, None),
                    sample_summary(&mailbox_id, 3, None),
                ],
            )
            .unwrap();
        cache.replace_messages(&other_mailbox, UidValidity(1), &[sample_summary(&other_mailbox, 1, None)]).unwrap();

        let summaries = cache.load_summaries(&mailbox_id, &[Uid(1), Uid(2), Uid(99)]).unwrap();
        assert_eq!(summaries.len(), 2, "uid 3 wasn't asked for, uid 99 isn't cached");
        assert_eq!(summaries.get(&Uid(1)).and_then(|s| s.preview.as_deref()), Some("preview one"));
        assert!(summaries.contains_key(&Uid(2)));
        assert!(!summaries.contains_key(&Uid(99)), "an uncached uid is simply absent");

        // The other mailbox's uid 1 must never answer for this mailbox's uid 1.
        assert_eq!(summaries.get(&Uid(1)).map(|s| s.mailbox.clone()), Some(mailbox_id.clone()));

        assert!(cache.load_summaries(&mailbox_id, &[]).unwrap().is_empty(), "empty want-list is a cheap empty answer");

        let path = cache_dir().join(format!("{}.sqlite3", sanitize_filename(&account_id)));
        let _ = std::fs::remove_file(path);
    }

    /// The batch counterpart of `reports_which_wanted_uids_have_cached_bodies`
    /// for `load_bodies`: the actual bodies come back, not just presence, and
    /// the same uidvalidity/want-list guards apply.
    #[test]
    fn load_bodies_returns_exactly_the_requested_subset() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let mailbox_id = MailboxId::new(&account_id, "INBOX");

        cache.store_body(&mailbox_id, Uid(1), UidValidity(1), &sample_body("one")).unwrap();
        cache.store_body(&mailbox_id, Uid(2), UidValidity(1), &sample_body("two")).unwrap();
        cache.store_body(&mailbox_id, Uid(3), UidValidity(2), &sample_body("other validity")).unwrap();

        let bodies = cache.load_bodies(&mailbox_id, &[Uid(1), Uid(3), Uid(9)], UidValidity(1)).unwrap();
        assert_eq!(bodies.keys().copied().collect::<HashSet<_>>(), HashSet::from([Uid(1)]));
        assert_eq!(bodies[&Uid(1)].text_body.as_deref(), Some("one"));

        assert!(cache.load_bodies(&mailbox_id, &[], UidValidity(1)).unwrap().is_empty());

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

    /// The expunge-purge contract: a message absent from the new set is
    /// dropped from every cache layer - the envelope row, the search index,
    /// its `bodies` row, its snooze entry, and its flat files (the attachment
    /// `.bin` sidecars and the raw `.eml` export). The stored `uidvalidity`
    /// rides along so a recycled uid under a newer validity is never purged.
    #[test]
    fn replace_messages_purges_bodies_snoozes_and_flat_files_for_absent_uids() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let mailbox_id = MailboxId::new(&account_id, "INBOX");

        cache
            .replace_messages(&mailbox_id, UidValidity(1), &[sample_summary(&mailbox_id, 1, None), sample_summary(&mailbox_id, 2, None)])
            .unwrap();
        cache.store_body(&mailbox_id, Uid(1), UidValidity(1), &sample_body("gone body")).unwrap();
        cache.store_body(&mailbox_id, Uid(2), UidValidity(1), &sample_body("kept body")).unwrap();
        cache.store_attachment(&mailbox_id, Uid(1), UidValidity(1), "2", b"gone pdf").unwrap();
        cache.store_attachment(&mailbox_id, Uid(1), UidValidity(1), "3", b"gone png").unwrap();
        cache.store_raw_message(&mailbox_id, Uid(1), UidValidity(1), b"Subject: gone\r\n\r\nx").unwrap();
        cache.snooze_message(&mailbox_id, Uid(1), Utc::now() + chrono::Duration::hours(1)).unwrap();

        // The resync drops uid 1 (expunged server-side) and keeps uid 2.
        cache.replace_messages(&mailbox_id, UidValidity(1), &[sample_summary(&mailbox_id, 2, None)]).unwrap();

        let remaining = cache.load_messages(&mailbox_id).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].uid, Uid(2));
        assert!(!cache.has_body(&mailbox_id, Uid(1), UidValidity(1)).unwrap(), "the body row is purged");
        assert!(cache.has_body(&mailbox_id, Uid(2), UidValidity(1)).unwrap(), "the kept message's body survives");
        assert!(cache.active_snoozed_uids(&mailbox_id, Utc::now()).unwrap().is_empty(), "the snooze entry is purged");
        assert!(
            cache.load_attachment(&mailbox_id, Uid(1), UidValidity(1), "2").unwrap().is_none(),
            "the attachment .bin is purged"
        );
        assert!(
            cache.load_attachment(&mailbox_id, Uid(1), UidValidity(1), "3").unwrap().is_none(),
            "every attachment part is purged"
        );
        assert!(cache.load_raw_message(&mailbox_id, Uid(1), UidValidity(1)).unwrap().is_none(), "the .eml is purged");
        assert!(
            cache.load_attachment(&mailbox_id, Uid(2), UidValidity(1), "9").unwrap().is_none(),
            "an untargeted file still loads (miss)"
        );

        let path = cache_dir().join(format!("{}.sqlite3", sanitize_filename(&account_id)));
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_dir_all(cache_dir().join("attachments").join(sanitize_filename(&account_id)));
        let _ = std::fs::remove_dir_all(cache_dir().join("messages").join(sanitize_filename(&account_id)));
    }

    /// A whole-mailbox clear is the empty-set extreme of the expunge purge:
    /// every stored row's bodies, snoozes, and flat files go with it, in one
    /// `replace_messages` call.
    #[test]
    fn replace_messages_purges_everything_on_a_whole_mailbox_clear() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let mailbox_id = MailboxId::new(&account_id, "INBOX");

        cache
            .replace_messages(&mailbox_id, UidValidity(1), &[sample_summary(&mailbox_id, 1, None), sample_summary(&mailbox_id, 2, None)])
            .unwrap();
        cache.store_body(&mailbox_id, Uid(1), UidValidity(1), &sample_body("one")).unwrap();
        cache.store_raw_message(&mailbox_id, Uid(2), UidValidity(1), b"Subject: two\r\n\r\nx").unwrap();
        cache.snooze_message(&mailbox_id, Uid(2), Utc::now() + chrono::Duration::hours(1)).unwrap();

        cache.replace_messages(&mailbox_id, UidValidity(1), &[]).unwrap();

        assert!(cache.load_messages(&mailbox_id).unwrap().is_empty());
        assert!(!cache.has_body(&mailbox_id, Uid(1), UidValidity(1)).unwrap());
        assert!(cache.active_snoozed_uids(&mailbox_id, Utc::now()).unwrap().is_empty());
        assert!(cache.load_raw_message(&mailbox_id, Uid(2), UidValidity(1)).unwrap().is_none());

        let path = cache_dir().join(format!("{}.sqlite3", sanitize_filename(&account_id)));
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_dir_all(cache_dir().join("messages").join(sanitize_filename(&account_id)));
    }

    /// The diff-based `replace_messages` contract: a re-sync that rewrites an
    /// envelope must not downgrade the message's search body text back from
    /// the full cached body to the preview - `store_body`'s upgrade survives
    /// the sync (and its `WHERE data IS NOT excluded.data` guard means an
    /// unchanged envelope isn't re-indexed at all).
    #[test]
    fn a_resync_keeps_the_full_body_index_text() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let mailbox_id = MailboxId::new(&account_id, "INBOX");

        let msg = searchable_summary(&mailbox_id, 1, "Quarterly report", "ada@example.com", Some("The numbers"));
        cache.replace_messages(&mailbox_id, UidValidity(1), &[msg.clone()]).unwrap();
        cache
            .store_body(&mailbox_id, Uid(1), UidValidity(1), &sample_body("This document is confidential."))
            .unwrap();
        assert_eq!(cache.search("confidential", 10).unwrap().len(), 1);

        // A resync with only a flag changed (what a mark-as-read elsewhere
        // produces) rewrites the envelope but must leave the body text alone.
        let mut flag_changed = msg.clone();
        flag_changed.flags.insert(SystemFlagBit::Seen);
        cache.replace_messages(&mailbox_id, UidValidity(1), &[flag_changed]).unwrap();
        assert_eq!(cache.search("confidential", 10).unwrap().len(), 1, "full body text survives a resync");
        assert_eq!(cache.search("numbers", 10).unwrap().len(), 1, "preview phrasing stays findable");
        assert!(!cache.load_messages(&mailbox_id).unwrap()[0].is_unread(), "the flag change itself stuck");

        // An *identical* envelope is not re-indexed at all - the steady-state
        // wake case, where the data guard reports zero changed rows.
        cache.replace_messages(&mailbox_id, UidValidity(1), &[msg]).unwrap();
        assert_eq!(cache.search("confidential", 10).unwrap().len(), 1);

        let path = cache_dir().join(format!("{}.sqlite3", sanitize_filename(&account_id)));
        let _ = std::fs::remove_file(path);
    }

    /// A preview that arrives on a later sync (the first sync indexes the new
    /// envelope with no snippet; `fetch_previews`' second emit carries one)
    /// becomes searchable - the changed envelope is re-indexed with it.
    #[test]
    fn a_resync_with_a_fresh_preview_indexes_the_preview() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let mailbox_id = MailboxId::new(&account_id, "INBOX");

        let msg = searchable_summary(&mailbox_id, 1, "Quarterly report", "ada@example.com", None);
        cache.replace_messages(&mailbox_id, UidValidity(1), &[msg.clone()]).unwrap();
        assert!(cache.search("snippetword", 10).unwrap().is_empty());

        let mut with_preview = msg.clone();
        with_preview.preview = Some("snippetword first".to_string());
        cache.replace_messages(&mailbox_id, UidValidity(1), &[with_preview]).unwrap();
        assert_eq!(cache.search("snippetword", 10).unwrap().len(), 1);

        let path = cache_dir().join(format!("{}.sqlite3", sanitize_filename(&account_id)));
        let _ = std::fs::remove_file(path);
    }

    /// The diff's scope: re-replacing one mailbox's window leaves every other
    /// mailbox's rows and index entries alone, and a UID absent from the new
    /// set (expunged server-side) is dropped from both the table and the index.
    #[test]
    fn replace_messages_diffs_only_the_target_mailbox() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let mailbox_id = MailboxId::new(&account_id, "INBOX");
        let other = MailboxId::new(&account_id, "Archive");

        cache
            .replace_messages(
                &mailbox_id,
                UidValidity(1),
                &[
                    searchable_summary(&mailbox_id, 1, "Vanishing subject", "ada@example.com", None),
                    searchable_summary(&mailbox_id, 2, "Keeping subject", "bob@elsewhere.org", None),
                    searchable_summary(&mailbox_id, 3, "Staying subject", "carol@example.org", None),
                ],
            )
            .unwrap();
        cache
            .replace_messages(&other, UidValidity(1), &[searchable_summary(&other, 9, "Archival notes", "dave@example.com", None)])
            .unwrap();

        // The resync drops uid 1 (expunged) and keeps 2 + 3.
        cache
            .replace_messages(
                &mailbox_id,
                UidValidity(1),
                &[
                    searchable_summary(&mailbox_id, 2, "Keeping subject", "bob@elsewhere.org", None),
                    searchable_summary(&mailbox_id, 3, "Staying subject", "carol@example.org", None),
                ],
            )
            .unwrap();

        let in_inbox = cache.load_messages(&mailbox_id).unwrap();
        assert_eq!(in_inbox.len(), 2);
        assert!(!in_inbox.iter().any(|m| m.uid == Uid(1)));
        // The other mailbox is untouched by the diff.
        let in_archive = cache.load_messages(&other).unwrap();
        assert_eq!(in_archive.len(), 1);
        assert_eq!(in_archive[0].uid, Uid(9));

        // Index rows follow the envelope rows: uid 1's hit is gone, the rest
        // (including the untouched other mailbox) still match.
        assert!(cache.search("vanishing", 10).unwrap().is_empty());
        assert_eq!(cache.search("keeping", 10).unwrap().len(), 1);
        assert_eq!(cache.search("staying", 10).unwrap().len(), 1);
        assert_eq!(cache.search("archival", 10).unwrap().len(), 1);

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
        // exactly what a pre-search build left on disk (user_version 4: the
        // envelope/body-format migrations done, the FTS index absent).
        let path = cache_dir().join(format!("{}.sqlite3", sanitize_filename(&account_id)));
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "
                PRAGMA user_version = 4;
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

    /// `recent_messages` returns the newest messages first, honors the limit,
    /// and keeps snoozed messages out of the feed.
    #[test]
    fn recent_messages_returns_newest_first_and_hides_snoozed() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let mailbox_id = MailboxId::new(&account_id, "INBOX");

        let mut old = sample_summary(&mailbox_id, 1, Some("old"));
        old.date = Utc::now() - chrono::Duration::days(2);
        let mut mid = sample_summary(&mailbox_id, 2, Some("mid"));
        mid.date = Utc::now() - chrono::Duration::days(1);
        let mut new = sample_summary(&mailbox_id, 3, Some("new"));
        new.date = Utc::now();
        cache.replace_messages(&mailbox_id, UidValidity(1), &[old, mid, new]).unwrap();

        let all = cache.recent_messages(10).unwrap();
        assert_eq!(all.iter().map(|m| m.uid.0).collect::<Vec<_>>(), vec![3, 2, 1], "newest first");

        let limited = cache.recent_messages(2).unwrap();
        assert_eq!(limited.iter().map(|m| m.uid.0).collect::<Vec<_>>(), vec![3, 2], "the limit is honored");

        // A snoozed message is hidden from the feed even when it's the newest.
        cache.snooze_message(&mailbox_id, Uid(3), Utc::now() + chrono::Duration::hours(1)).unwrap();
        let filtered = cache.recent_messages(10).unwrap();
        assert_eq!(filtered.iter().map(|m| m.uid.0).collect::<Vec<_>>(), vec![2, 1]);

        let path = cache_dir().join(format!("{}.sqlite3", sanitize_filename(&account_id)));
        let _ = std::fs::remove_file(path);
    }
}
