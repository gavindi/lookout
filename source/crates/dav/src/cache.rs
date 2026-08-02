use std::sync::Mutex;

use chrono::{Datelike, NaiveDate};
use lookout_core::{AccountId, EventOccurrence};
use rusqlite::Connection;

use crate::Result;

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
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS occurrences (
                month TEXT PRIMARY KEY,
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
}

fn first_of_month(date: NaiveDate) -> NaiveDate {
    date.with_day(1).unwrap_or(date)
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
            start,
            end: start + Duration::hours(1),
            all_day: false,
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
}
