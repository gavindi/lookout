/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::sync::Mutex;

use chrono::{Datelike, NaiveDate};
use lookout_core::{AccountId, CalendarTask, EventOccurrence, VCard};
use rusqlite::Connection;

use crate::{ContactRecord, Result};

/// A per-account local SQLite cache of expanded calendar occurrences, keyed by
/// month - the CalDAV mirror of `lookout_mail::cache::Cache`. Used for a fast
/// paint when a month is re-shown (navigating back to a previously-viewed
/// month, or a cold start after events for it were fetched in an earlier
/// session) before the live REPORT round-trip completes. Never the source of
/// truth: every entry is superseded by the next `OccurrencesUpdated` event
/// from the live session, exactly like the mail cache.
///
/// The connection is `Mutex`-wrapped purely to make `CalendarCache: Sync` (and
/// so `&CalendarCache: Send`) - `rusqlite::Connection` itself isn't `Sync`
/// because its internal statement cache uses a `RefCell`, which otherwise
/// poisons the `Send`-ness of the whole `run_calendar_session` future wherever
/// a `&CalendarCache` is held across an `.await` point. There's never actual
/// cross-thread contention (one account session owns its cache), so the lock
/// is uncontended in practice.
pub struct CalendarCache {
    conn: Mutex<Connection>,
}

fn cache_dir() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("lookout").join("calendar")
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
/// `$XDG_CACHE_HOME/lookout/calendar/` so the next sync starts fresh from the
/// server. Safe to call while account sessions are live: each session keeps
/// its own already-open connection (POSIX unlink doesn't disturb an open fd),
/// and the cache is only a fast-paint hint anyway - so only the on-disk files
/// are dropped, and the in-memory/live data keeps working as-is.
pub fn clear_all_caches() -> Result<()> {
    let dir = cache_dir();
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    let contacts_dir = contacts_cache_dir();
    if contacts_dir.exists() {
        std::fs::remove_dir_all(&contacts_dir)?;
    }
    Ok(())
}

/// Removes one subscription's cache database (on unsubscribe). A missing file
/// is not an error - the feed may never have synced. Same "safe while the
/// session is live" reasoning as [`clear_all_caches`].
pub fn remove_subscription_cache(subscription_id: &str) -> Result<()> {
    let path = cache_dir().join(format!("{}.sqlite3", sanitize_filename(&AccountId(format!("webcal-{subscription_id}")))));
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
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

/// First-of-month `NaiveDate` values are stored as `%Y-%m-%d` strings.
fn month_key(month: NaiveDate) -> String {
    month.format("%Y-%m-%d").to_string()
}

impl CalendarCache {
    /// Opens (creating if needed) the cache database for `account_id` under
    /// `$XDG_CACHE_HOME/lookout/calendar/`.
    pub fn open(account_id: &AccountId) -> Result<Self> {
        let dir = cache_dir();
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}.sqlite3", sanitize_filename(account_id)));
        let conn = Connection::open(path)?;
        // WAL + busy timeout + `synchronous=NORMAL`, the same tuning as the
        // mail cache: this file is written in full-table chunks (`store_month`)
        // and read from other threads (the calendar UI while the session
        // writes), so a per-commit fsync and the rollback journal both cost
        // real latency for data that is always rebuilt from the server.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS occurrences (
                month TEXT PRIMARY KEY,
                data TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS tasks (
                data TEXT NOT NULL
            );
            ",
        )?;
        Ok(CalendarCache { conn: Mutex::new(conn) })
    }

    /// The cached expanded occurrences for `month` (any date within the month -
    /// the first-of-month is normalized before the lookup), or `None` if this
    /// account has never synced that month.
    pub fn load_month(&self, month: NaiveDate) -> Result<Option<Vec<EventOccurrence>>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT data FROM occurrences WHERE month = ?1")?;
        let mut rows = stmt.query_map([month_key(first_of_month(month))], |row| row.get::<_, String>(0))?;
        match rows.next() {
            Some(row) => Ok(Some(serde_json::from_str(&row?)?)),
            None => Ok(None),
        }
    }

    /// Stores (replacing any previous entry) the freshly-fetched expanded
    /// occurrences for `month`.
    pub fn store_month(&self, month: NaiveDate, occurrences: &[EventOccurrence]) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let data = serde_json::to_string(occurrences)?;
        conn.execute(
            "INSERT OR REPLACE INTO occurrences (month, data) VALUES (?1, ?2)",
            rusqlite::params![month_key(first_of_month(month)), data],
        )?;
        Ok(())
    }

    /// The cached tasks from the last successful sync, or `None` if this
    /// account has never synced them. Like `load_month`, this is only a
    /// fast-paint hint - every `TasksUpdated` event from the live session
    /// supersedes it.
    pub fn load_tasks(&self) -> Result<Option<Vec<CalendarTask>>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT data FROM tasks")?;
        let mut rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        match rows.next() {
            Some(row) => Ok(Some(serde_json::from_str(&row?)?)),
            None => Ok(None),
        }
    }

    /// Replaces the cached task list with the freshly-fetched one (a single
    /// row holding the whole JSON array - tasks are few and small, and the
    /// table never grows).
    pub fn store_tasks(&self, tasks: &[CalendarTask]) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let data = serde_json::to_string(tasks)?;
        conn.execute("DELETE FROM tasks", [])?;
        conn.execute("INSERT INTO tasks (data) VALUES (?1)", [data])?;
        Ok(())
    }
}

fn first_of_month(date: NaiveDate) -> NaiveDate {
    date.with_day(1).unwrap_or(date)
}

// ---------------------------------------------------------------------------
// Contacts (CardDAV) cache
// ---------------------------------------------------------------------------

/// Per-account SQLite cache of CardDAV address books and their vCards, keyed
/// by server href. Unlike [`CalendarCache`] this is not just a fast-paint
/// hint: it also stores each address book's RFC 6578 `sync-token` so polls
/// can run incremental `sync-collection` REPORTs instead of refetching every
/// vCard, and it is the baseline the Deleted bucket diffs against (a contact
/// missing from the cache is a server-side deletion, tracked across
/// restarts).
///
/// Same `Mutex`-wrapped connection reasoning as [`CalendarCache`]: one
/// account's poll loop owns its cache, so the lock is uncontended in
/// practice and exists only to make `&ContactsCache: Send` across `.await`.
fn contacts_cache_dir() -> std::path::PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("lookout").join("contacts")
}

/// Returns the contacts cache directory and a list of `(filename, size_bytes)`
/// for each SQLite database file in it - the CardDAV counterpart of
/// [`cache_info`], for the config view's storage breakdown.
pub fn contacts_cache_info() -> (std::path::PathBuf, Vec<(String, u64)>) {
    let dir = contacts_cache_dir();
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

/// One address book as stored in the cache: its href, display name, and the
/// last `sync-token` returned by the server, if an incremental sync has ever
/// completed for it.
#[derive(Debug, Clone)]
pub struct CachedAddressBook {
    pub href: String,
    pub display_name: String,
    pub sync_token: Option<String>,
}

/// One contact as stored in the cache: the server href, the address book it
/// lives in, its last `getetag`, and the parsed vCard.
#[derive(Debug, Clone)]
pub struct CachedContact {
    pub href: String,
    pub book_href: String,
    pub etag: Option<String>,
    pub card: VCard,
}

pub struct ContactsCache {
    conn: Mutex<Connection>,
}

impl ContactsCache {
    /// Opens (creating if needed) the cache database for `account_id` under
    /// `$XDG_CACHE_HOME/lookout/contacts/`.
    pub fn open(account_id: &AccountId) -> Result<Self> {
        let dir = contacts_cache_dir();
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}.sqlite3", sanitize_filename(account_id)));
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS address_books (
                href TEXT PRIMARY KEY,
                displayname TEXT NOT NULL,
                sync_token TEXT
            );
            CREATE TABLE IF NOT EXISTS contacts (
                href TEXT PRIMARY KEY,
                book_href TEXT NOT NULL,
                etag TEXT,
                card TEXT NOT NULL
            );
            ",
        )?;
        Ok(ContactsCache { conn: Mutex::new(conn) })
    }

    /// All cached address books for the account, with their stored sync
    /// tokens (the incremental-sync cursor).
    pub fn load_address_books(&self) -> Result<Vec<CachedAddressBook>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT href, displayname, sync_token FROM address_books")?;
        let rows = stmt.query_map([], |row| {
            Ok(CachedAddressBook {
                href: row.get(0)?,
                display_name: row.get(1)?,
                sync_token: row.get(2)?,
            })
        })?;
        let mut books = Vec::new();
        for row in rows {
            books.push(row?);
        }
        Ok(books)
    }

    /// Every cached contact for the account (across all books).
    pub fn load_contacts(&self) -> Result<Vec<CachedContact>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT href, book_href, etag, card FROM contacts")?;
        let rows = stmt.query_map([], |row| {
            let href: String = row.get(0)?;
            let book_href: String = row.get(1)?;
            let etag: Option<String> = row.get(2)?;
            let card_text: String = row.get(3)?;
            match VCard::parse(&card_text) {
                Ok(card) => Ok(Some(CachedContact { href, book_href, etag, card })),
                Err(e) => {
                    tracing::warn!("skipping unparseable cached vCard at {href:?}: {e}");
                    Ok(None)
                }
            }
        })?;
        let mut contacts = Vec::new();
        for row in rows {
            if let Some(contact) = row? {
                contacts.push(contact);
            }
        }
        Ok(contacts)
    }

    /// Upserts the account's address-book list, preserving each book's stored
    /// sync token across re-discoveries (only the display name is refreshed).
    pub fn store_address_books(&self, books: &[crate::AddressBookInfo]) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        for book in books {
            conn.execute(
                "INSERT INTO address_books (href, displayname) VALUES (?1, ?2)
                 ON CONFLICT(href) DO UPDATE SET displayname = excluded.displayname",
                rusqlite::params![book.href, book.display_name],
            )?;
        }
        Ok(())
    }

    /// The stored `sync-token` cursor for one address book, or `None` when no
    /// incremental sync has completed for it yet (or the book is unknown).
    pub fn sync_token(&self, book_href: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT sync_token FROM address_books WHERE href = ?1")?;
        let mut rows = stmt.query_map([book_href], |row| row.get::<_, Option<String>>(0))?;
        match rows.next() {
            Some(row) => Ok(row?),
            None => Ok(None),
        }
    }

    /// Applies the result of one `sync-collection` poll for `book_href`:
    /// upserts the changed records, deletes the gone hrefs, and stores the
    /// server's next `sync_token` (or clears it when the caller fell back to
    /// a full refetch - the token only survives a completed incremental
    /// sync). The book row is upserted (not just updated) so the token
    /// survives even if discovery hasn't stored the book yet.
    pub fn apply_delta(&self, book_href: &str, new_token: Option<String>, changed: &[ContactRecord], deleted_hrefs: &[String]) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        for record in changed {
            conn.execute(
                "INSERT OR REPLACE INTO contacts (href, book_href, etag, card) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![record.href, book_href, record.etag, record.card.serialize()],
            )?;
        }
        for href in deleted_hrefs {
            conn.execute("DELETE FROM contacts WHERE href = ?1", [href])?;
        }
        conn.execute(
            "INSERT INTO address_books (href, displayname, sync_token) VALUES (?1, '', ?2)
             ON CONFLICT(href) DO UPDATE SET sync_token = excluded.sync_token",
            rusqlite::params![book_href, new_token],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod contacts_tests {
    use super::*;

    fn temp_account_id() -> AccountId {
        AccountId(format!("/test/contacts_cache_{}", uuid::Uuid::new_v4()))
    }

    fn sample_card(uid: &str, email: &str) -> VCard {
        VCard::parse(&format!("BEGIN:VCARD\r\nVERSION:4.0\r\nUID:{uid}\r\nFN:{uid}\r\nEMAIL:{email}\r\nEND:VCARD\r\n")).expect("sample card parses")
    }

    fn temp_path(account_id: &AccountId) -> std::path::PathBuf {
        contacts_cache_dir().join(format!("{}.sqlite3", sanitize_filename(account_id)))
    }

    #[test]
    fn stores_books_tokens_and_delta_round_trips() {
        let account_id = temp_account_id();
        let path = temp_path(&account_id);
        let cache = ContactsCache::open(&account_id).unwrap();

        assert!(cache.load_address_books().unwrap().is_empty());
        assert!(cache.load_contacts().unwrap().is_empty());

        let book = crate::AddressBookInfo {
            account_id: account_id.clone(),
            display_name: "Personal".to_string(),
            href: "/addressbooks/alice/personal/".to_string(),
        };
        cache.store_address_books(&[book]).unwrap();

        let alice = crate::ContactRecord {
            href: "/addressbooks/alice/personal/alice.vcf".to_string(),
            etag: Some("\"a1\"".to_string()),
            card: sample_card("alice", "alice@example.com"),
        };
        let bob = crate::ContactRecord {
            href: "/addressbooks/alice/personal/bob.vcf".to_string(),
            etag: Some("\"b1\"".to_string()),
            card: sample_card("bob", "bob@example.com"),
        };
        cache.apply_delta("/addressbooks/alice/personal/", Some("token-1".to_string()), &[alice, bob], &[]).unwrap();

        let books = cache.load_address_books().unwrap();
        assert_eq!(books.len(), 1);
        assert_eq!(books[0].sync_token.as_deref(), Some("token-1"));
        assert_eq!(books[0].display_name, "Personal");

        let contacts = cache.load_contacts().unwrap();
        assert_eq!(contacts.len(), 2);
        assert!(contacts.iter().any(|c| c.card.uid.as_deref() == Some("alice")));

        // A later delta deletes bob and keeps alice, moving the token on.
        cache
            .apply_delta(
                "/addressbooks/alice/personal/",
                Some("token-2".to_string()),
                &[],
                &["/addressbooks/alice/personal/bob.vcf".to_string()],
            )
            .unwrap();
        let contacts = cache.load_contacts().unwrap();
        assert_eq!(contacts.len(), 1);
        assert_eq!(contacts[0].href, "/addressbooks/alice/personal/alice.vcf");
        assert_eq!(cache.load_address_books().unwrap()[0].sync_token.as_deref(), Some("token-2"));

        // Reopening (a fresh connection, i.e. next launch) sees it all.
        drop(cache);
        let reopened = ContactsCache::open(&account_id).unwrap();
        assert_eq!(reopened.load_contacts().unwrap().len(), 1);
        assert_eq!(reopened.load_address_books().unwrap()[0].sync_token.as_deref(), Some("token-2"));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn full_refetch_clears_the_stored_sync_token() {
        let account_id = temp_account_id();
        let path = temp_path(&account_id);
        let cache = ContactsCache::open(&account_id).unwrap();
        let book = crate::AddressBookInfo {
            account_id: account_id.clone(),
            display_name: "Personal".to_string(),
            href: "/addressbooks/alice/personal/".to_string(),
        };
        cache.store_address_books(&[book]).unwrap();
        cache.apply_delta("/addressbooks/alice/personal/", Some("token-9".to_string()), &[], &[]).unwrap();
        // A fallback full refetch starts from "no token".
        cache.apply_delta("/addressbooks/alice/personal/", None, &[], &[]).unwrap();
        assert_eq!(cache.load_address_books().unwrap()[0].sync_token, None);

        let _ = std::fs::remove_file(&path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone, Utc};
    use lookout_core::CalendarId;

    fn temp_account_id() -> AccountId {
        AccountId(format!("/test/calendar_cache_{}", uuid::Uuid::new_v4()))
    }

    fn sample_occurrence(calendar_id: &CalendarId, summary: &str, day: u32) -> EventOccurrence {
        let start = Utc.with_ymd_and_hms(2026, 8, day, 9, 0, 0).unwrap();
        EventOccurrence {
            uid: lookout_core::EventUid(format!("uid-{day}")),
            calendar_id: calendar_id.clone(),
            summary: Some(summary.to_string()),
            description: None,
            location: None,
            start,
            end: start + Duration::hours(1),
            all_day: false,
            rrule: None,
            recurrence_id: None,
            exdates: Vec::new(),
            master_start: None,
            master_end: None,
            href: None,
            etag: None,
            master_href: None,
            master_etag: None,
            attendees: Vec::new(),
            organizer: None,
            categories: Vec::new(),
            sensitivity: lookout_core::EventSensitivity::default(),
            transparency: lookout_core::EventTransparency::default(),
            reminder_minutes_before: None,
            conference_url: None,
        }
    }

    #[test]
    fn round_trips_months_through_the_cache() {
        // Uses a unique account id (and therefore a unique sqlite file
        // under the real XDG cache dir) so parallel test runs don't collide;
        // this is acceptable for a fast, disk-backed unit test and mirrors
        // how the cache is actually keyed in production.
        let account_id = temp_account_id();
        let cache = CalendarCache::open(&account_id).unwrap();
        assert!(cache.load_month(NaiveDate::from_ymd_opt(2026, 8, 1).unwrap()).unwrap().is_none());

        let calendar = CalendarId("cal-1".to_string());
        let occurrences = vec![sample_occurrence(&calendar, "Standup", 3), sample_occurrence(&calendar, "Review", 10)];
        cache.store_month(NaiveDate::from_ymd_opt(2026, 8, 12).unwrap(), &occurrences).unwrap();

        // Any date within the month resolves to the same first-of-month key.
        let loaded = cache.load_month(NaiveDate::from_ymd_opt(2026, 8, 25).unwrap()).unwrap().unwrap();
        assert_eq!(loaded.len(), 2);
        assert!(loaded.iter().any(|o| o.summary.as_deref() == Some("Standup")));
        assert!(loaded.iter().any(|o| o.summary.as_deref() == Some("Review")));

        // Storing again replaces the entry rather than accumulating.
        cache.store_month(NaiveDate::from_ymd_opt(2026, 8, 1).unwrap(), &occurrences[..1]).unwrap();
        assert_eq!(cache.load_month(NaiveDate::from_ymd_opt(2026, 8, 1).unwrap()).unwrap().unwrap().len(), 1);

        let path = cache_dir().join(format!("{}.sqlite3", sanitize_filename(&account_id)));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn distinct_months_and_accounts_do_not_collide() {
        let account_id = temp_account_id();
        let cache = CalendarCache::open(&account_id).unwrap();
        let calendar = CalendarId("cal-1".to_string());
        cache
            .store_month(NaiveDate::from_ymd_opt(2026, 7, 1).unwrap(), &[sample_occurrence(&calendar, "July", 5)])
            .unwrap();
        cache
            .store_month(NaiveDate::from_ymd_opt(2026, 8, 1).unwrap(), &[sample_occurrence(&calendar, "August", 5)])
            .unwrap();

        assert_eq!(
            cache.load_month(NaiveDate::from_ymd_opt(2026, 7, 15).unwrap()).unwrap().unwrap()[0].summary.as_deref(),
            Some("July")
        );
        assert_eq!(
            cache.load_month(NaiveDate::from_ymd_opt(2026, 8, 15).unwrap()).unwrap().unwrap()[0].summary.as_deref(),
            Some("August")
        );

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
    fn round_trips_tasks_through_the_cache() {
        let account_id = temp_account_id();
        let cache = CalendarCache::open(&account_id).unwrap();
        assert!(cache.load_tasks().unwrap().is_none());

        let task = |uid: &str, summary: &str| CalendarTask {
            uid: lookout_core::TaskUid(uid.to_string()),
            calendar_id: CalendarId("cal-1".to_string()),
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
        cache.store_tasks(&[task("t-1", "First"), task("t-2", "Second")]).unwrap();

        let loaded = cache.load_tasks().unwrap().unwrap();
        assert_eq!(loaded.len(), 2);
        assert!(loaded.iter().any(|t| t.summary.as_deref() == Some("First")));

        // Storing again replaces rather than accumulates.
        cache.store_tasks(&[task("t-3", "Third")]).unwrap();
        assert_eq!(cache.load_tasks().unwrap().unwrap().len(), 1);
        assert_eq!(cache.load_tasks().unwrap().unwrap()[0].summary.as_deref(), Some("Third"));

        let path = cache_dir().join(format!("{}.sqlite3", sanitize_filename(&account_id)));
        let _ = std::fs::remove_file(path);
    }
}
