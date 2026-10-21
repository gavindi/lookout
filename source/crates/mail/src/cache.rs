/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::Mutex;

use chrono::{DateTime, Local, Timelike, Utc};
use lookout_core::{
    AccountId, ContactsProvider, EmailAddress, EmailBody, EmailSummary, FlagStatus, FollowUpFlag, Importance, InferenceClassification, Mailbox, MailboxId, SystemFlagBit, Uid,
    UidValidity,
};
use rusqlite::{Connection, OptionalExtension};

use crate::error::Result;
use crate::search_query::SearchQuery;

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
    /// `@mentions` detection context (M-25): the user's own addresses and
    /// names, compiled once at session start. This is configuration, not
    /// cached data - the session installs it wherever it parses a body
    /// snippet, and hanging it off the per-account handle avoids threading a
    /// matcher through every `sync_mailbox` call site. The app's read-side
    /// handle leaves it empty, matching nothing.
    mention_matcher: std::sync::RwLock<lookout_core::MentionMatcher>,
}

/// State of a row in the local outbox (see [`OutboxEntry`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboxState {
    /// Waiting to send: its `not_before` time hasn't arrived, or a previous
    /// attempt failed and it's waiting out the retry backoff.
    Pending,
    /// Every retry attempt failed. The row is left for the user to act on
    /// (item 1.14) rather than retried forever.
    Failed,
}

impl OutboxState {
    fn as_str(self) -> &'static str {
        match self {
            OutboxState::Pending => "pending",
            OutboxState::Failed => "failed",
        }
    }

    /// Unknown values (a future schema's state this build doesn't know) read
    /// as `Pending`, so a row is retried rather than silently lost.
    fn from_db(value: &str) -> Self {
        match value {
            "failed" => OutboxState::Failed,
            _ => OutboxState::Pending,
        }
    }
}

/// One queued outgoing message in the account's local outbox. `payload` is
/// opaque to the cache: the session serializes a
/// [`crate::send::OutboxMessage`] into it (item 0.3).
#[derive(Debug, Clone)]
pub struct OutboxEntry {
    pub id: i64,
    pub account_id: AccountId,
    pub not_before: DateTime<Utc>,
    pub attempts: u32,
    pub payload: Vec<u8>,
    pub state: OutboxState,
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

/// Re-indexes `msg` after its envelope row was inserted or changed (flags,
/// preview, ...). An envelope never carries the body text `store_body`
/// indexed, so the existing index row's text is kept - a flag-only rewrite
/// must not downgrade a full-body index back to the preview. A message with
/// no index row yet indexes its preview.
///
/// `is_new` says the message had no `messages` row before this write (and so
/// no index row). Both the body lookup and the delete half of
/// `index_upsert_message` filter on the FTS table's `mailbox_id`/`uid`
/// columns, which are `UNINDEXED`: each is a full scan of the whole index. A
/// large folder's first sync inserts every message, so doing the lookup and
/// delete per row is O(n²) - 12k messages took over a minute before this
/// short-circuit. A new message has nothing to look up or delete, so it
/// takes the plain INSERT-only path instead.
fn index_preserving_body(conn: &rusqlite::Connection, msg: &EmailSummary, is_new: bool) -> Result<()> {
    let existing_body: Option<String> = if is_new {
        None
    } else {
        conn.query_row(
            "SELECT body FROM search_fts WHERE mailbox_id = ?1 AND uid = ?2",
            rusqlite::params![msg.mailbox.0, msg.uid.0],
            |row| row.get(0),
        )
        .ok()
    };
    let body = match existing_body.as_deref() {
        Some(existing) if !existing.is_empty() => existing.to_string(),
        _ => msg.preview.as_deref().unwrap_or("").to_string(),
    };
    if is_new {
        index_message(conn, msg, &body)
    } else {
        index_upsert_message(conn, msg, &body)
    }
}

/// Re-indexes `(mailbox_id, uid)` with the full text of its just-stored (or,
/// for `store_partial_messages`, already-cached) `body`, but only if the
/// envelope is cached - a stray body for a wiped envelope has no
/// subject/sender to index against. The preview text rides along so
/// previously-indexed phrasing stays findable.
///
/// A prefetch-indexed `partial` row with no preview yet also gets one derived
/// from the body: such a folder never runs `sync_mailbox`'s preview fetch, so
/// this is the only way a search hit from it shows a snippet.
fn index_stored_body(conn: &rusqlite::Connection, mailbox_id: &MailboxId, uid: Uid, body: &EmailBody) -> Result<()> {
    let row: Option<(String, bool)> = conn
        .query_row(
            "SELECT data, partial FROM messages WHERE mailbox_id = ?1 AND uid = ?2",
            rusqlite::params![mailbox_id.0, uid.0],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some(mut summary) = row.as_ref().and_then(|(data, _)| serde_json::from_str::<EmailSummary>(data).ok()) else {
        return Ok(());
    };
    let partial = row.is_some_and(|(_, partial)| partial);
    if partial && summary.preview.is_none() {
        if let Some(preview) = crate::body::preview_from_body(body) {
            summary.preview = Some(preview);
            conn.execute(
                "UPDATE messages SET data = ?1 WHERE mailbox_id = ?2 AND uid = ?3",
                rusqlite::params![serde_json::to_string(&summary)?, mailbox_id.0, uid.0],
            )?;
        }
    }
    let mut indexed = body_index_text(body).unwrap_or_default();
    if let Some(preview) = &summary.preview {
        if !indexed.is_empty() {
            indexed.push(' ');
        }
        indexed.push_str(preview);
    }
    index_upsert_message(conn, &summary, &indexed)
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
    let mut stmt = conn.prepare_cached("SELECT data FROM messages WHERE mailbox_id = ?1 AND uid = ?2")?;
    let mut rows = stmt.query_map(rusqlite::params![mailbox_id.0, uid.0], |row| row.get::<_, String>(0))?;
    match rows.next() {
        Some(Ok(data)) => Ok(serde_json::from_str(&data).ok()),
        Some(Err(e)) => Err(e.into()),
        None => Ok(None),
    }
}

/// The `follow_up_flags` key for a message: its bare Message-ID, or - for the
/// rare message without one - its folder and UID.
pub fn follow_up_key(summary: &EmailSummary) -> String {
    match summary.message_id.as_deref().map(bare_message_id).filter(|id| !id.is_empty()) {
        Some(id) => id,
        None => format!("uid:{}:{}", summary.mailbox.0, summary.uid.0),
    }
}

fn bare_message_id(id: &str) -> String {
    id.trim().trim_start_matches('<').trim_end_matches('>').to_string()
}

fn flag_status_code(status: FlagStatus) -> i64 {
    match status {
        FlagStatus::NotFlagged => 0,
        FlagStatus::Flagged => 1,
        FlagStatus::Complete => 2,
    }
}

/// An overlay table's `SELECT` paired with the same query narrowed to the keys
/// in a JSON array bound as `?1` (see [`load_overlay_table`]). Both halves are
/// literals, so each is one `prepare_cached` entry.
macro_rules! overlay_sql {
    ($select:literal) => {
        ($select, concat!($select, " WHERE message_key IN (SELECT value FROM json_each(?1))"))
    };
}

/// Reads one overlay table into a map keyed by `message_key`: every row when
/// `keys_json` is `None`, otherwise only the rows whose key is in that JSON
/// array (a primary-key probe per key, see `OVERLAY_KEYED_MAX`).
fn load_overlay_table<T>(
    conn: &Connection,
    (all_sql, keyed_sql): (&str, &str),
    keys_json: Option<&str>,
    map: impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<(String, T)>,
) -> Result<HashMap<String, T>> {
    let mut stmt = conn.prepare_cached(if keys_json.is_some() { keyed_sql } else { all_sql })?;
    let rows = stmt.query_map(rusqlite::params_from_iter(keys_json), map)?;
    let mut out = HashMap::new();
    for row in rows {
        let (key, value) = row?;
        out.insert(key, value);
    }
    Ok(out)
}

fn load_local_follow_ups(conn: &Connection, keys_json: Option<&str>) -> Result<HashMap<String, FollowUpFlag>> {
    load_overlay_table(conn, overlay_sql!("SELECT message_key, status, due, completed FROM follow_up_flags"), keys_json, |row| {
        let status = match row.get::<_, i64>(1)? {
            1 => FlagStatus::Flagged,
            2 => FlagStatus::Complete,
            _ => FlagStatus::NotFlagged,
        };
        let flag = FollowUpFlag {
            status,
            start: None,
            due: row.get::<_, Option<i64>>(2)?.and_then(|t| DateTime::from_timestamp(t, 0)),
            completed: row.get::<_, Option<i64>>(3)?.and_then(|t| DateTime::from_timestamp(t, 0)),
        };
        Ok((row.get(0)?, flag))
    })
}

/// Rewrites `summary`'s follow-up keywords to the local row's state, if it
/// has one, so the UI reads one representation however the flag is stored.
/// `key` is the summary's [`follow_up_key`].
fn apply_local_follow_up(local: &HashMap<String, FollowUpFlag>, key: &str, summary: &mut EmailSummary) {
    let Some(flag) = local.get(key) else {
        return;
    };
    summary.keywords.retain(|k| !lookout_core::is_flag_keyword(k));
    summary.keywords.extend(lookout_core::follow_up_keywords(flag));
}

/// One message's Outlook properties as pushed from a Graph delta or read on
/// demand (OUTLOOK_PARITY.md §4.4, item 5.4). `message_id` is the RFC 822
/// `Message-ID` (angle brackets allowed); `categories` are Lookout category
/// *keys* (the app maps Graph's category names onto keys before sending), so a
/// key maps straight back to the `$Lookout-tag-<key>` keyword.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteMailProperties {
    pub message_id: String,
    pub graph_id: String,
    pub categories: Vec<String>,
    pub importance: Importance,
    pub classification: InferenceClassification,
    /// The follow-up flag, or `None` when Graph reports it `notFlagged`.
    pub flag: Option<FollowUpFlag>,
}

/// One Focused/Other sender override (M-03), as stored locally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InferenceOverrideRow {
    pub sender: String,
    pub classify_as: InferenceClassification,
    /// Graph's override id, once the create has answered; `None` for a local
    /// override not yet pushed.
    pub override_id: Option<String>,
}

/// The Graph `importance` code stored in `local_importance` (`Normal` is never
/// stored - it is the absence of a row).
fn importance_code(importance: Importance) -> i64 {
    match importance {
        Importance::Low => 1,
        Importance::Normal => 0,
        Importance::High => 2,
    }
}

fn importance_from_code(code: i64) -> Importance {
    match code {
        1 => Importance::Low,
        2 => Importance::High,
        _ => Importance::Normal,
    }
}

/// The Focused/Other code stored in `local_inference` (`Focused` is never
/// stored - it is the absence of a row).
fn classification_code(classification: InferenceClassification) -> i64 {
    match classification {
        InferenceClassification::Focused => 0,
        InferenceClassification::Other => 1,
    }
}

fn classification_from_code(code: i64) -> InferenceClassification {
    if code == 1 {
        InferenceClassification::Other
    } else {
        InferenceClassification::Focused
    }
}

fn load_local_categories(conn: &Connection, keys_json: Option<&str>) -> Result<HashMap<String, Vec<String>>> {
    load_overlay_table(conn, overlay_sql!("SELECT message_key, category_keys FROM local_categories"), keys_json, |row| {
        Ok((row.get(0)?, serde_json::from_str(&row.get::<_, String>(1)?).unwrap_or_default()))
    })
}

fn load_local_importance(conn: &Connection, keys_json: Option<&str>) -> Result<HashMap<String, Importance>> {
    load_overlay_table(conn, overlay_sql!("SELECT message_key, importance FROM local_importance"), keys_json, |row| {
        Ok((row.get(0)?, importance_from_code(row.get(1)?)))
    })
}

fn load_local_inference(conn: &Connection, keys_json: Option<&str>) -> Result<HashMap<String, InferenceClassification>> {
    load_overlay_table(conn, overlay_sql!("SELECT message_key, classification FROM local_inference"), keys_json, |row| {
        Ok((row.get(0)?, classification_from_code(row.get(1)?)))
    })
}

/// Rewrites `summary`'s category keywords to the local row's keys, if it has
/// one - the `$Lookout-tag-*` keyword store for a folder that can't keep custom
/// keywords (Exchange IMAP, item 5.4).
fn apply_local_categories(local: &HashMap<String, Vec<String>>, key: &str, summary: &mut EmailSummary) {
    let Some(keys) = local.get(key) else {
        return;
    };
    summary.keywords.retain(|k| !k.starts_with(lookout_core::TAG_KEYWORD_PREFIX));
    summary.keywords.extend(keys.iter().map(|key| lookout_core::tag_keyword(key)));
}

fn apply_local_importance(local: &HashMap<String, Importance>, key: &str, summary: &mut EmailSummary) {
    if let Some(importance) = local.get(key) {
        summary.importance = *importance;
    }
}

fn apply_local_classification(local: &HashMap<String, InferenceClassification>, key: &str, summary: &mut EmailSummary) {
    if let Some(classification) = local.get(key) {
        summary.inference_classification = *classification;
    }
}

/// Reads of at most this many summaries look their overlay rows up by key;
/// larger reads scan each overlay table once. On a Microsoft 365 account the
/// Graph delta fills the overlay tables for most of the mailbox, so a scan to
/// overlay a handful of messages (the body path's `load_summaries`, a search)
/// deserializes far more rows than it uses - while for a whole folder, one
/// scan per table is cheaper than thousands of primary-key probes.
const OVERLAY_KEYED_MAX: usize = 512;

/// Every locally-stored Outlook overlay (item 5.4), loaded once per read.
#[derive(Default)]
struct LocalOverlays {
    follow_ups: HashMap<String, FollowUpFlag>,
    categories: HashMap<String, Vec<String>>,
    importance: HashMap<String, Importance>,
    classification: HashMap<String, InferenceClassification>,
}

impl LocalOverlays {
    /// Loads every overlay row (`keys_json` is `None`), or only the rows for
    /// the [`follow_up_key`]s in that JSON array.
    fn load(conn: &Connection, keys_json: Option<&str>) -> Result<Self> {
        Ok(LocalOverlays {
            follow_ups: load_local_follow_ups(conn, keys_json)?,
            categories: load_local_categories(conn, keys_json)?,
            importance: load_local_importance(conn, keys_json)?,
            classification: load_local_inference(conn, keys_json)?,
        })
    }

    /// Applies the overlays to `summary`, whose [`follow_up_key`] is `key`.
    fn apply(&self, key: &str, summary: &mut EmailSummary) {
        apply_local_follow_up(&self.follow_ups, key, summary);
        apply_local_categories(&self.categories, key, summary);
        apply_local_importance(&self.importance, key, summary);
        apply_local_classification(&self.classification, key, summary);
    }
}

/// Overlays every locally-stored Outlook property (follow-up flags, categories,
/// importance, Focused/Other classification) onto `messages`, so the UI reads
/// one representation however each value is stored (item 1.16, item 5.4).
fn overlay_local<'a>(conn: &Connection, messages: impl IntoIterator<Item = &'a mut EmailSummary>) -> Result<()> {
    let messages: Vec<&mut EmailSummary> = messages.into_iter().collect();
    if messages.is_empty() {
        return Ok(());
    }
    let keys: Vec<String> = messages.iter().map(|summary| follow_up_key(summary)).collect();
    let keys_json = if keys.len() <= OVERLAY_KEYED_MAX { Some(serde_json::to_string(&keys)?) } else { None };
    let overlays = LocalOverlays::load(conn, keys_json.as_deref())?;
    for (summary, key) in messages.into_iter().zip(&keys) {
        overlays.apply(key, summary);
    }
    Ok(())
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
        // Room for every `prepare_cached` statement at once (rusqlite's default
        // is 16): the hot read paths and the eight overlay lookups would
        // otherwise evict the sync and delete statements, and each other.
        conn.set_prepared_statement_cache_capacity(64);
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
            -- `partial` marks a row the background prefetch indexed from a
            -- never-opened folder's newest-N window (`store_partial_messages`)
            -- rather than a full `sync_mailbox` snapshot: searchable, but
            -- never served as the folder's contents (see `has_messages`).
            CREATE TABLE IF NOT EXISTS messages (
                mailbox_id TEXT NOT NULL,
                uid INTEGER NOT NULL,
                uidvalidity INTEGER NOT NULL,
                data TEXT NOT NULL,
                partial INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (mailbox_id, uid)
            );
            CREATE INDEX IF NOT EXISTS messages_by_mailbox ON messages (mailbox_id);
            CREATE TABLE IF NOT EXISTS snoozed (
                mailbox_id TEXT NOT NULL,
                uid INTEGER NOT NULL,
                snoozed_until INTEGER NOT NULL,
                PRIMARY KEY (mailbox_id, uid)
            );
            CREATE TABLE IF NOT EXISTS follow_up_flags (
                message_key TEXT PRIMARY KEY,
                status INTEGER NOT NULL,
                due INTEGER,
                completed INTEGER,
                updated_at INTEGER NOT NULL
            );
            -- Microsoft 365 per-message Outlook properties (OUTLOOK_PARITY.md
            -- §4.4, item 5.4): the categories, importance and Focused/Other
            -- classification a mailbox either can't store as IMAP keywords
            -- (Exchange has no custom keywords) or doesn't expose at all, plus
            -- the Graph-id map and delta cursor the sync needs. Every overlay
            -- table holds only NON-default values, keyed by `follow_up_key`
            -- (the bare Message-ID where there is one) exactly like
            -- `follow_up_flags`, and is overlaid onto every cache read; the
            -- `updated_at` guard keeps a local change made while a Graph
            -- snapshot was in flight. Append-only, so no `user_version` wipe.
            CREATE TABLE IF NOT EXISTS message_graph_ids (
                message_key TEXT PRIMARY KEY,
                graph_id TEXT NOT NULL,
                updated_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS message_graph_ids_by_graph ON message_graph_ids (graph_id);
            -- The Graph mail transport's identity map (MAIL_GRAPH_OVERVIEW.md
            -- item 5.7.3): one synthetic `Uid` per Graph immutable message id,
            -- unique across the account, so a moved message keeps its Uid.
            -- `graph_uid_space` holds the account's single `UidValidity`
            -- (fixed when the map is created) and the next Uid to hand out.
            -- Both are empty on IMAP accounts. Kept outside `messages`, so an
            -- envelope wipe keeps every Uid. Append-only, so no `user_version`
            -- wipe.
            CREATE TABLE IF NOT EXISTS graph_uids (
                graph_id TEXT PRIMARY KEY,
                uid INTEGER NOT NULL UNIQUE
            );
            CREATE TABLE IF NOT EXISTS graph_uid_space (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                uidvalidity INTEGER NOT NULL,
                next_uid INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS local_categories (
                message_key TEXT PRIMARY KEY,
                category_keys TEXT NOT NULL,
                updated_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS local_importance (
                message_key TEXT PRIMARY KEY,
                importance INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS local_inference (
                message_key TEXT PRIMARY KEY,
                classification INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS inference_overrides (
                sender TEXT PRIMARY KEY,
                classify_as INTEGER NOT NULL,
                override_id TEXT,
                updated_at INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS graph_delta_cursor (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                cursor TEXT NOT NULL,
                updated_at INTEGER NOT NULL
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
            -- The local outgoing-mail queue (OUTLOOK_PARITY.md §2.4 decision 2,
            -- item 0.3): the persistent `not_before`-stamped queue behind undo
            -- send, schedule send and the offline outbox. `payload` is opaque to
            -- the cache - the session serializes a `send::OutboxMessage` into it
            -- - and `state` is `pending` or `failed`; `attempts` drives the
            -- retry backoff. AUTOINCREMENT so an id quoted in an
            -- `OutboxQueued`/`OutboxSent` event is never reused after the row is
            -- deleted. Append-only, so it needs no `user_version` wipe.
            CREATE TABLE IF NOT EXISTS outbox (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                account_id TEXT NOT NULL,
                not_before INTEGER NOT NULL,
                attempts INTEGER NOT NULL DEFAULT 0,
                payload BLOB NOT NULL,
                state TEXT NOT NULL DEFAULT 'pending'
            );
            CREATE INDEX IF NOT EXISTS outbox_due ON outbox (state, not_before);
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
        // Caches created before the `partial` column existed. Every existing
        // row came from a full sync, so the default (0, complete) is right.
        // The UI's read handle and the session open this file concurrently,
        // so a lost race on the ALTER ("duplicate column") is re-checked
        // rather than treated as a failure.
        let has_partial_column = |conn: &Connection| -> rusqlite::Result<bool> { conn.prepare("SELECT 1 FROM pragma_table_info('messages') WHERE name = 'partial'")?.exists([]) };
        if !has_partial_column(&conn)? {
            if let Err(e) = conn.execute("ALTER TABLE messages ADD COLUMN partial INTEGER NOT NULL DEFAULT 0", []) {
                if !has_partial_column(&conn)? {
                    return Err(e.into());
                }
            }
        }
        // Messages/envelope cache version. The shared `user_version` PRAGMA is
        // a single monotonic high-water mark (see `BODY_CACHE_VERSION` below),
        // so a new messages-format change must raise this *above* every other
        // constant to trigger its wipe - a value below the stored maximum would
        // silently never fire.
        //
        // Version 1: pre-full-sync builds kept only a folder's newest ~200
        // messages, so their `messages` rows held a *subset* of the mailbox and
        // the cache-hit path would serve that subset forever; wiping forces a
        // full re-sync. Version 5: `EmailSummary` gained importance,
        // sensitivity and `List-Id`, read from a selected-header block only
        // newer builds request - cached rows can't be back-filled, so they are
        // wiped once and re-fetched with the header block. Version 7:
        // `EmailSummary` gained `mentions` (M-25), derived from the body
        // snippet the preview pass fetches - an already-cached row carries a
        // preview but no mention verdict, and the pass only runs for rows
        // *without* a preview, so older rows would stay unflagged forever;
        // wiping forces one full re-sync whose preview pass re-inspects every
        // body. Each version subsumes the earlier ones. Snoozes, addresses and
        // the mailbox list are untouched.
        const ENVELOPE_CACHE_VERSION: i64 = 7;
        // One-time body-cache migration: `bodies` rows changed format from
        // raw RFC 5322 bytes to serialized `EmailBody` JSON (the partial-fetch
        // path never assembles a whole raw message), so pre-partial-fetch
        // caches' raw rows can't be served and are wiped once. Envelope rows
        // survive; their `structure` stays `None` until the next sync, which
        // is exactly the fallback path's cue.
        // Version 6: `EmailBody` gained `auth_results` (the parsed RFC 8601
        // Authentication-Results verdicts, item 6.1). Old rows decode with it
        // `None`, and since it is *derived* from the raw headers at assembly
        // time it can't be back-filled from the stored JSON, so bodies are
        // wiped once and reassembled. A *body* bump must stay above
        // `ENVELOPE_CACHE_VERSION`, or the shared `user_version` high-water
        // mark would swallow it (the envelope bump to 7 already exceeds this
        // value).
        const BODY_CACHE_VERSION: i64 = 6;
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
            mention_matcher: std::sync::RwLock::new(lookout_core::MentionMatcher::default()),
        })
    }

    /// Installs the `@mentions` detection context for this account's session
    /// (M-25). `run_account_session` calls this once, from its `AccountConfig`.
    pub fn set_mention_matcher(&self, matcher: lookout_core::MentionMatcher) {
        *self.mention_matcher.write().unwrap() = matcher;
    }

    /// The `@mentions` detection context, or an empty matcher (matching
    /// nothing) if the session hasn't installed one.
    pub fn mention_matcher(&self) -> lookout_core::MentionMatcher {
        self.mention_matcher.read().unwrap().clone()
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
        // This is a full snapshot, so every row it writes is complete: the
        // `partial` arm of the guard promotes a prefetch-indexed row even when
        // its envelope is byte-identical.
        let mut upsert = tx.prepare_cached(
            "INSERT INTO messages (mailbox_id, uid, uidvalidity, data, partial) VALUES (?1, ?2, ?3, ?4, 0) \
             ON CONFLICT(mailbox_id, uid) DO UPDATE SET uidvalidity = excluded.uidvalidity, data = excluded.data, partial = 0 \
             WHERE messages.data IS NOT excluded.data OR messages.partial != 0",
        )?;
        for msg in messages {
            let data = serde_json::to_string(msg)?;
            let modified = upsert.execute(rusqlite::params![mailbox_id.0, msg.uid.0, uidvalidity.0, data])?;
            if modified > 0 {
                index_preserving_body(&tx, msg, !stored.contains_key(&msg.uid.0))?;
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
        tracing::debug!(mailbox = %mailbox_id.0, rows = messages.len(), "replace_messages committed");
        Ok(())
    }

    /// Upserts `messages` into `mailbox_id` without touching the rest of the
    /// folder: the incremental counterpart of `replace_messages`, for a source
    /// that reports only what changed, such as a Graph folder delta
    /// (MAIL_GRAPH_OVERVIEW.md item 5.7.4). Removals go through
    /// `delete_messages`. Rows are written complete (a `partial` prefetch row
    /// is promoted), and the search index is rewritten only for rows whose
    /// envelope changed, keeping any full-body index text `store_body` added.
    pub fn upsert_messages(&self, mailbox_id: &MailboxId, uidvalidity: UidValidity, messages: &[EmailSummary]) -> Result<()> {
        if messages.is_empty() {
            return Ok(());
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        {
            let mut exists = tx.prepare_cached("SELECT 1 FROM messages WHERE mailbox_id = ?1 AND uid = ?2")?;
            let mut upsert = tx.prepare_cached(
                "INSERT INTO messages (mailbox_id, uid, uidvalidity, data, partial) VALUES (?1, ?2, ?3, ?4, 0) \
                 ON CONFLICT(mailbox_id, uid) DO UPDATE SET uidvalidity = excluded.uidvalidity, data = excluded.data, partial = 0 \
                 WHERE messages.data IS NOT excluded.data OR messages.partial != 0",
            )?;
            for msg in messages {
                let is_new = !exists.exists(rusqlite::params![mailbox_id.0, msg.uid.0])?;
                let data = serde_json::to_string(msg)?;
                if upsert.execute(rusqlite::params![mailbox_id.0, msg.uid.0, uidvalidity.0, data])? > 0 {
                    index_preserving_body(&tx, msg, is_new)?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Indexes a never-opened folder's newest-N envelope window, as fetched
    /// by the background prefetch, so the folder's recent mail is searchable
    /// before the user ever opens it. The rows are stored `partial`: `search`
    /// and the per-uid lookups see them, but `has_messages`/`load_messages`
    /// don't, so the folder is never shown (or cache-hit-skipped) as if this
    /// window were its whole contents. The first real `sync_mailbox` reuses
    /// them as already-fetched envelopes and `replace_messages` promotes
    /// them to complete.
    ///
    /// A no-op once the folder holds any complete row - a full snapshot is
    /// fuller than this window, and the folder's own syncs keep it current.
    /// Otherwise the stored partial set is replaced by `messages`: rows that
    /// fell out of the window (or were expunged) go, with their index rows,
    /// bodies and files, so a re-run keeps the window bounded and fresh. A
    /// snoozed row is kept - it left the window, not the server. A row whose
    /// new envelope lacks a preview keeps the one an earlier pass derived.
    pub fn store_partial_messages(&self, mailbox_id: &MailboxId, uidvalidity: UidValidity, messages: &[EmailSummary]) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let complete: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM messages WHERE mailbox_id = ?1 AND partial = 0)", [&mailbox_id.0], |row| {
            row.get(0)
        })?;
        if complete {
            return Ok(());
        }
        let mut stored: HashMap<u32, (u32, Option<String>)> = HashMap::new();
        {
            let mut stmt = tx.prepare("SELECT uid, uidvalidity, data FROM messages WHERE mailbox_id = ?1")?;
            let rows = stmt.query_map([&mailbox_id.0], |row| Ok((row.get::<_, u32>(0)?, row.get::<_, u32>(1)?, row.get::<_, String>(2)?)))?;
            for row in rows {
                let (uid, validity, data) = row?;
                let preview = serde_json::from_str::<EmailSummary>(&data).ok().and_then(|m| m.preview);
                stored.insert(uid, (validity, preview));
            }
        }
        {
            let mut upsert = tx.prepare_cached(
                "INSERT INTO messages (mailbox_id, uid, uidvalidity, data, partial) VALUES (?1, ?2, ?3, ?4, 1) \
                 ON CONFLICT(mailbox_id, uid) DO UPDATE SET uidvalidity = excluded.uidvalidity, data = excluded.data \
                 WHERE messages.data IS NOT excluded.data",
            )?;
            for msg in messages {
                let mut msg = msg.clone();
                if msg.preview.is_none() {
                    if let Some((validity, Some(preview))) = stored.get(&msg.uid.0) {
                        if *validity == uidvalidity.0 {
                            msg.preview = Some(preview.clone());
                        }
                    }
                }
                let data = serde_json::to_string(&msg)?;
                if upsert.execute(rusqlite::params![mailbox_id.0, msg.uid.0, uidvalidity.0, data])? == 0 {
                    continue;
                }
                // A row new to the index whose body an earlier prefetch pass
                // already cached: that `store_bodies` found no envelope to
                // index against, and the body won't be fetched again, so it
                // seeds the full-text row (and the snippet) now.
                let is_new = stored.get(&msg.uid.0).is_none_or(|(validity, _)| *validity != uidvalidity.0);
                let cached_body: Option<EmailBody> = if is_new {
                    tx.query_row(
                        "SELECT data FROM bodies WHERE mailbox_id = ?1 AND uid = ?2 AND uidvalidity = ?3",
                        rusqlite::params![mailbox_id.0, msg.uid.0, uidvalidity.0],
                        |row| row.get::<_, Vec<u8>>(0),
                    )
                    .optional()?
                    .and_then(|data| serde_json::from_slice(&data).ok())
                } else {
                    None
                };
                match cached_body {
                    Some(body) => index_stored_body(&tx, mailbox_id, msg.uid, &body)?,
                    None => index_preserving_body(&tx, &msg, is_new)?,
                }
            }
        }
        let present: HashSet<u32> = messages.iter().map(|m| m.uid.0).collect();
        let snoozed: HashSet<u32> = {
            let mut stmt = tx.prepare("SELECT uid FROM snoozed WHERE mailbox_id = ?1")?;
            let rows = stmt.query_map([&mailbox_id.0], |row| row.get::<_, u32>(0))?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let mut purged: HashSet<(u32, u32)> = HashSet::new();
        for (&uid, &(stored_validity, _)) in &stored {
            if !present.contains(&uid) && !snoozed.contains(&uid) {
                delete_index_row(&tx, mailbox_id, Uid(uid))?;
                tx.execute("DELETE FROM messages WHERE mailbox_id = ?1 AND uid = ?2", rusqlite::params![mailbox_id.0, uid])?;
                tx.execute("DELETE FROM bodies WHERE mailbox_id = ?1 AND uid = ?2", rusqlite::params![mailbox_id.0, uid])?;
                purged.insert((uid, stored_validity));
            }
        }
        self.purge_message_files(mailbox_id, &purged);
        tx.commit()?;
        Ok(())
    }

    /// The folder's cached envelope snapshot, as written by `sync_mailbox` -
    /// what the list paints before (or instead of) a live sync. Excludes the
    /// prefetch's `partial` search-index rows: those are only the newest-N
    /// window of a folder never synced in full (see `store_partial_messages`).
    pub fn load_messages(&self, mailbox_id: &MailboxId) -> Result<Vec<EmailSummary>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached("SELECT data FROM messages WHERE mailbox_id = ?1 AND partial = 0")?;
        let rows = stmt.query_map([&mailbox_id.0], |row| row.get::<_, String>(0))?;
        let mut messages = Vec::new();
        for row in rows {
            if let Ok(msg) = serde_json::from_str::<EmailSummary>(&row?) {
                messages.push(msg);
            }
        }
        overlay_local(&conn, &mut messages)?;
        Ok(messages)
    }

    /// Loads cached summaries for `mailbox_id`, keyed by UID, but only the
    /// ones cached under `uidvalidity` - a mismatch (the mailbox was
    /// recreated, RFC 3501 §2.3.1.1) means a cached UID no longer names the
    /// same message, so those rows must never be reused as-is. Used by
    /// `sync_mailbox`'s incremental refresh to tell which UIDs already have
    /// an ENVELOPE/BODYSTRUCTURE worth keeping - those never change once a
    /// message exists, so only a UID missing here needs a full re-fetch.
    /// Includes the prefetch's `partial` rows, so a folder's first full sync
    /// doesn't re-download the window the prefetch already indexed.
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

    /// Returns `true` if the cache holds a full-sync snapshot for `mailbox_id`,
    /// without deserializing it. Used by the session actor to skip a
    /// redundant IMAP sync when the data is already cached, and by the
    /// prefetch to tell an opened folder from one whose envelopes it should
    /// index. The prefetch's `partial` rows don't count - serving them would
    /// show the newest-N window as the whole folder.
    pub fn has_messages(&self, mailbox_id: &MailboxId) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT 1 FROM messages WHERE mailbox_id = ?1 AND partial = 0 LIMIT 1")?;
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
        let mut stmt = conn.prepare_cached("SELECT data FROM messages WHERE mailbox_id = ?1 AND uid = ?2")?;
        let mut out = HashMap::new();
        for &uid in uids {
            let mut rows = stmt.query_map(rusqlite::params![mailbox_id.0, uid.0], |row| row.get::<_, String>(0))?;
            if let Some(Ok(data)) = rows.next() {
                if let Ok(summary) = serde_json::from_str::<EmailSummary>(&data) {
                    out.insert(uid, summary);
                }
            }
        }
        overlay_local(&conn, out.values_mut())?;
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
        index_stored_body(&tx, mailbox_id, uid, body)?;
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
            index_stored_body(&tx, mailbox_id, *uid, body)?;
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
    /// file so the OS can reclaim them - no full-file rewrite. It also drops
    /// Graph Uid mappings no cached message uses (`prune_graph_uids`).
    pub fn run_maintenance(&self) -> Result<()> {
        self.sweep_orphaned_files();
        self.prune_graph_uids()?;
        let conn = self.conn.lock().unwrap();
        const INCREMENTAL: i64 = 2;
        let mode: i64 = conn.query_row("PRAGMA auto_vacuum", [], |row| row.get(0))?;
        if mode != INCREMENTAL {
            conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
            conn.execute("VACUUM", [])?;
        }
        // `execute`, not `query`: unlike `PRAGMA auto_vacuum` above, this
        // pragma emits one output row per page it actually reclaims (empty
        // rows, but rows all the same) whenever there's anything to reclaim,
        // which makes rusqlite's `execute` reject it with "Execute returned
        // results" the moment a real cache has freed pages to sweep.
        // `execute_batch` runs via `sqlite3_exec`, which drains and discards
        // any rows instead of erroring on them.
        conn.execute_batch("PRAGMA incremental_vacuum")?;
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
    /// Rows whose date can't be parsed are skipped, as are the prefetch's
    /// `partial` search-index rows, so the chart reflects the synced folders
    /// it always has rather than shifting with how far the prefetch got.
    pub fn hour_histogram(&self, since: Option<DateTime<Utc>>) -> Result<[i64; 24]> {
        let conn = self.conn.lock().unwrap();
        // The cut is passed as the stored format (RFC 3339 UTC), which sorts
        // lexicographically like the serialized dates themselves.
        let (sql, since_string) = match since {
            Some(since) => (
                "SELECT json_extract(data, '$.date') FROM messages WHERE partial = 0 AND json_extract(data, '$.date') >= ?1",
                Some(since.to_rfc3339()),
            ),
            None => ("SELECT json_extract(data, '$.date') FROM messages WHERE partial = 0", None),
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
    /// prefetched. A folder never opened is covered by the background
    /// prefetch's `partial` rows - its newest-N window (see
    /// `store_partial_messages`) - once the prefetch pass reaches it. Older
    /// mail in such a folder, and body text for never-fetched messages, need
    /// the IMAP `SEARCH` fallback - see `AccountCommand::SearchMailbox`.
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
        let mut stmt = conn.prepare_cached(
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
            let mut stmt = conn.prepare_cached("SELECT mailbox_id, uid FROM snoozed WHERE snoozed_until > ?1")?;
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
        overlay_local(&conn, &mut out)?;
        Ok(out)
    }

    /// Structured search (M-18, roadmap 2.2) over this account's local index:
    /// the parsed query's text component runs through FTS5 and its remaining
    /// operators become `json_extract` predicates over `messages.data` (see
    /// [`crate::search_query::SearchQuery`]). `mailbox_scope` restricts the
    /// result to those mailboxes (`None` covers every folder of the account),
    /// letting the app's scope dropdown and `folder:` token narrow the sweep.
    /// Snoozed messages stay excluded, exactly as in [`Cache::search`].
    pub fn search_query(&self, query: &SearchQuery, mailbox_scope: Option<&[MailboxId]>, limit: usize) -> Result<Vec<EmailSummary>> {
        if query.is_empty() {
            return Ok(Vec::new());
        }
        if mailbox_scope.is_some_and(<[MailboxId]>::is_empty) {
            return Ok(Vec::new());
        }
        let fts = query.fts_match();
        let (predicates, predicate_params) = query.sql_predicates();
        let conn = self.conn.lock().unwrap();

        let mut sql = String::from("SELECT messages.data FROM messages");
        if fts.is_some() {
            sql.push_str(
                " JOIN search_fts ON messages.mailbox_id = search_fts.mailbox_id \
                  AND messages.uid = search_fts.uid",
            );
        }
        let mut wheres: Vec<String> = Vec::new();
        if fts.is_some() {
            wheres.push("search_fts MATCH ?".to_string());
        }
        wheres.extend(predicates);
        if let Some(scope) = mailbox_scope {
            let placeholders = vec!["?"; scope.len()].join(", ");
            wheres.push(format!("messages.mailbox_id IN ({placeholders})"));
        }
        sql.push_str(" WHERE ");
        sql.push_str(&wheres.join(" AND "));
        if fts.is_some() {
            sql.push_str(" ORDER BY rank LIMIT ?");
        } else {
            sql.push_str(" ORDER BY json_extract(messages.data, '$.date') DESC LIMIT ?");
        }

        let mut values: Vec<rusqlite::types::Value> = Vec::new();
        if let Some(fts) = &fts {
            values.push(rusqlite::types::Value::Text(fts.clone()));
        }
        values.extend(predicate_params);
        if let Some(scope) = mailbox_scope {
            for id in scope {
                values.push(rusqlite::types::Value::Text(id.0.clone()));
            }
        }
        values.push(rusqlite::types::Value::Integer(limit as i64));

        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(values), |row| row.get::<_, String>(0))?;
        let mut data_by_key: Vec<(String, u32, String)> = Vec::new();
        for row in rows {
            let data = row?;
            let Ok(msg) = serde_json::from_str::<EmailSummary>(&data) else {
                continue;
            };
            data_by_key.push((msg.mailbox.0.clone(), msg.uid.0, data));
        }

        let now = Utc::now().timestamp();
        let mut snoozed: HashSet<(String, u32)> = HashSet::new();
        {
            let mut stmt = conn.prepare_cached("SELECT mailbox_id, uid FROM snoozed WHERE snoozed_until > ?1")?;
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
        overlay_local(&conn, &mut out)?;
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
    /// are snoozed. The prefetch's `partial` search-index rows are left out
    /// too: their flags are only as fresh as the last prefetch pass, and the
    /// feed covers the folders the user actually syncs.
    pub fn recent_messages(&self, limit: usize) -> Result<Vec<EmailSummary>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT data FROM messages WHERE partial = 0 \
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
            let mut stmt = conn.prepare_cached("SELECT mailbox_id, uid FROM snoozed WHERE snoozed_until > ?1")?;
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
        overlay_local(&conn, &mut out)?;
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

    /// Patches in a fetched preview snippet (`previews`) and/or an `@mentions`
    /// verdict (`mentions`, M-25) for each uid, without the whole-mailbox
    /// diff/upsert/expunge-purge `replace_messages` does - `fetch_previews`
    /// calls this for the up-to-`PREVIEW_FETCH_LIMIT` messages it just fetched
    /// a snippet for, and a body fetch calls it with a mention-only map, not
    /// the whole mailbox.
    ///
    /// A uid may appear in either map or both (the preview pass derives both
    /// from one fetch), so the two are walked as a union: each row is parsed
    /// and rewritten once.
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
    pub fn update_previews(&self, mailbox_id: &MailboxId, previews: &HashMap<Uid, String>, mentions: &HashMap<Uid, bool>) -> Result<bool> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let mut all_found = true;
        {
            let mut select = tx.prepare("SELECT data FROM messages WHERE mailbox_id = ?1 AND uid = ?2")?;
            let mut update = tx.prepare("UPDATE messages SET data = ?1 WHERE mailbox_id = ?2 AND uid = ?3")?;
            let uids: HashSet<&Uid> = previews.keys().chain(mentions.keys()).collect();
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
                if let Some(preview) = previews.get(uid) {
                    summary.preview = Some(preview.clone());
                }
                if let Some(value) = mentions.get(uid) {
                    summary.mentions = *value;
                }
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

    /// Clears the snooze on `uids` in `mailbox_id` (the undo of
    /// `snooze_messages`), so they show in the folder again at once.
    pub fn unsnooze_messages(&self, mailbox_id: &MailboxId, uids: &[Uid]) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare("DELETE FROM snoozed WHERE mailbox_id = ?1 AND uid = ?2")?;
            for uid in uids {
                stmt.execute(rusqlite::params![mailbox_id.0, uid.0])?;
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
    pub fn purge_expired_snoozed(&self, now: DateTime<Utc>) -> Result<Vec<MailboxId>> {
        let conn = self.conn.lock().unwrap();
        let woken: Vec<MailboxId> = {
            let mut stmt = conn.prepare("SELECT DISTINCT mailbox_id FROM snoozed WHERE snoozed_until <= ?1")?;
            let rows = stmt.query_map(rusqlite::params![now.timestamp()], |row| row.get::<_, String>(0))?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?.into_iter().map(MailboxId).collect()
        };
        conn.execute("DELETE FROM snoozed WHERE snoozed_until <= ?1", rusqlite::params![now.timestamp()])?;
        Ok(woken)
    }

    /// When the earliest local snooze ends, if any - what the session caps
    /// its IDLE slice against so a local-only snooze (item 1.15's fallback
    /// on servers that can't store keywords) wakes on time instead of at the
    /// next sync.
    pub fn next_snooze_wake(&self) -> Result<Option<DateTime<Utc>>> {
        let conn = self.conn.lock().unwrap();
        let ts: Option<i64> = conn.query_row("SELECT MIN(snoozed_until) FROM snoozed", [], |row| row.get(0))?;
        Ok(ts.and_then(|t| DateTime::from_timestamp(t, 0)))
    }

    /// Records follow-up flags for messages whose folder can't store custom
    /// keywords (item 1.16; Exchange IMAP lacks `\*` in `PERMANENTFLAGS`), or
    /// that Graph reported. Keyed by [`follow_up_key`] - the Message-ID where
    /// there is one - so the flag follows the message across moves and
    /// matches Graph's `internetMessageId`. A `NotFlagged` row is kept as a
    /// tombstone: it overrides a stale flag keyword and stops an older Graph
    /// snapshot from resurrecting a flag cleared here.
    pub fn set_local_follow_ups(&self, summaries: &[EmailSummary], flag: &FollowUpFlag, now: DateTime<Utc>) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare("INSERT OR REPLACE INTO follow_up_flags (message_key, status, due, completed, updated_at) VALUES (?1, ?2, ?3, ?4, ?5)")?;
            for summary in summaries {
                stmt.execute(rusqlite::params![
                    follow_up_key(summary),
                    flag_status_code(flag.status),
                    flag.due.map(|d| d.timestamp()),
                    flag.completed.map(|d| d.timestamp()),
                    now.timestamp()
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Drops the local follow-up rows for `summaries` - their keywords on the
    /// server are authoritative again (a keyword STORE just succeeded).
    pub fn clear_local_follow_ups(&self, summaries: &[EmailSummary]) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare("DELETE FROM follow_up_flags WHERE message_key = ?1")?;
            for summary in summaries {
                stmt.execute([follow_up_key(summary)])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Records locally the categories (Lookout `$Lookout-tag-<key>` keys) for
    /// messages whose folder can't store custom keywords (Exchange IMAP,
    /// item 5.4) or that Graph reported - the category counterpart of
    /// [`Cache::set_local_follow_ups`]. An empty `keys` clears the row.
    pub fn set_local_categories(&self, summaries: &[EmailSummary], keys: &[String], now: DateTime<Utc>) -> Result<()> {
        if keys.is_empty() {
            return self.clear_local_categories(summaries);
        }
        let json = serde_json::to_string(keys)?;
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare("INSERT OR REPLACE INTO local_categories (message_key, category_keys, updated_at) VALUES (?1, ?2, ?3)")?;
            for summary in summaries {
                stmt.execute(rusqlite::params![follow_up_key(summary), json, now.timestamp()])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Applies a tag-keyword change to each message's local category row
    /// (item 5.4) - the Exchange-IMAP fallback for a keyword `STORE` that the
    /// server can't keep. `add`/`remove` are Lookout category *keys*; each
    /// summary's effective key set is its current tag keywords adjusted by
    /// them, and an empty result clears the row.
    pub fn update_local_categories(&self, summaries: &[EmailSummary], add: &[String], remove: &[String], now: DateTime<Utc>) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        {
            let mut upsert = tx.prepare("INSERT OR REPLACE INTO local_categories (message_key, category_keys, updated_at) VALUES (?1, ?2, ?3)")?;
            let mut clear = tx.prepare("DELETE FROM local_categories WHERE message_key = ?1")?;
            for summary in summaries {
                let mut keys: std::collections::BTreeSet<&str> = summary.keywords.iter().filter_map(|keyword| lookout_core::tag_key_from_keyword(keyword)).collect();
                for key in add {
                    keys.insert(key.as_str());
                }
                for key in remove {
                    keys.remove(key.as_str());
                }
                let message_key = follow_up_key(summary);
                if keys.is_empty() {
                    clear.execute([message_key])?;
                } else {
                    let list: Vec<&str> = keys.into_iter().collect();
                    upsert.execute(rusqlite::params![message_key, serde_json::to_string(&list)?, now.timestamp()])?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Drops the local category rows for `summaries` - their keywords on the
    /// server are authoritative again (a keyword STORE just succeeded).
    pub fn clear_local_categories(&self, summaries: &[EmailSummary]) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare("DELETE FROM local_categories WHERE message_key = ?1")?;
            for summary in summaries {
                stmt.execute([follow_up_key(summary)])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Records locally a message's importance (item 5.4) where the header can't
    /// carry it reliably; `Normal` clears the row (the default needs no overlay).
    pub fn set_local_importance(&self, summaries: &[EmailSummary], importance: Importance, now: DateTime<Utc>) -> Result<()> {
        if importance == Importance::Normal {
            return self.clear_local_importance(summaries);
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare("INSERT OR REPLACE INTO local_importance (message_key, importance, updated_at) VALUES (?1, ?2, ?3)")?;
            for summary in summaries {
                stmt.execute(rusqlite::params![follow_up_key(summary), importance_code(importance), now.timestamp()])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Drops the local importance rows for `summaries`.
    pub fn clear_local_importance(&self, summaries: &[EmailSummary]) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare("DELETE FROM local_importance WHERE message_key = ?1")?;
            for summary in summaries {
                stmt.execute([follow_up_key(summary)])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Records locally a message's Focused/Other classification (M-03) where
    /// Graph carries it (Exchange IMAP exposes no such flag); `Focused` clears
    /// the row (the default needs no overlay).
    pub fn set_local_inference(&self, summaries: &[EmailSummary], classification: InferenceClassification, now: DateTime<Utc>) -> Result<()> {
        if classification == InferenceClassification::Focused {
            return self.clear_local_inference(summaries);
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare("INSERT OR REPLACE INTO local_inference (message_key, classification, updated_at) VALUES (?1, ?2, ?3)")?;
            for summary in summaries {
                stmt.execute(rusqlite::params![follow_up_key(summary), classification_code(classification), now.timestamp()])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Drops the local Focused/Other rows for `summaries`.
    pub fn clear_local_inference(&self, summaries: &[EmailSummary]) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare("DELETE FROM local_inference WHERE message_key = ?1")?;
            for summary in summaries {
                stmt.execute([follow_up_key(summary)])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// The Graph id of `message_key` (a bare Message-ID), if the map has it.
    pub fn graph_id(&self, message_key: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row("SELECT graph_id FROM message_graph_ids WHERE message_key = ?1", [message_key], |row| {
                row.get::<_, String>(0)
            })
            .optional()?)
    }

    /// The Graph ids of `message_keys`, keyed by the same bare Message-IDs -
    /// what the write path resolves before a Graph `PATCH` (item 5.4).
    pub fn graph_ids(&self, message_keys: &[String]) -> Result<HashMap<String, String>> {
        if message_keys.is_empty() {
            return Ok(HashMap::new());
        }
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT graph_id FROM message_graph_ids WHERE message_key = ?1")?;
        let mut out = HashMap::new();
        for key in message_keys {
            if let Some(id) = stmt.query_row([key], |row| row.get::<_, String>(0)).optional()? {
                out.insert(key.clone(), id);
            }
        }
        Ok(out)
    }

    /// The stored `/me/messages/delta` cursor, if a sync has persisted one.
    pub fn graph_delta_cursor(&self) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row("SELECT cursor FROM graph_delta_cursor WHERE id = 1", [], |row| row.get::<_, String>(0))
            .optional()?)
    }

    /// The locally-stored Focused/Other sender overrides (M-03).
    pub fn inference_overrides(&self) -> Result<Vec<InferenceOverrideRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT sender, classify_as, override_id FROM inference_overrides")?;
        let rows = stmt.query_map([], |row| {
            Ok(InferenceOverrideRow {
                sender: row.get(0)?,
                classify_as: classification_from_code(row.get(1)?),
                override_id: row.get(2)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Records a sender override (M-03), with Graph's override id once known.
    pub fn set_inference_override(&self, sender: &str, classify_as: InferenceClassification, override_id: Option<&str>, now: DateTime<Utc>) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO inference_overrides (sender, classify_as, override_id, updated_at) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![sender, classification_code(classify_as), override_id, now.timestamp()],
        )?;
        Ok(())
    }

    /// Drops a sender override ("stop moving").
    pub fn clear_inference_override(&self, sender: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM inference_overrides WHERE sender = ?1", [sender])?;
        Ok(())
    }

    /// Replaces the local Focused/Other override rows with a Graph snapshot
    /// (M-03 / item 5.4), keyed by sender. A row changed here after `taken_at`
    /// (an override just set, not yet on Graph) is kept as it is.
    pub fn apply_inference_overrides(&self, overrides: &[(String, InferenceClassification, Option<String>)], taken_at: DateTime<Utc>) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM inference_overrides WHERE updated_at <= ?1", [taken_at.timestamp()])?;
        {
            let mut stmt = tx.prepare("INSERT OR IGNORE INTO inference_overrides (sender, classify_as, override_id, updated_at) VALUES (?1, ?2, ?3, ?4)")?;
            for (sender, classification, override_id) in overrides {
                stmt.execute(rusqlite::params![sender, classification_code(*classification), override_id, taken_at.timestamp()])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Applies one page of the `/me/messages/delta` feed (item 5.4): records
    /// each message's Graph id and its non-default Outlook properties, drops
    /// the overlay rows for the tombstones in `removed_graph_ids` (a deleted or
    /// moved-out message), and persists `delta_cursor`. Because the delta is
    /// *incremental* (unlike the flagged snapshot), only the messages it names
    /// are touched; a local change written after `taken_at` is left alone, so
    /// an in-flight snapshot can't undo it.
    pub fn apply_remote_mail_properties(
        &self,
        properties: &[RemoteMailProperties],
        removed_graph_ids: &[String],
        delta_cursor: Option<&str>,
        taken_at: DateTime<Utc>,
    ) -> Result<()> {
        let ts = taken_at.timestamp();
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        {
            let mut upsert_id = tx.prepare(
                "INSERT INTO message_graph_ids (message_key, graph_id, updated_at) VALUES (?1, ?2, ?3) \
                 ON CONFLICT(message_key) DO UPDATE SET graph_id = excluded.graph_id, updated_at = excluded.updated_at",
            )?;
            let mut clear_cat = tx.prepare("DELETE FROM local_categories WHERE message_key = ?1 AND updated_at <= ?2")?;
            let mut set_cat = tx.prepare(
                "INSERT INTO local_categories (message_key, category_keys, updated_at) VALUES (?1, ?2, ?3) \
                 ON CONFLICT(message_key) DO UPDATE SET category_keys = excluded.category_keys, updated_at = excluded.updated_at \
                 WHERE local_categories.updated_at <= excluded.updated_at",
            )?;
            let mut clear_imp = tx.prepare("DELETE FROM local_importance WHERE message_key = ?1 AND updated_at <= ?2")?;
            let mut set_imp = tx.prepare(
                "INSERT INTO local_importance (message_key, importance, updated_at) VALUES (?1, ?2, ?3) \
                 ON CONFLICT(message_key) DO UPDATE SET importance = excluded.importance, updated_at = excluded.updated_at \
                 WHERE local_importance.updated_at <= excluded.updated_at",
            )?;
            let mut clear_cls = tx.prepare("DELETE FROM local_inference WHERE message_key = ?1 AND updated_at <= ?2")?;
            let mut set_cls = tx.prepare(
                "INSERT INTO local_inference (message_key, classification, updated_at) VALUES (?1, ?2, ?3) \
                 ON CONFLICT(message_key) DO UPDATE SET classification = excluded.classification, updated_at = excluded.updated_at \
                 WHERE local_inference.updated_at <= excluded.updated_at",
            )?;
            let mut clear_flag = tx.prepare("DELETE FROM follow_up_flags WHERE message_key = ?1 AND updated_at <= ?2")?;
            let mut set_flag = tx.prepare(
                "INSERT INTO follow_up_flags (message_key, status, due, completed, updated_at) VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT(message_key) DO UPDATE SET status = excluded.status, due = excluded.due, completed = excluded.completed, updated_at = excluded.updated_at \
                 WHERE follow_up_flags.updated_at <= excluded.updated_at",
            )?;

            for p in properties {
                let key = bare_message_id(&p.message_id);
                if key.is_empty() {
                    continue;
                }
                upsert_id.execute(rusqlite::params![key, p.graph_id, ts])?;
                if p.categories.is_empty() {
                    clear_cat.execute(rusqlite::params![key, ts])?;
                } else {
                    set_cat.execute(rusqlite::params![key, serde_json::to_string(&p.categories)?, ts])?;
                }
                if p.importance == Importance::Normal {
                    clear_imp.execute(rusqlite::params![key, ts])?;
                } else {
                    set_imp.execute(rusqlite::params![key, importance_code(p.importance), ts])?;
                }
                if p.classification == InferenceClassification::Focused {
                    clear_cls.execute(rusqlite::params![key, ts])?;
                } else {
                    set_cls.execute(rusqlite::params![key, classification_code(p.classification), ts])?;
                }
                match &p.flag {
                    Some(flag) if flag.status != FlagStatus::NotFlagged => {
                        set_flag.execute(rusqlite::params![
                            key,
                            flag_status_code(flag.status),
                            flag.due.map(|d| d.timestamp()),
                            flag.completed.map(|d| d.timestamp()),
                            ts
                        ])?;
                    }
                    _ => {
                        clear_flag.execute(rusqlite::params![key, ts])?;
                    }
                }
            }

            let mut find_key = tx.prepare("SELECT message_key FROM message_graph_ids WHERE graph_id = ?1 LIMIT 1")?;
            for graph_id in removed_graph_ids {
                let key: Option<String> = find_key.query_row([graph_id], |row| row.get(0)).optional()?;
                if let Some(key) = key {
                    for table in ["local_categories", "local_importance", "local_inference", "follow_up_flags"] {
                        tx.execute(&format!("DELETE FROM {table} WHERE message_key = ?1"), [&key])?;
                    }
                }
                tx.execute("DELETE FROM message_graph_ids WHERE graph_id = ?1", [graph_id])?;
            }

            if let Some(cursor) = delta_cursor {
                tx.execute(
                    "INSERT INTO graph_delta_cursor (id, cursor, updated_at) VALUES (1, ?1, ?2) \
                     ON CONFLICT(id) DO UPDATE SET cursor = excluded.cursor, updated_at = excluded.updated_at",
                    rusqlite::params![cursor, ts],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// The Graph mail transport's `UidValidity` for this account
    /// (MAIL_GRAPH_OVERVIEW.md item 5.7.3): one value for every folder, fixed
    /// (at random) when the Uid map is created. It changes only when the map
    /// is lost (the cache file deleted or cleared), which is exactly when the
    /// Uids it vouches for stop meaning anything. An expired Graph delta
    /// cursor keeps it.
    pub fn graph_uid_validity(&self) -> Result<UidValidity> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let validity = graph_uid_space(&tx)?.0;
        tx.commit()?;
        Ok(validity)
    }

    /// The synthetic `Uid` of each Graph message id in `graph_ids`, in the same
    /// order: the existing Uid for an id seen before, otherwise a new one, handed
    /// out in the order given (the session passes new messages oldest-first).
    /// Uids are unique across the account, start at 1 and are never reused.
    /// One `IMMEDIATE` transaction, so two handles on the same file can't hand
    /// out the same Uid. Fails rather than wraps if the account ever exhausts
    /// `u32` Uids.
    pub fn graph_uids(&self, graph_ids: &[&str]) -> Result<Vec<Uid>> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let (_, mut next) = graph_uid_space(&tx)?;
        let mut uids = Vec::with_capacity(graph_ids.len());
        {
            let mut find = tx.prepare_cached("SELECT uid FROM graph_uids WHERE graph_id = ?1")?;
            let mut insert = tx.prepare_cached("INSERT INTO graph_uids (graph_id, uid) VALUES (?1, ?2)")?;
            for graph_id in graph_ids {
                if let Some(uid) = find.query_row([graph_id], |row| row.get::<_, u32>(0)).optional()? {
                    uids.push(Uid(uid));
                    continue;
                }
                let uid = u32::try_from(next).map_err(|_| std::io::Error::other("the account has run out of Graph message Uids"))?;
                insert.execute(rusqlite::params![graph_id, uid])?;
                next += 1;
                uids.push(Uid(uid));
            }
        }
        tx.execute("UPDATE graph_uid_space SET next_uid = ?1 WHERE id = 1", [next])?;
        tx.commit()?;
        Ok(uids)
    }

    /// The Uid behind each Graph message id in `graph_ids` that the map
    /// already knows - the non-allocating counterpart of
    /// [`Self::graph_uids`], for ids that arrive as delta tombstones: a
    /// message deleted before this cache ever saw it must not burn a Uid.
    pub fn graph_uids_existing(&self, graph_ids: &[String]) -> Result<HashMap<String, Uid>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached("SELECT uid FROM graph_uids WHERE graph_id = ?1")?;
        let mut uids = HashMap::new();
        for graph_id in graph_ids {
            if let Some(uid) = stmt.query_row([graph_id], |row| row.get::<_, u32>(0)).optional()? {
                uids.insert(graph_id.clone(), Uid(uid));
            }
        }
        Ok(uids)
    }

    /// The Graph message id behind each of `uids` that has one; Uids with no
    /// mapping are left out.
    pub fn graph_ids_for(&self, uids: &[Uid]) -> Result<HashMap<Uid, String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached("SELECT graph_id FROM graph_uids WHERE uid = ?1")?;
        let mut ids = HashMap::new();
        for uid in uids {
            if let Some(graph_id) = stmt.query_row([uid.0], |row| row.get::<_, String>(0)).optional()? {
                ids.insert(*uid, graph_id);
            }
        }
        Ok(ids)
    }

    /// Drops the Graph Uid mappings no cached message uses any more, returning
    /// how many went. Uids are account-unique on the Graph transport, so "no
    /// `messages` row with this uid" is exact. Run from `run_maintenance`,
    /// before any sync: a message that left one folder and hasn't been seen in
    /// another yet simply gets a new Uid when it reappears. A no-op on IMAP
    /// accounts, whose map is empty.
    pub fn prune_graph_uids(&self) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute("DELETE FROM graph_uids WHERE NOT EXISTS (SELECT 1 FROM messages WHERE messages.uid = graph_uids.uid)", [])?)
    }

    /// Applies every locally-stored Outlook overlay (follow-up flags,
    /// categories, importance, Focused/Other) to `messages`, for summaries that
    /// came from the server rather than from a cache read, which overlays them
    /// already.
    pub fn overlay_follow_ups(&self, messages: &mut [EmailSummary]) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        overlay_local(&conn, messages)
    }

    /// Every cached message carrying a follow-up flag (open or complete),
    /// across the account's folders - the Tasks view's "Flagged email"
    /// section (item 1.16). Includes the prefetch's `partial` rows, so flagged
    /// mail in a folder never opened still shows. A message cached in several
    /// folders (a copy) is listed once.
    pub fn flagged_messages(&self) -> Result<Vec<EmailSummary>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT data FROM messages WHERE data LIKE '%$Lookout-flag%' \
             OR trim(json_extract(data, '$.message_id'), '<> ') IN (SELECT message_key FROM follow_up_flags WHERE status != 0)",
        )?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut messages = Vec::new();
        for row in rows {
            if let Ok(msg) = serde_json::from_str::<EmailSummary>(&row?) {
                messages.push(msg);
            }
        }
        overlay_local(&conn, &mut messages)?;
        let mut seen = HashSet::new();
        messages.retain(|m| m.follow_up().is_some() && seen.insert(follow_up_key(m)));
        Ok(messages)
    }

    /// The messages of `mailbox_id` that belong to a conversation known by
    /// `message_ids` (bare or `<>`-bracketed Message-IDs): those whose own
    /// Message-ID, In-Reply-To or any References entry is one of them. The
    /// reading pane's conversation view (item 1.7) uses it to pull the user's
    /// replies out of Sent Items, whose thread keys are computed per folder
    /// and so never match the list's. Includes the prefetch's `partial` rows,
    /// so a Sent folder never opened in full still contributes, and overlays
    /// local follow-up flags like every other read path.
    pub fn conversation_members_in(&self, mailbox_id: &MailboxId, message_ids: &[String]) -> Result<Vec<EmailSummary>> {
        let ids: Vec<String> = message_ids.iter().map(|id| bare_message_id(id)).filter(|id| !id.is_empty()).collect();
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let ids_json = serde_json::to_string(&ids)?;
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT data FROM messages WHERE mailbox_id = ?1 AND ( \
               trim(json_extract(data, '$.message_id'), '<> ') IN (SELECT value FROM json_each(?2)) \
               OR trim(json_extract(data, '$.in_reply_to'), '<> ') IN (SELECT value FROM json_each(?2)) \
               OR EXISTS (SELECT 1 FROM json_each(data, '$.references') AS r \
                          WHERE trim(r.value, '<> ') IN (SELECT value FROM json_each(?2))))",
        )?;
        let rows = stmt.query_map(rusqlite::params![mailbox_id.0, ids_json], |row| row.get::<_, String>(0))?;
        let mut messages = Vec::new();
        for row in rows {
            if let Ok(msg) = serde_json::from_str::<EmailSummary>(&row?) {
                messages.push(msg);
            }
        }
        overlay_local(&conn, &mut messages)?;
        Ok(messages)
    }

    /// Adds an outgoing message to the local outbox, due at `not_before`
    /// (`Utc::now()` for a normal send, a future time for schedule send).
    /// Returns the new row's id, which the session quotes in its
    /// `OutboxQueued`/`OutboxSent`/`OutboxFailed` events.
    pub fn enqueue_outbox(&self, account_id: &AccountId, not_before: DateTime<Utc>, payload: &[u8]) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO outbox (account_id, not_before, attempts, payload, state) VALUES (?1, ?2, 0, ?3, ?4)",
            rusqlite::params![account_id.0, not_before.timestamp(), payload, OutboxState::Pending.as_str()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// The outbox rows due to send as of `now`: `pending` rows whose time has
    /// arrived, oldest first. `failed` rows are excluded - they only move
    /// again when the user retries them (item 1.14).
    pub fn due_outbox(&self, now: DateTime<Utc>) -> Result<Vec<OutboxEntry>> {
        self.outbox_rows("WHERE state = 'pending' AND not_before <= ?1 ORDER BY not_before, id", rusqlite::params![now.timestamp()])
    }

    /// Every outbox row, newest first - the outbox view's list (item 1.14).
    pub fn all_outbox(&self) -> Result<Vec<OutboxEntry>> {
        self.outbox_rows("ORDER BY id DESC", [])
    }

    /// The `not_before` of the earliest pending row, if any - what the session
    /// caps its IDLE slice against so a scheduled send wakes on time.
    pub fn next_outbox_not_before(&self) -> Result<Option<DateTime<Utc>>> {
        let conn = self.conn.lock().unwrap();
        let ts: Option<i64> = conn.query_row("SELECT MIN(not_before) FROM outbox WHERE state = 'pending'", [], |row| row.get(0))?;
        Ok(ts.and_then(|t| DateTime::from_timestamp(t, 0)))
    }

    /// Records a failed send attempt: increments `attempts` and, once it
    /// reaches `max_attempts`, marks the row `failed`; otherwise leaves it
    /// `pending` with `not_before` pushed out to `retry_at`. Returns the
    /// row's resulting state.
    pub fn outbox_attempt_failed(&self, id: i64, retry_at: DateTime<Utc>, max_attempts: u32) -> Result<OutboxState> {
        let conn = self.conn.lock().unwrap();
        let attempts: i64 = conn
            .query_row("SELECT attempts FROM outbox WHERE id = ?1", rusqlite::params![id], |row| row.get(0))
            .optional()?
            .unwrap_or(0);
        let attempts = attempts + 1;
        let state = if attempts >= i64::from(max_attempts) {
            OutboxState::Failed
        } else {
            OutboxState::Pending
        };
        conn.execute(
            "UPDATE outbox SET attempts = ?1, not_before = ?2, state = ?3 WHERE id = ?4",
            rusqlite::params![attempts, retry_at.timestamp(), state.as_str(), id],
        )?;
        Ok(state)
    }

    /// Removes an outbox row - after a successful send, or to drop a payload
    /// that can't be decoded.
    pub fn delete_outbox(&self, id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM outbox WHERE id = ?1", rusqlite::params![id])?;
        Ok(())
    }

    /// Withdraws an outbox row that hasn't gone out yet - undo send (item
    /// 1.13), and schedule send's "Cancel send"/"Edit" (item 1.14). A
    /// `pending` or permanently `failed` row is removed and returned, so the
    /// caller can reopen it in the composer; a row already sent (deleted)
    /// returns `None`.
    pub fn cancel_outbox(&self, id: i64) -> Result<Option<OutboxEntry>> {
        let conn = self.conn.lock().unwrap();
        let entry = Self::outbox_rows_locked(&conn, "WHERE id = ?1", rusqlite::params![id])?.into_iter().next();
        if entry.is_some() {
            conn.execute("DELETE FROM outbox WHERE id = ?1", rusqlite::params![id])?;
        }
        Ok(entry)
    }

    /// Moves an outbox row's due time to `not_before` and makes it `pending`
    /// again with a fresh retry budget - schedule send's "Send now" and a
    /// failed row's "Retry" (item 1.14). Returns whether the row still
    /// existed (it may have gone out in the meantime).
    pub fn reschedule_outbox(&self, id: i64, not_before: DateTime<Utc>) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let updated = conn.execute(
            "UPDATE outbox SET not_before = ?1, attempts = 0, state = ?2 WHERE id = ?3",
            rusqlite::params![not_before.timestamp(), OutboxState::Pending.as_str(), id],
        )?;
        Ok(updated > 0)
    }

    /// Shared row reader for the outbox queries.
    fn outbox_rows<P: rusqlite::Params>(&self, clause: &str, params: P) -> Result<Vec<OutboxEntry>> {
        let conn = self.conn.lock().unwrap();
        Self::outbox_rows_locked(&conn, clause, params)
    }

    fn outbox_rows_locked<P: rusqlite::Params>(conn: &Connection, clause: &str, params: P) -> Result<Vec<OutboxEntry>> {
        let mut stmt = conn.prepare(&format!("SELECT id, account_id, not_before, attempts, payload, state FROM outbox {clause}"))?;
        let rows = stmt.query_map(params, |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Vec<u8>>(4)?,
                row.get::<_, String>(5)?,
            ))
        })?;
        let mut entries = Vec::new();
        for row in rows {
            let (id, account_id, not_before, attempts, payload, state) = row?;
            entries.push(OutboxEntry {
                id,
                account_id: AccountId(account_id),
                not_before: DateTime::from_timestamp(not_before, 0).unwrap_or_else(Utc::now),
                attempts: u32::try_from(attempts).unwrap_or(0),
                payload,
                state: OutboxState::from_db(&state),
            });
        }
        Ok(entries)
    }

    /// Every distinct mailbox id with a cached row (messages, bodies, the
    /// folder list or snoozes) that is `root` itself or lies beneath it.
    /// Filtered in Rust with `is_same_or_descendant` rather than a SQL `LIKE`,
    /// so a `%` or `_` in a folder name can't widen the match.
    fn cached_ids_in_tree(conn: &Connection, root: &MailboxId, delimiter: char) -> Result<Vec<MailboxId>> {
        let mut ids: HashSet<String> = HashSet::new();
        for table in ["messages", "bodies", "mailboxes", "snoozed"] {
            let mut stmt = conn.prepare(&format!("SELECT DISTINCT mailbox_id FROM {table}"))?;
            for id in stmt.query_map([], |row| row.get::<_, String>(0))? {
                ids.insert(id?);
            }
        }
        Ok(ids
            .into_iter()
            .map(MailboxId)
            .filter(|id| crate::mailbox_name::is_same_or_descendant(id, root, delimiter))
            .collect())
    }

    /// Forgets everything cached for the folder `root` and every folder
    /// beneath it - envelopes, bodies, search-index rows, snoozes, the folder
    /// list's own rows, and the attachment/raw-message files - after that
    /// subtree was renamed, moved or deleted on the server. Envelope rows
    /// carry their mailbox id inside their serialized data, so a rename is
    /// handled by forgetting the old ids and letting the folder resync under
    /// its new one rather than by rewriting every row; call `rekey_snoozed`
    /// first to carry snoozes across a rename.
    pub fn forget_mailbox_tree(&self, root: &MailboxId, delimiter: char) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let ids = Self::cached_ids_in_tree(&conn, root, delimiter)?;
        let tx = conn.transaction()?;
        let mut purged_by_mailbox: Vec<(MailboxId, HashSet<(u32, u32)>)> = Vec::new();
        for id in &ids {
            let mut purged = HashSet::new();
            {
                let mut stmt = tx.prepare_cached("SELECT uid, uidvalidity FROM messages WHERE mailbox_id = ?1")?;
                for row in stmt.query_map([&id.0], |row| Ok((row.get::<_, u32>(0)?, row.get::<_, u32>(1)?)))? {
                    purged.insert(row?);
                }
            }
            for table in ["messages", "bodies", "search_fts", "mailboxes", "snoozed"] {
                tx.execute(&format!("DELETE FROM {table} WHERE mailbox_id = ?1"), [&id.0])?;
            }
            purged_by_mailbox.push((id.clone(), purged));
        }
        tx.commit()?;
        drop(conn);
        for (id, purged) in &purged_by_mailbox {
            self.purge_message_files(id, purged);
        }
        Ok(())
    }

    /// Moves the snoozes recorded under the folder `from` (and beneath it)
    /// across to the same places under `to`, so a renamed or moved folder's
    /// snoozed messages stay hidden. IMAP keeps a mailbox's UIDs across a
    /// `RENAME` (RFC 3501 §6.3.5), so each `(mailbox, uid)` stays valid.
    pub fn rekey_snoozed(&self, from: &MailboxId, to: &MailboxId, delimiter: char) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let ids = Self::cached_ids_in_tree(&conn, from, delimiter)?;
        let tx = conn.transaction()?;
        for id in &ids {
            if let Some(new_id) = crate::mailbox_name::rekey_mailbox_id(id, from, to, delimiter) {
                tx.execute("UPDATE OR REPLACE snoozed SET mailbox_id = ?1 WHERE mailbox_id = ?2", rusqlite::params![new_id.0, id.0])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Marks every cached message in `mailbox_id` `\Seen`, mirroring the
    /// whole-mailbox `UID STORE 1:* +FLAGS (\Seen)` behind "Mark all as read".
    pub fn mark_mailbox_seen(&self, mailbox_id: &MailboxId) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        {
            let mut select = tx.prepare("SELECT uid, data FROM messages WHERE mailbox_id = ?1")?;
            let mut update = tx.prepare("UPDATE messages SET data = ?1 WHERE mailbox_id = ?2 AND uid = ?3")?;
            let rows: Vec<(u32, String)> = select.query_map([&mailbox_id.0], |row| Ok((row.get(0)?, row.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
            for (uid, data) in rows {
                let Ok(mut summary) = serde_json::from_str::<EmailSummary>(&data) else { continue };
                if summary.flags.insert(SystemFlagBit::Seen) {
                    update.execute(rusqlite::params![serde_json::to_string(&summary)?, mailbox_id.0, uid])?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }
}

impl ContactsProvider for Cache {
    fn search_contacts(&self, prefix: &str, limit: usize) -> Vec<EmailAddress> {
        self.search_addresses(prefix, limit).unwrap_or_default()
    }
}

/// The account's Graph Uid space `(validity, next_uid)`, created on first use
/// with a random nonzero validity and Uid 1 next. Random rather than the
/// IMAP-style creation time: a map deleted and recreated within the same
/// second would otherwise get the same validity, and Uids still held in memory
/// from the old map would silently alias new messages. Runs inside the
/// caller's transaction.
fn graph_uid_space(tx: &rusqlite::Transaction<'_>) -> Result<(UidValidity, i64)> {
    if let Some((validity, next)) = tx
        .query_row("SELECT uidvalidity, next_uid FROM graph_uid_space WHERE id = 1", [], |row| {
            Ok((row.get::<_, u32>(0)?, row.get::<_, i64>(1)?))
        })
        .optional()?
    {
        return Ok((UidValidity(validity), next));
    }
    let validity = (uuid::Uuid::new_v4().as_u128() as u32).max(1);
    tx.execute("INSERT INTO graph_uid_space (id, uidvalidity, next_uid) VALUES (1, ?1, 1)", [validity])?;
    Ok((UidValidity(validity), 1))
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

        assert_eq!(
            cache.next_snooze_wake().unwrap().map(|t| t.timestamp()),
            Some((now - chrono::Duration::hours(1)).timestamp())
        );
        assert_eq!(
            cache.purge_expired_snoozed(now).unwrap(),
            vec![mailbox_id.clone()],
            "reports the mailbox it woke messages in"
        );
        assert_eq!(
            cache.next_snooze_wake().unwrap().map(|t| t.timestamp()),
            Some((now + chrono::Duration::hours(1)).timestamp())
        );
        assert!(cache.purge_expired_snoozed(now).unwrap().is_empty());

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

    /// `UnsnoozeMessages` (undo snooze): clearing a snooze shows the listed
    /// uids again and leaves the others snoozed.
    #[test]
    fn unsnoozing_clears_only_the_listed_uids() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let mailbox_id = MailboxId::new(&account_id, "INBOX");
        let now = Utc::now();

        cache.snooze_messages(&mailbox_id, &[Uid(1), Uid(2), Uid(3)], now + chrono::Duration::hours(1)).unwrap();
        cache.unsnooze_messages(&mailbox_id, &[Uid(1), Uid(3), Uid(9)]).unwrap();
        assert_eq!(cache.active_snoozed_uids(&mailbox_id, now).unwrap(), HashSet::from([Uid(2)]));

        let path = cache_dir().join(format!("{}.sqlite3", sanitize_filename(&account_id)));
        let _ = std::fs::remove_file(path);
    }

    /// `CancelOutbox` (undo send, schedule send's "Cancel send"): a pending
    /// or failed row is withdrawn once and handed back; a row already gone
    /// (sent) can't be cancelled.
    #[test]
    fn cancel_outbox_withdraws_pending_and_failed_rows() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let now = Utc::now();

        let pending = cache.enqueue_outbox(&account_id, now + chrono::Duration::seconds(5), b"undoable").unwrap();
        let withdrawn = cache.cancel_outbox(pending).unwrap().expect("pending row withdrawn");
        assert_eq!(withdrawn.id, pending);
        assert_eq!(withdrawn.payload, b"undoable");
        assert!(cache.cancel_outbox(pending).unwrap().is_none(), "already withdrawn");
        assert_eq!(cache.next_outbox_not_before().unwrap(), None);

        let failed = cache.enqueue_outbox(&account_id, now, b"doomed").unwrap();
        assert_eq!(cache.outbox_attempt_failed(failed, now, 1).unwrap(), OutboxState::Failed);
        let withdrawn = cache.cancel_outbox(failed).unwrap().expect("failed row withdrawn for editing");
        assert_eq!(withdrawn.state, OutboxState::Failed);
        assert!(cache.all_outbox().unwrap().is_empty());

        let path = cache_dir().join(format!("{}.sqlite3", sanitize_filename(&account_id)));
        let _ = std::fs::remove_file(path);
    }

    /// Schedule send's "Send now" / "Retry": a pending or failed row becomes
    /// pending at the new time with a fresh retry budget; a vanished row
    /// reports `false`.
    #[test]
    fn reschedule_outbox_resets_state_and_due_time() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let now = Utc::now();
        let now_secs = DateTime::from_timestamp(now.timestamp(), 0).unwrap();

        let scheduled = cache.enqueue_outbox(&account_id, now + chrono::Duration::hours(3), b"later").unwrap();
        assert!(cache.due_outbox(now).unwrap().is_empty());
        assert!(cache.reschedule_outbox(scheduled, now).unwrap());
        assert_eq!(cache.due_outbox(now).unwrap().iter().map(|e| e.id).collect::<Vec<_>>(), vec![scheduled]);
        assert_eq!(cache.next_outbox_not_before().unwrap(), Some(now_secs));

        assert_eq!(cache.outbox_attempt_failed(scheduled, now, 1).unwrap(), OutboxState::Failed);
        assert!(cache.due_outbox(now).unwrap().is_empty());
        assert!(cache.reschedule_outbox(scheduled, now).unwrap());
        let retried = cache.due_outbox(now).unwrap();
        assert_eq!(retried.len(), 1);
        assert_eq!(retried[0].state, OutboxState::Pending);
        assert_eq!(retried[0].attempts, 0);

        cache.delete_outbox(scheduled).unwrap();
        assert!(!cache.reschedule_outbox(scheduled, now).unwrap());

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

    /// `PRAGMA incremental_vacuum` emits one output row per page it actually
    /// reclaims - nothing when there's nothing to reclaim, which is why the
    /// prior `conn.execute` (rusqlite errors on `execute` seeing a row) only
    /// surfaced once a real cache had freed pages to sweep. Deleting a large
    /// blob is what leaves incremental_vacuum something to do.
    #[test]
    fn run_maintenance_reclaims_freed_pages_without_erroring() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();

        // First call migrates a fresh database to incremental auto_vacuum
        // (a plain `VACUUM`, which leaves nothing behind to reclaim); only
        // once that's settled does inserting-then-deleting a large blob
        // leave the free pages that made the pre-fix `execute` choke.
        cache.run_maintenance().unwrap();
        {
            let conn = cache.conn.lock().unwrap();
            let big = vec![0u8; 200_000];
            for uid in 0..20 {
                conn.execute(
                    "INSERT INTO bodies (mailbox_id, uid, uidvalidity, data) VALUES ('m', ?1, 1, ?2)",
                    rusqlite::params![uid, big],
                )
                .unwrap();
            }
            conn.execute("DELETE FROM bodies", []).unwrap();
        }

        cache.run_maintenance().unwrap();

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
    /// opening it must wipe the envelope table (forcing full re-syncs); a
    /// pre-partial-fetch build stored raw RFC 5322 bytes in `bodies`, so a
    /// format change must wipe those too; and a pre-header build's envelopes
    /// lack importance/sensitivity/List-Id, so they are wiped to re-fetch with
    /// the selected-header block. Each wipe happens exactly once, since every
    /// sync after the migration writes the whole folder (and every body fetch
    /// writes the new format).
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

        // Simulate a version-6 database (bodies current at `BODY_CACHE_VERSION`,
        // envelopes one behind at `ENVELOPE_CACHE_VERSION`): the envelope rows
        // are wiped so the preview/mention pass re-inspects each body (M-25),
        // while the bodies themselves survive.
        cache.replace_messages(&mailbox_id, UidValidity(1), &[sample_summary(&mailbox_id, 3, None)]).unwrap();
        cache.store_body(&mailbox_id, Uid(3), UidValidity(1), &sample_body("kept body")).unwrap();
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.pragma_update(None, "user_version", 6).unwrap();
        }
        cache.store_body(&mailbox_id, Uid(3), UidValidity(1), &sample_body("kept body")).unwrap();
        let cache = Cache::open(&account_id).unwrap();
        assert!(
            cache.load_messages(&mailbox_id).unwrap().is_empty(),
            "a version-6 cache's envelope rows must be wiped for the mention pass"
        );
        assert!(
            cache.has_body(&mailbox_id, Uid(3), UidValidity(1)).unwrap(),
            "a version-6 cache's bodies must survive the envelope-only bump"
        );

        let _ = std::fs::remove_file(path);
    }

    /// The selected-header markers survive the JSON envelope round trip, so a
    /// message synced with importance/sensitivity/List-Id keeps them when the
    /// list is reloaded from the cache.
    #[test]
    fn summary_markers_round_trip_through_the_cache() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let mailbox_id = MailboxId::new(&account_id, "INBOX");

        let mut msg = sample_summary(&mailbox_id, 7, None);
        msg.importance = lookout_core::Importance::High;
        msg.sensitivity = lookout_core::MessageSensitivity::Private;
        msg.list_id = Some("announce.example.com".to_string());
        cache.replace_messages(&mailbox_id, UidValidity(1), &[msg]).unwrap();

        let loaded = cache.load_messages(&mailbox_id).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].importance, lookout_core::Importance::High);
        assert_eq!(loaded[0].sensitivity, lookout_core::MessageSensitivity::Private);
        assert_eq!(loaded[0].list_id.as_deref(), Some("announce.example.com"));

        let path = cache_dir().join(format!("{}.sqlite3", sanitize_filename(&account_id)));
        let _ = std::fs::remove_file(path);
    }

    /// The outbox lifecycle: due rows are delivered, future rows wait, a
    /// failure backs off and increments `attempts`, exceeding `max_attempts`
    /// marks the row `failed` (which `due_outbox` then ignores), and deletes
    /// take a row out of `next_outbox_not_before`.
    #[test]
    fn outbox_enqueue_due_retry_and_delete_lifecycle() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let now = Utc::now();
        let now_secs = DateTime::from_timestamp(now.timestamp(), 0).unwrap();

        let due_id = cache.enqueue_outbox(&account_id, now, b"due payload").unwrap();
        let later_id = cache.enqueue_outbox(&account_id, now + chrono::Duration::hours(1), b"later payload").unwrap();

        // Only the due row is delivered.
        let due = cache.due_outbox(now).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].id, due_id);
        assert_eq!(due[0].payload, b"due payload");
        assert_eq!(due[0].attempts, 0);
        assert_eq!(due[0].state, OutboxState::Pending);
        assert_eq!(due[0].account_id, account_id);
        assert_eq!(cache.next_outbox_not_before().unwrap(), Some(now_secs));

        // `all_outbox` is newest first.
        let all = cache.all_outbox().unwrap();
        assert_eq!(all.iter().map(|e| e.id).collect::<Vec<_>>(), vec![later_id, due_id]);

        // A first failure below the cap leaves it pending, but pushed out of
        // the due window.
        assert_eq!(cache.outbox_attempt_failed(due_id, now + chrono::Duration::minutes(1), 2).unwrap(), OutboxState::Pending);
        assert!(cache.due_outbox(now).unwrap().is_empty());
        // The next due time is now the retry, not the later row.
        assert_eq!(
            cache.next_outbox_not_before().unwrap(),
            Some(DateTime::from_timestamp((now + chrono::Duration::minutes(1)).timestamp(), 0).unwrap())
        );

        // The second failure reaches the cap and marks it failed.
        assert_eq!(cache.outbox_attempt_failed(due_id, now + chrono::Duration::minutes(2), 2).unwrap(), OutboxState::Failed);
        let failed = cache.all_outbox().unwrap();
        let failed_row = failed.iter().find(|e| e.id == due_id).unwrap();
        assert_eq!(failed_row.attempts, 2);
        assert_eq!(failed_row.state, OutboxState::Failed);
        // A failed row never becomes due again, and doesn't hold the next-due
        // time.
        assert!(cache.due_outbox(now + chrono::Duration::hours(2)).unwrap().iter().all(|e| e.id != due_id));
        assert_eq!(
            cache.next_outbox_not_before().unwrap(),
            Some(DateTime::from_timestamp((now + chrono::Duration::hours(1)).timestamp(), 0).unwrap())
        );

        // Deleting the pending row leaves no next-due time; deleting the failed
        // row removes it entirely.
        cache.delete_outbox(later_id).unwrap();
        assert_eq!(cache.next_outbox_not_before().unwrap(), None);
        cache.delete_outbox(due_id).unwrap();
        assert!(cache.all_outbox().unwrap().is_empty());

        let path = cache_dir().join(format!("{}.sqlite3", sanitize_filename(&account_id)));
        let _ = std::fs::remove_file(path);
    }

    fn with_message_id(summary: EmailSummary, id: &str) -> EmailSummary {
        EmailSummary {
            message_id: Some(format!("<{id}>")),
            ..summary
        }
    }

    fn flagged(due: Option<DateTime<Utc>>) -> FollowUpFlag {
        FollowUpFlag {
            status: FlagStatus::Flagged,
            due,
            ..Default::default()
        }
    }

    /// A local follow-up row (a folder without keyword support) shows on
    /// every read path as the same keywords the server path would store, and
    /// a tombstone masks a stale keyword.
    #[test]
    fn local_follow_ups_overlay_cache_reads_and_list_flagged_mail() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let mailbox_id = MailboxId::new(&account_id, "INBOX");
        let mut keyworded = with_message_id(sample_summary(&mailbox_id, 2, None), "two@x");
        keyworded.keywords.insert(lookout_core::FLAG_KEYWORD.to_string());
        let plain = with_message_id(sample_summary(&mailbox_id, 1, None), "one@x");
        cache.replace_messages(&mailbox_id, UidValidity(1), &[plain.clone(), keyworded.clone()]).unwrap();

        let due = lookout_core::flag_due_for_date(chrono::NaiveDate::from_ymd_opt(2026, 10, 16).unwrap());
        cache.set_local_follow_ups(std::slice::from_ref(&plain), &flagged(Some(due)), Utc::now()).unwrap();
        let loaded = cache.load_messages(&mailbox_id).unwrap();
        let one = loaded.iter().find(|m| m.uid == Uid(1)).unwrap();
        assert_eq!(one.follow_up(), Some(flagged(Some(due))));
        assert_eq!(cache.load_summaries(&mailbox_id, &[Uid(1)]).unwrap()[&Uid(1)].follow_up(), Some(flagged(Some(due))));

        let mut listed: Vec<u32> = cache.flagged_messages().unwrap().iter().map(|m| m.uid.0).collect();
        listed.sort();
        assert_eq!(listed, vec![1, 2], "both the keyword and the local flag are listed");

        // A tombstone overrides the keyword the server still shows.
        cache.set_local_follow_ups(std::slice::from_ref(&keyworded), &FollowUpFlag::default(), Utc::now()).unwrap();
        assert_eq!(cache.load_summaries(&mailbox_id, &[Uid(2)]).unwrap()[&Uid(2)].follow_up(), None);
        assert_eq!(cache.flagged_messages().unwrap().iter().map(|m| m.uid.0).collect::<Vec<_>>(), vec![1]);

        // Server summaries (not cache reads) get the same overlay.
        let mut fresh = vec![plain.clone()];
        cache.overlay_follow_ups(&mut fresh).unwrap();
        assert!(fresh[0].is_flagged_for_follow_up());

        cache.clear_local_follow_ups(&[plain, keyworded]).unwrap();
        assert_eq!(cache.flagged_messages().unwrap().iter().map(|m| m.uid.0).collect::<Vec<_>>(), vec![2]);

        let path = cache_dir().join(format!("{}.sqlite3", sanitize_filename(&account_id)));
        let _ = std::fs::remove_file(path);
    }

    /// Locally-stored categories, importance and Focused/Other overlay every
    /// read (the Exchange-IMAP path, item 5.4) and the `category:` search finds
    /// a locally-stored tag.
    #[test]
    fn local_properties_overlay_cache_reads_and_search() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let mailbox_id = MailboxId::new(&account_id, "INBOX");
        let msg = with_message_id(sample_summary(&mailbox_id, 1, None), "one@x");
        cache.replace_messages(&mailbox_id, UidValidity(1), std::slice::from_ref(&msg)).unwrap();

        let now = Utc::now();
        let one = std::slice::from_ref(&msg);
        cache.set_local_categories(one, &["work".to_string(), "red".to_string()], now).unwrap();
        cache.set_local_importance(one, lookout_core::Importance::High, now).unwrap();
        cache.set_local_inference(one, lookout_core::InferenceClassification::Other, now).unwrap();

        let loaded = cache.load_messages(&mailbox_id).unwrap();
        assert!(loaded[0].keywords.contains(&lookout_core::tag_keyword("work")));
        assert!(loaded[0].keywords.contains(&lookout_core::tag_keyword("red")));
        assert_eq!(loaded[0].importance, lookout_core::Importance::High);
        assert_eq!(loaded[0].inference_classification, lookout_core::InferenceClassification::Other);
        // `load_summaries` (the on-demand body path) overlays too.
        assert_eq!(
            cache.load_summaries(&mailbox_id, &[Uid(1)]).unwrap()[&Uid(1)].inference_classification,
            lookout_core::InferenceClassification::Other
        );
        // Server summaries get the same overlay through `overlay_follow_ups`.
        let mut fresh = vec![msg.clone()];
        cache.overlay_follow_ups(&mut fresh).unwrap();
        assert!(fresh[0].keywords.contains(&lookout_core::tag_keyword("work")));

        assert_eq!(cache.search_query(&crate::search_query::parse("category:work"), None, 50).unwrap().len(), 1);
        assert!(cache.search_query(&crate::search_query::parse("category:nope"), None, 50).unwrap().is_empty());

        // Clearing resets to the defaults and drops the tag keywords.
        cache.clear_local_categories(one).unwrap();
        cache.clear_local_importance(one).unwrap();
        cache.clear_local_inference(one).unwrap();
        let loaded = cache.load_messages(&mailbox_id).unwrap();
        assert!(!loaded[0].keywords.iter().any(|k| k.starts_with(lookout_core::TAG_KEYWORD_PREFIX)));
        assert_eq!(loaded[0].importance, lookout_core::Importance::Normal);
        assert_eq!(loaded[0].inference_classification, lookout_core::InferenceClassification::Focused);

        let path = cache_dir().join(format!("{}.sqlite3", sanitize_filename(&account_id)));
        let _ = std::fs::remove_file(path);
    }

    /// A small read looks its overlay rows up by key and a large one scans the
    /// tables (`OVERLAY_KEYED_MAX`); both must overlay exactly the same values,
    /// including the `uid:` key of a message without a Message-ID, a tombstone
    /// flag, and with rows present for keys outside the read.
    #[test]
    fn keyed_and_full_overlay_loads_agree() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let mailbox_id = MailboxId::new(&account_id, "INBOX");
        let count = OVERLAY_KEYED_MAX as u32 + 20;
        let messages: Vec<EmailSummary> = (1..=count)
            .map(|uid| {
                let summary = sample_summary(&mailbox_id, uid, None);
                if uid % 7 == 0 {
                    summary
                } else {
                    with_message_id(summary, &format!("m{uid}@x"))
                }
            })
            .collect();
        cache.replace_messages(&mailbox_id, UidValidity(1), &messages).unwrap();

        let now = Utc::now();
        let every = |n: u32| messages.iter().filter(move |m| m.uid.0 % n == 0).cloned().collect::<Vec<_>>();
        cache.set_local_follow_ups(&every(3), &flagged(None), now).unwrap();
        cache.set_local_follow_ups(&every(11), &FollowUpFlag::default(), now).unwrap();
        cache.set_local_categories(&every(4), &["work".to_string()], now).unwrap();
        cache.set_local_importance(&every(5), lookout_core::Importance::High, now).unwrap();
        cache.set_local_inference(&every(2), lookout_core::InferenceClassification::Other, now).unwrap();
        // Rows for messages this read never sees.
        let stranger = with_message_id(sample_summary(&MailboxId::new(&account_id, "Archive"), 1, None), "elsewhere@x");
        cache.set_local_importance(std::slice::from_ref(&stranger), lookout_core::Importance::Low, now).unwrap();

        // The two loaders, side by side on the same raw summaries.
        let keys: Vec<String> = messages.iter().map(follow_up_key).collect();
        let keys_json = serde_json::to_string(&keys).unwrap();
        let overlaid = |keys_json: Option<&str>| {
            let conn = cache.conn.lock().unwrap();
            let overlays = LocalOverlays::load(&conn, keys_json).unwrap();
            let mut out = messages.clone();
            for (summary, key) in out.iter_mut().zip(&keys) {
                overlays.apply(key, summary);
            }
            out
        };
        // `EmailSummary` has no `PartialEq`; its sets are ordered, so its JSON
        // compares deterministically.
        let json = |summaries: &[EmailSummary]| summaries.iter().map(|m| serde_json::to_value(m).unwrap()).collect::<Vec<_>>();
        let full = overlaid(None);
        assert_eq!(json(&overlaid(Some(&keys_json))), json(&full));
        assert_ne!(json(&full), json(&messages), "the fixture overlays something");

        // And through the public reads: `load_messages` takes the full path,
        // a few-uid `load_summaries` the keyed one.
        let loaded = cache.load_messages(&mailbox_id).unwrap();
        assert!(loaded.len() > OVERLAY_KEYED_MAX);
        let wanted = [Uid(7), Uid(12), Uid(33), Uid(55), Uid(60), Uid(77)];
        let few = cache.load_summaries(&mailbox_id, &wanted).unwrap();
        assert_eq!(few.len(), wanted.len());
        for uid in wanted {
            let listed = loaded.iter().find(|m| m.uid == uid).unwrap();
            let expected = full.iter().find(|m| m.uid == uid).unwrap();
            assert_eq!(json(std::slice::from_ref(&few[&uid])), json(std::slice::from_ref(expected)), "uid {}", uid.0);
            assert_eq!(json(std::slice::from_ref(listed)), json(std::slice::from_ref(expected)), "uid {}", uid.0);
        }

        let path = cache_dir().join(format!("{}.sqlite3", sanitize_filename(&account_id)));
        let _ = std::fs::remove_file(path);
    }

    /// The keyed overlay lookup matches on the normalised key: a bracketed
    /// Message-ID resolves to its bare row, and a message without one to its
    /// `uid:` row.
    #[test]
    fn load_summaries_overlays_with_bracketed_and_missing_message_ids() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let mailbox_id = MailboxId::new(&account_id, "INBOX");
        let bracketed = with_message_id(sample_summary(&mailbox_id, 1, None), "one@x");
        let bare = sample_summary(&mailbox_id, 2, None);
        assert!(bare.message_id.is_none());
        cache.replace_messages(&mailbox_id, UidValidity(1), &[bracketed.clone(), bare.clone()]).unwrap();
        cache.set_local_importance(&[bracketed, bare], lookout_core::Importance::High, Utc::now()).unwrap();

        let loaded = cache.load_summaries(&mailbox_id, &[Uid(1), Uid(2)]).unwrap();
        assert_eq!(loaded[&Uid(1)].importance, lookout_core::Importance::High);
        assert_eq!(loaded[&Uid(2)].importance, lookout_core::Importance::High);

        let path = cache_dir().join(format!("{}.sqlite3", sanitize_filename(&account_id)));
        let _ = std::fs::remove_file(path);
    }

    /// A delta page records the Graph-id map, the non-default properties and
    /// the cursor, and leaves a newer local change alone.
    #[test]
    fn apply_remote_mail_properties_upserts_non_defaults_and_respects_taken_at() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let mailbox_id = MailboxId::new(&account_id, "INBOX");
        let msg = with_message_id(sample_summary(&mailbox_id, 1, None), "one@x");
        cache.replace_messages(&mailbox_id, UidValidity(1), std::slice::from_ref(&msg)).unwrap();

        let taken_at = Utc::now();
        let due = lookout_core::flag_due_for_date(chrono::NaiveDate::from_ymd_opt(2026, 10, 16).unwrap());
        let props = RemoteMailProperties {
            message_id: "<one@x>".to_string(),
            graph_id: "g1".to_string(),
            categories: vec!["work".to_string()],
            importance: lookout_core::Importance::High,
            classification: lookout_core::InferenceClassification::Other,
            flag: Some(flagged(Some(due))),
        };
        cache.apply_remote_mail_properties(std::slice::from_ref(&props), &[], Some("cursor-1"), taken_at).unwrap();

        assert_eq!(cache.graph_id("one@x").unwrap().as_deref(), Some("g1"));
        assert_eq!(cache.graph_ids(&["one@x".to_string()]).unwrap().get("one@x").map(String::as_str), Some("g1"));
        assert_eq!(cache.graph_delta_cursor().unwrap().as_deref(), Some("cursor-1"));
        let loaded = cache.load_messages(&mailbox_id).unwrap();
        assert!(loaded[0].keywords.contains(&lookout_core::tag_keyword("work")));
        assert_eq!(loaded[0].importance, lookout_core::Importance::High);
        assert_eq!(loaded[0].inference_classification, lookout_core::InferenceClassification::Other);
        assert_eq!(loaded[0].follow_up(), Some(flagged(Some(due))));

        // A local change made *after* the snapshot survives an older one.
        cache.set_local_importance(std::slice::from_ref(&msg), lookout_core::Importance::Low, Utc::now()).unwrap();
        let mut stale = props;
        stale.categories = Vec::new();
        stale.importance = lookout_core::Importance::High;
        stale.classification = lookout_core::InferenceClassification::Focused;
        stale.flag = None;
        cache.apply_remote_mail_properties(&[stale], &[], None, taken_at - chrono::Duration::seconds(60)).unwrap();
        let loaded = cache.load_messages(&mailbox_id).unwrap();
        assert_eq!(loaded[0].importance, lookout_core::Importance::Low, "a newer local change is not clobbered");
        assert!(loaded[0].keywords.contains(&lookout_core::tag_keyword("work")), "the earlier category survives too");

        let path = cache_dir().join(format!("{}.sqlite3", sanitize_filename(&account_id)));
        let _ = std::fs::remove_file(path);
    }

    /// A removed delta entry drops the message's Graph id and every overlay row
    /// so a deleted/moved-out message leaves nothing behind.
    #[test]
    fn apply_remote_mail_properties_drops_removed_graph_ids() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let mailbox_id = MailboxId::new(&account_id, "INBOX");
        let msg = with_message_id(sample_summary(&mailbox_id, 1, None), "one@x");
        cache.replace_messages(&mailbox_id, UidValidity(1), std::slice::from_ref(&msg)).unwrap();
        cache
            .apply_remote_mail_properties(
                &[RemoteMailProperties {
                    message_id: "<one@x>".to_string(),
                    graph_id: "g1".to_string(),
                    categories: vec!["work".to_string()],
                    importance: lookout_core::Importance::High,
                    classification: lookout_core::InferenceClassification::Other,
                    flag: None,
                }],
                &[],
                None,
                Utc::now(),
            )
            .unwrap();

        cache.apply_remote_mail_properties(&[], &["g1".to_string()], None, Utc::now()).unwrap();
        assert_eq!(cache.graph_id("one@x").unwrap(), None);
        let loaded = cache.load_messages(&mailbox_id).unwrap();
        assert!(!loaded[0].keywords.iter().any(|k| k.starts_with(lookout_core::TAG_KEYWORD_PREFIX)));
        assert_eq!(loaded[0].importance, lookout_core::Importance::Normal);
        assert_eq!(loaded[0].inference_classification, lookout_core::InferenceClassification::Focused);

        let path = cache_dir().join(format!("{}.sqlite3", sanitize_filename(&account_id)));
        let _ = std::fs::remove_file(path);
    }

    /// The Focused/Other sender overrides (M-03) round-trip, upsert by sender
    /// and clear.
    #[test]
    fn inference_overrides_round_trip() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        assert!(cache.inference_overrides().unwrap().is_empty());

        cache
            .set_inference_override("sam@contoso.com", lookout_core::InferenceClassification::Other, Some("o1"), Utc::now())
            .unwrap();
        let rows = cache.inference_overrides().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].sender, "sam@contoso.com");
        assert_eq!(rows[0].classify_as, lookout_core::InferenceClassification::Other);
        assert_eq!(rows[0].override_id.as_deref(), Some("o1"));

        // Re-setting the same sender replaces its row.
        cache
            .set_inference_override("sam@contoso.com", lookout_core::InferenceClassification::Focused, None, Utc::now())
            .unwrap();
        let rows = cache.inference_overrides().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].classify_as, lookout_core::InferenceClassification::Focused);
        assert_eq!(rows[0].override_id, None);

        cache.clear_inference_override("sam@contoso.com").unwrap();
        assert!(cache.inference_overrides().unwrap().is_empty());

        let path = cache_dir().join(format!("{}.sqlite3", sanitize_filename(&account_id)));
        let _ = std::fs::remove_file(path);
    }

    /// A Graph override snapshot replaces older rows, but keeps one set here
    /// after it was taken (M-03).
    #[test]
    fn apply_inference_overrides_replaces_older_rows_only() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let taken_at = Utc::now();
        // Set here after the snapshot was taken: must survive.
        cache
            .set_inference_override("local@x", lookout_core::InferenceClassification::Other, None, taken_at + chrono::Duration::seconds(5))
            .unwrap();
        cache
            .apply_inference_overrides(
                &[
                    ("outlook@x".to_string(), lookout_core::InferenceClassification::Focused, Some("o1".to_string())),
                    ("local@x".to_string(), lookout_core::InferenceClassification::Focused, Some("o2".to_string())),
                ],
                taken_at,
            )
            .unwrap();

        let mut rows = cache.inference_overrides().unwrap();
        rows.sort_by(|a, b| a.sender.cmp(&b.sender));
        assert_eq!(rows.len(), 2);
        // The snapshot's own override is stored with its Graph id.
        assert_eq!(rows[0].sender, "local@x");
        assert_eq!(rows[0].classify_as, lookout_core::InferenceClassification::Other, "the newer local choice wins");
        assert_eq!(rows[0].override_id, None);
        assert_eq!(rows[1].sender, "outlook@x");
        assert_eq!(rows[1].classify_as, lookout_core::InferenceClassification::Focused);
        assert_eq!(rows[1].override_id.as_deref(), Some("o1"));

        let path = cache_dir().join(format!("{}.sqlite3", sanitize_filename(&account_id)));
        let _ = std::fs::remove_file(path);
    }

    /// The delta form of a local category write adjusts each message's
    /// effective tag set and clears the row when nothing remains.
    #[test]
    fn update_local_categories_applies_a_delta_and_clears() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let mailbox_id = MailboxId::new(&account_id, "INBOX");
        let msg = with_message_id(sample_summary(&mailbox_id, 1, None), "one@x");
        cache.replace_messages(&mailbox_id, UidValidity(1), std::slice::from_ref(&msg)).unwrap();

        cache
            .update_local_categories(std::slice::from_ref(&msg), &["work".to_string(), "red".to_string()], &[], Utc::now())
            .unwrap();
        let loaded = cache.load_messages(&mailbox_id).unwrap();
        assert!(loaded[0].keywords.contains(&lookout_core::tag_keyword("work")));
        assert!(loaded[0].keywords.contains(&lookout_core::tag_keyword("red")));

        // Removing one key keeps the other (adjusting the overlaid set).
        cache
            .update_local_categories(std::slice::from_ref(&loaded[0]), &[], &["red".to_string()], Utc::now())
            .unwrap();
        let loaded = cache.load_messages(&mailbox_id).unwrap();
        assert!(loaded[0].keywords.contains(&lookout_core::tag_keyword("work")));
        assert!(!loaded[0].keywords.contains(&lookout_core::tag_keyword("red")));

        // Removing the last key clears the row entirely.
        cache
            .update_local_categories(std::slice::from_ref(&loaded[0]), &[], &["work".to_string()], Utc::now())
            .unwrap();
        let loaded = cache.load_messages(&mailbox_id).unwrap();
        assert!(!loaded[0].keywords.iter().any(|k| k.starts_with(lookout_core::TAG_KEYWORD_PREFIX)));

        let path = cache_dir().join(format!("{}.sqlite3", sanitize_filename(&account_id)));
        let _ = std::fs::remove_file(path);
    }

    /// A conversation's Sent Items members are found by Message-ID,
    /// In-Reply-To or References, bracketed or not; unrelated mail and other
    /// folders stay out, and the prefetch's partial rows count.
    #[test]
    fn conversation_members_match_by_id_reply_or_references() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let sent = MailboxId::new(&account_id, "Sent");
        let inbox = MailboxId::new(&account_id, "INBOX");
        let same = with_message_id(sample_summary(&sent, 1, None), "<root@x>");
        let mut reply = with_message_id(sample_summary(&sent, 2, None), "<reply@x>");
        reply.in_reply_to = Some("<root@x>".into());
        let mut deep = with_message_id(sample_summary(&sent, 3, None), "<deep@x>");
        deep.references = vec!["<other@x>".into(), "<mid@x>".into()];
        let unrelated = with_message_id(sample_summary(&sent, 4, None), "<elsewhere@x>");
        cache.replace_messages(&sent, UidValidity(1), &[same, reply, deep, unrelated]).unwrap();
        // A Sent folder never opened in full: only the prefetch's window.
        let sent_window = MailboxId::new(&account_id, "Sent Window");
        let mut partial = with_message_id(sample_summary(&sent_window, 5, None), "<late@x>");
        partial.in_reply_to = Some("mid@x".into());
        cache.store_partial_messages(&sent_window, UidValidity(1), &[partial]).unwrap();
        let mut in_inbox = with_message_id(sample_summary(&inbox, 6, None), "<inbox@x>");
        in_inbox.in_reply_to = Some("<root@x>".into());
        cache.replace_messages(&inbox, UidValidity(1), &[in_inbox]).unwrap();

        let mut uids: Vec<u32> = cache
            .conversation_members_in(&sent, &["root@x".to_string(), "<mid@x>".to_string()])
            .unwrap()
            .iter()
            .map(|m| m.uid.0)
            .collect();
        uids.sort();
        assert_eq!(uids, vec![1, 2, 3]);
        assert_eq!(
            cache
                .conversation_members_in(&sent_window, &["<mid@x>".to_string()])
                .unwrap()
                .iter()
                .map(|m| m.uid.0)
                .collect::<Vec<_>>(),
            vec![5]
        );
        assert!(cache.conversation_members_in(&sent, &[]).unwrap().is_empty());
        assert!(cache.conversation_members_in(&sent, &["<>".to_string()]).unwrap().is_empty());

        let path = cache_dir().join(format!("{}.sqlite3", sanitize_filename(&account_id)));
        let _ = std::fs::remove_file(path);
    }

    fn sample_summary(mailbox_id: &MailboxId, uid: u32, preview: Option<&str>) -> EmailSummary {
        EmailSummary {
            inference_classification: Default::default(),
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
            mentions: false,
            importance: Default::default(),
            sensitivity: Default::default(),
            list_id: None,
            structure: None,
        }
    }

    fn titled(mailbox_id: &MailboxId, uid: u32, subject: &str) -> EmailSummary {
        EmailSummary {
            subject: Some(subject.to_string()),
            ..sample_summary(mailbox_id, uid, None)
        }
    }

    fn search_uids(cache: &Cache, query: &str) -> Vec<u32> {
        let mut uids: Vec<u32> = cache.search(query, 50).unwrap().iter().map(|m| m.uid.0).collect();
        uids.sort();
        uids
    }

    #[test]
    fn a_partial_window_is_searchable_but_never_served_as_the_folder() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let mailbox_id = MailboxId::new(&account_id, "Projects");
        let window = [titled(&mailbox_id, 1, "quarterly zeppelin"), titled(&mailbox_id, 2, "zeppelin hangar")];

        cache.store_partial_messages(&mailbox_id, UidValidity(1), &window).unwrap();
        assert_eq!(search_uids(&cache, "zeppelin"), vec![1, 2]);
        assert!(!cache.has_messages(&mailbox_id).unwrap(), "a partial window must not cache-hit the folder's sync");
        assert!(cache.load_messages(&mailbox_id).unwrap().is_empty(), "a partial window must not paint as the folder");
        assert_eq!(
            cache.load_messages_by_uid(&mailbox_id, UidValidity(1)).unwrap().len(),
            2,
            "the first full sync reuses the window"
        );

        // The first full sync promotes even byte-identical rows to complete.
        let full = [window[0].clone(), window[1].clone(), titled(&mailbox_id, 3, "older zeppelin")];
        cache.replace_messages(&mailbox_id, UidValidity(1), &full).unwrap();
        assert!(cache.has_messages(&mailbox_id).unwrap());
        assert_eq!(cache.load_messages(&mailbox_id).unwrap().len(), 3);
        assert_eq!(search_uids(&cache, "zeppelin"), vec![1, 2, 3]);

        // Once the folder has a full snapshot, a later prefetch pass leaves it alone.
        cache.store_partial_messages(&mailbox_id, UidValidity(1), &[titled(&mailbox_id, 4, "zeppelin")]).unwrap();
        assert_eq!(cache.load_messages(&mailbox_id).unwrap().len(), 3);
        assert_eq!(search_uids(&cache, "zeppelin"), vec![1, 2, 3]);

        let path = cache_dir().join(format!("{}.sqlite3", sanitize_filename(&account_id)));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_partial_window_rerun_drops_rows_that_left_it_but_keeps_snoozed_ones() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let mailbox_id = MailboxId::new(&account_id, "Projects");
        let first = [1, 2, 3].map(|uid| titled(&mailbox_id, uid, "zeppelin"));
        cache.store_partial_messages(&mailbox_id, UidValidity(1), &first).unwrap();
        cache.snooze_message(&mailbox_id, Uid(1), Utc::now() + chrono::Duration::hours(1)).unwrap();
        cache.store_body(&mailbox_id, Uid(2), UidValidity(1), &sample_body("hangar")).unwrap();

        let second = [3, 4].map(|uid| titled(&mailbox_id, uid, "zeppelin"));
        cache.store_partial_messages(&mailbox_id, UidValidity(1), &second).unwrap();

        let mut kept: Vec<u32> = cache.load_messages_by_uid(&mailbox_id, UidValidity(1)).unwrap().keys().map(|u| u.0).collect();
        kept.sort();
        assert_eq!(kept, vec![1, 3, 4], "uid 2 left the window; the snoozed uid 1 stays");
        assert!(!cache.has_body(&mailbox_id, Uid(2), UidValidity(1)).unwrap());
        assert_eq!(search_uids(&cache, "zeppelin"), vec![3, 4], "the snoozed row stays hidden from search");
        assert!(search_uids(&cache, "hangar").is_empty(), "a dropped row's index text goes with it");

        let path = cache_dir().join(format!("{}.sqlite3", sanitize_filename(&account_id)));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_partial_window_indexes_bodies_cached_before_and_after_it() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let mailbox_id = MailboxId::new(&account_id, "Projects");
        // An earlier prefetch pass cached this body with no envelope to index against.
        cache.store_body(&mailbox_id, Uid(1), UidValidity(1), &sample_body("the quokka enclosure")).unwrap();

        cache
            .store_partial_messages(&mailbox_id, UidValidity(1), &[titled(&mailbox_id, 1, "zoo"), titled(&mailbox_id, 2, "zoo")])
            .unwrap();
        assert_eq!(search_uids(&cache, "quokka"), vec![1]);
        // A body fetched after the window was indexed joins it the usual way.
        cache.store_bodies(&mailbox_id, UidValidity(1), &[(Uid(2), sample_body("wombat burrow"))]).unwrap();
        assert_eq!(search_uids(&cache, "wombat"), vec![2]);

        let rows = cache.load_messages_by_uid(&mailbox_id, UidValidity(1)).unwrap();
        assert_eq!(rows[&Uid(1)].preview.as_deref(), Some("the quokka enclosure"), "a search hit shows a snippet");
        assert_eq!(rows[&Uid(2)].preview.as_deref(), Some("wombat burrow"));

        // A re-run's fresh envelope (no preview) keeps the derived snippet.
        cache
            .store_partial_messages(&mailbox_id, UidValidity(1), &[titled(&mailbox_id, 1, "zoo"), titled(&mailbox_id, 2, "zoo")])
            .unwrap();
        let rows = cache.load_messages_by_uid(&mailbox_id, UidValidity(1)).unwrap();
        assert_eq!(rows[&Uid(1)].preview.as_deref(), Some("the quokka enclosure"));
        assert_eq!(search_uids(&cache, "quokka"), vec![1], "the full-text row survives the re-run");

        let path = cache_dir().join(format!("{}.sqlite3", sanitize_filename(&account_id)));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn forgetting_a_folder_tree_spares_siblings_and_keeps_rekeyed_snoozes() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let work = MailboxId::new(&account_id, "Work");
        let work_child = MailboxId::new(&account_id, "Work/2026");
        let work2 = MailboxId::new(&account_id, "Work2");
        for id in [&work, &work_child, &work2] {
            cache.replace_messages(id, UidValidity(1), &[sample_summary(id, 1, None)]).unwrap();
            cache.store_body(id, Uid(1), UidValidity(1), &sample_body("hello")).unwrap();
        }
        let until = Utc::now() + chrono::Duration::days(1);
        cache.snooze_message(&work_child, Uid(1), until).unwrap();

        let renamed = MailboxId::new(&account_id, "Archive/Work");
        cache.rekey_snoozed(&work, &renamed, '/').unwrap();
        cache.forget_mailbox_tree(&work, '/').unwrap();

        assert!(!cache.has_messages(&work).unwrap());
        assert!(!cache.has_messages(&work_child).unwrap());
        assert!(!cache.has_body(&work_child, Uid(1), UidValidity(1)).unwrap());
        assert!(cache.has_messages(&work2).unwrap(), "a sibling sharing the prefix is untouched");
        assert!(cache.has_body(&work2, Uid(1), UidValidity(1)).unwrap());
        let moved = MailboxId::new(&account_id, "Archive/Work/2026");
        assert!(cache.active_snoozed_uids(&moved, Utc::now()).unwrap().contains(&Uid(1)), "the snooze followed the rename");
        assert!(cache.active_snoozed_uids(&work_child, Utc::now()).unwrap().is_empty());
        let _ = remove_account_cache(&account_id);
    }

    #[test]
    fn mark_mailbox_seen_flags_every_cached_message() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let inbox = MailboxId::new(&account_id, "INBOX");
        let messages: Vec<EmailSummary> = (1..=3).map(|uid| sample_summary(&inbox, uid, None)).collect();
        cache.replace_messages(&inbox, UidValidity(1), &messages).unwrap();
        cache.mark_mailbox_seen(&inbox).unwrap();
        assert!(cache.load_messages(&inbox).unwrap().iter().all(|m| m.flags.contains(&SystemFlagBit::Seen)));
        let _ = remove_account_cache(&account_id);
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
        assert!(cache.update_previews(&mailbox_id, &previews, &HashMap::new()).unwrap());

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
    fn update_previews_patches_mentions_without_a_preview() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let mailbox_id = MailboxId::new(&account_id, "INBOX");

        let messages = vec![sample_summary(&mailbox_id, 1, Some("kept")), sample_summary(&mailbox_id, 2, Some("kept"))];
        cache.replace_messages(&mailbox_id, UidValidity(1), &messages).unwrap();

        // A mention-only patch (the on-demand body-fetch path): the preview map
        // is empty, so `update_previews` must still walk the mention uid.
        let mentions = HashMap::from([(Uid(1), true)]);
        assert!(cache.update_previews(&mailbox_id, &HashMap::new(), &mentions).unwrap());

        let reloaded = cache.load_messages_by_uid(&mailbox_id, UidValidity(1)).unwrap();
        assert!(reloaded[&Uid(1)].mentions);
        assert!(!reloaded[&Uid(2)].mentions);
        // The preview is untouched by a mention-only patch.
        assert_eq!(reloaded[&Uid(1)].preview.as_deref(), Some("kept"));

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
        assert!(!cache.update_previews(&mailbox_id, &previews, &HashMap::new()).unwrap());

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

    /// `SearchQuery` operators compile to the right SQL predicates: free text
    /// and `from:`/`subject:` go through FTS, the rest through `json_extract`,
    /// and `mailbox_scope` narrows the folders.
    #[test]
    fn search_query_filters_by_operators_and_scope() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let inbox = MailboxId::new(&account_id, "INBOX");
        let archive = MailboxId::new(&account_id, "Archive");

        let mut a = searchable_summary(&inbox, 1, "Quarterly report", "ada@example.com", Some("The numbers are attached"));
        a.to = vec![lookout_core::EmailAddress::new("team@example.com")];
        a.date = "2026-06-15T10:00:00Z".parse().unwrap();
        a.keywords = std::collections::BTreeSet::from([lookout_core::tag_keyword("work")]);

        let mut b = searchable_summary(&inbox, 2, "Big file", "bob@elsewhere.org", Some("Pizza at noon"));
        b.flags = std::collections::BTreeSet::from([SystemFlagBit::Seen]);
        b.has_attachment = true;
        b.size = 2 * 1024 * 1024;
        b.date = "2026-01-10T10:00:00Z".parse().unwrap();

        let mut c = searchable_summary(&archive, 9, "Archived quarterly", "carol@example.com", None);
        c.date = "2025-01-01T10:00:00Z".parse().unwrap();

        cache.replace_messages(&inbox, UidValidity(1), &[a, b]).unwrap();
        cache.replace_messages(&archive, UidValidity(1), &[c]).unwrap();

        let query = |text: &str| crate::search_query::parse(text);
        let uids = |text: &str, scope: Option<&[MailboxId]>| {
            let mut found: Vec<u32> = cache.search_query(&query(text), scope, 50).unwrap().iter().map(|m| m.uid.0).collect();
            found.sort();
            found
        };
        let all: Vec<MailboxId> = vec![inbox.clone(), archive.clone()];

        assert_eq!(uids("from:ada", Some(&all)), vec![1]);
        assert_eq!(uids("subject:report", Some(&all)), vec![1]);
        assert_eq!(uids("to:team", Some(&all)), vec![1]);
        assert_eq!(uids("is:unread", Some(&all)), vec![1, 9]);
        assert_eq!(uids("has:attachment", Some(&all)), vec![2]);
        assert_eq!(uids("size:>1MB", Some(&all)), vec![2]);
        assert_eq!(uids("category:work", Some(&all)), vec![1]);
        assert_eq!(uids("received:>=2026-06-01", Some(&all)), vec![1]);
        // A filter-only query with no text component still works.
        assert_eq!(uids("is:unread has:attachment", Some(&all)), Vec::<u32>::new());
        // Scope restricts the sweep.
        assert_eq!(uids("quarterly", Some(std::slice::from_ref(&archive))), vec![9]);
        assert_eq!(uids("quarterly", Some(std::slice::from_ref(&inbox))), vec![1]);
        // An empty scope is an empty result, not "every folder".
        assert!(uids("quarterly", Some(&[])).is_empty());

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
        cache.replace_messages(&mailbox_id, UidValidity(1), std::slice::from_ref(&msg)).unwrap();
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
        cache.replace_messages(&mailbox_id, UidValidity(1), std::slice::from_ref(&msg)).unwrap();
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
        // exactly what a pre-search build left on disk (user_version current:
        // the envelope/body-format and header migrations done, the FTS index
        // absent). A comfortably-high version marks it current without having
        // to track `ENVELOPE_CACHE_VERSION`/`BODY_CACHE_VERSION` here.
        let path = cache_dir().join(format!("{}.sqlite3", sanitize_filename(&account_id)));
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "
                PRAGMA user_version = 100;
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

    // --- The Graph mail transport's identity map (item 5.7.3) ---

    #[test]
    fn graph_uids_are_stable_ordered_and_survive_a_reopen() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();

        let first = cache.graph_uids(&["g-old", "g-new"]).unwrap();
        assert_eq!(first, vec![Uid(1), Uid(2)], "handed out from 1, in the order given");
        // Known ids keep their Uid; a copy (a new Graph id) gets the next one.
        assert_eq!(cache.graph_uids(&["g-new", "g-copy", "g-old"]).unwrap(), vec![Uid(2), Uid(3), Uid(1)]);

        let validity = cache.graph_uid_validity().unwrap();
        assert_ne!(validity.0, 0);
        let reopened = Cache::open(&account_id).unwrap();
        assert_eq!(reopened.graph_uid_validity().unwrap(), validity);
        assert_eq!(reopened.graph_uids(&["g-copy", "g-later"]).unwrap(), vec![Uid(3), Uid(4)]);

        let ids = reopened.graph_ids_for(&[Uid(1), Uid(4), Uid(99)]).unwrap();
        assert_eq!(ids.get(&Uid(1)).map(String::as_str), Some("g-old"));
        assert_eq!(ids.get(&Uid(4)).map(String::as_str), Some("g-later"));
        assert!(!ids.contains_key(&Uid(99)));
        let _ = remove_account_cache(&account_id);
    }

    #[test]
    fn graph_uids_existing_never_allocates_for_an_unknown_id() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let uids = cache.graph_uids(&["known"]).unwrap();
        let existing = cache.graph_uids_existing(&["known".to_string(), "never-seen".to_string()]).unwrap();
        assert_eq!(existing.get("known"), Some(&uids[0]));
        assert!(!existing.contains_key("never-seen"));
        // The lookup didn't touch the allocator: the next new id gets Uid 2.
        assert_eq!(cache.graph_uids(&["next"]).unwrap(), vec![Uid(2)]);
        let _ = remove_account_cache(&account_id);
    }

    #[test]
    fn a_lost_graph_uid_map_starts_over_under_a_new_validity() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        cache.graph_uids(&["g1", "g2"]).unwrap();
        let validity = cache.graph_uid_validity().unwrap();
        drop(cache);

        remove_account_cache(&account_id).unwrap();
        let cache = Cache::open(&account_id).unwrap();
        assert_ne!(cache.graph_uid_validity().unwrap(), validity, "a recreated map must not reuse the old validity");
        assert_eq!(cache.graph_uids(&["g2"]).unwrap(), vec![Uid(1)]);
        let _ = remove_account_cache(&account_id);
    }

    #[test]
    fn an_envelope_wipe_keeps_the_graph_uid_map() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        cache.graph_uids(&["g1", "g2"]).unwrap();
        let validity = cache.graph_uid_validity().unwrap();
        let path = cache_dir().join(format!("{}.sqlite3", sanitize_filename(&account_id)));
        drop(cache);
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.pragma_update(None, "user_version", 0).unwrap();
        }
        let cache = Cache::open(&account_id).unwrap();
        assert_eq!(cache.graph_uid_validity().unwrap(), validity);
        assert_eq!(cache.graph_uids(&["g2", "g1"]).unwrap(), vec![Uid(2), Uid(1)]);
        let _ = remove_account_cache(&account_id);
    }

    #[test]
    fn pruning_drops_only_graph_uids_no_message_uses() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let mailbox_id = MailboxId::new(&account_id, "AAMkAGINBOX=");
        let uids = cache.graph_uids(&["kept", "gone"]).unwrap();
        cache
            .replace_messages(&mailbox_id, cache.graph_uid_validity().unwrap(), &[sample_summary(&mailbox_id, uids[0].0, None)])
            .unwrap();

        assert_eq!(cache.prune_graph_uids().unwrap(), 1);
        let ids = cache.graph_ids_for(&uids).unwrap();
        assert_eq!(ids.get(&uids[0]).map(String::as_str), Some("kept"));
        assert!(!ids.contains_key(&uids[1]));
        // Uids are never reused, even after a prune.
        assert_eq!(cache.graph_uids(&["gone"]).unwrap(), vec![Uid(3)]);
        cache.run_maintenance().unwrap();
        let _ = remove_account_cache(&account_id);
    }

    #[test]
    fn two_handles_never_hand_out_the_same_graph_uid() {
        let account_id = temp_account_id();
        let a = std::sync::Arc::new(Cache::open(&account_id).unwrap());
        let b = std::sync::Arc::new(Cache::open(&account_id).unwrap());
        let handles: Vec<_> = [a, b]
            .into_iter()
            .enumerate()
            .map(|(n, cache)| {
                std::thread::spawn(move || {
                    let ids: Vec<String> = (0..50).map(|i| format!("h{n}-{i}")).collect();
                    let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
                    let mut uids = Vec::new();
                    for chunk in refs.chunks(5) {
                        uids.extend(cache.graph_uids(chunk).unwrap());
                    }
                    uids
                })
            })
            .collect();
        let all: Vec<Uid> = handles.into_iter().flat_map(|handle| handle.join().unwrap()).collect();
        let distinct: HashSet<Uid> = all.iter().copied().collect();
        assert_eq!(all.len(), 100);
        assert_eq!(distinct.len(), 100, "every Graph id got its own Uid");
        let _ = remove_account_cache(&account_id);
    }

    #[test]
    fn a_graph_part_number_stays_inside_the_attachments_dir() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let mailbox_id = MailboxId::new(&account_id, "AAMkAGINBOX=");
        // `lookout_graph::graph_part_number("AA+k/ATT3=")`.
        let part = "AA-k_ATT3=";
        cache.store_attachment(&mailbox_id, Uid(1), UidValidity(9), part, b"%PDF").unwrap();
        assert_eq!(
            cache.load_attachment(&mailbox_id, Uid(1), UidValidity(9), part).unwrap().as_deref(),
            Some(b"%PDF".as_slice())
        );
        let path = cache.attachment_path(&mailbox_id, Uid(1), UidValidity(9), part);
        assert_eq!(path.parent(), Some(cache.attachments_dir.as_path()));
        let _ = remove_account_cache(&account_id);
    }

    #[test]
    fn upsert_messages_changes_only_the_rows_it_is_given() {
        let account_id = temp_account_id();
        let cache = Cache::open(&account_id).unwrap();
        let mailbox_id = MailboxId::new(&account_id, "AAMkAGINBOX=");
        let validity = UidValidity(5);
        cache
            .replace_messages(&mailbox_id, validity, &[sample_summary(&mailbox_id, 1, None), sample_summary(&mailbox_id, 2, None)])
            .unwrap();
        cache.store_body(&mailbox_id, Uid(2), validity, &sample_body("zebracrossing in the body")).unwrap();

        // A delta naming only uid 2 (changed) and uid 3 (new).
        let mut changed = sample_summary(&mailbox_id, 2, None);
        changed.subject = Some("renamed".to_string());
        cache.upsert_messages(&mailbox_id, validity, &[changed, sample_summary(&mailbox_id, 3, None)]).unwrap();

        let uids: Vec<u32> = cache.load_messages(&mailbox_id).unwrap().iter().map(|m| m.uid.0).collect();
        assert_eq!(uids.len(), 3, "uid 1 is untouched, not deleted: {uids:?}");
        assert_eq!(cache.load_summary(&mailbox_id, Uid(2)).unwrap().unwrap().subject.as_deref(), Some("renamed"));
        assert_eq!(cache.search("zebracrossing", 10).unwrap().len(), 1, "the full-body index text survives the envelope change");
        assert_eq!(cache.search("renamed", 10).unwrap().len(), 1);

        // A partial prefetch row is promoted to a complete one.
        let other = MailboxId::new(&account_id, "AAMkAGOTHER=");
        cache.store_partial_messages(&other, validity, &[sample_summary(&other, 9, None)]).unwrap();
        assert!(!cache.has_messages(&other).unwrap());
        cache.upsert_messages(&other, validity, &[sample_summary(&other, 9, None)]).unwrap();
        assert!(cache.has_messages(&other).unwrap());
        cache.upsert_messages(&other, validity, &[]).unwrap();
        let _ = remove_account_cache(&account_id);
    }
}
