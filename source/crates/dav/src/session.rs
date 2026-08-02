use std::time::Duration;

use chrono::{Datelike, NaiveDate, NaiveTime};
use lookout_core::{CalendarInfo, EventOccurrence};

use crate::cache::CalendarCache;
use crate::client::DavClient;
use crate::config::{CalendarAccountConfig, Credential};
use crate::error::{Error, Result};
use crate::recurrence;

/// How often to re-poll while idle. CalDAV has no IMAP-IDLE-equivalent
/// long-poll built into the core spec (RFC 6578 `sync-collection` would be
/// the "proper" incremental mechanism, but is out of scope for this pass) -
/// so this is a plain fixed-interval refetch instead.
const POLL_INTERVAL: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone)]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Idle,
    Busy,
    Error { message: String, retryable: bool },
}

#[derive(Debug)]
pub enum CalendarCommand {
    /// Any date within the month to display; the actor resolves the
    /// month's own first/last-instant bounds itself.
    SyncMonth(NaiveDate),
    /// Force a resync of the currently-displayed month outside the poll cadence.
    Refresh,
    /// A hint that it's worth retrying the connection now rather than
    /// waiting out the current backoff delay. A no-op if already connected.
    Reconnect,
    Shutdown,
}

/// Named `CalendarSessionEvent` rather than `CalendarEvent` to avoid
/// colliding with `lookout_core::CalendarEvent` when both are in scope
/// unqualified (e.g. in `crates/app/src/window.rs`).
#[derive(Debug)]
pub enum CalendarSessionEvent {
    ConnectionStateChanged(ConnectionState),
    CalendarsUpdated(Vec<CalendarInfo>),
    OccurrencesUpdated { month: NaiveDate, occurrences: Vec<EventOccurrence> },
    Error(String),
}

/// Fetches a fresh credential immediately before each (re)connect attempt.
/// `lookout-dav` never caches credentials itself; the app crate implements
/// this trait against `lookout-goa`, keeping this crate free of D-Bus
/// concerns and independently testable.
#[async_trait::async_trait]
pub trait CalendarCredentialProvider: Send + Sync {
    async fn calendar_credential(&self) -> std::result::Result<Credential, String>;
}

/// Runs one account's CalDAV sync lifecycle on the calling task (spawn this
/// onto the shared tokio worker thread, same as `lookout_mail::session::
/// run_account_session`). Reconnects with backoff on any error; re-fetches
/// credentials from `credentials` on every attempt rather than reusing a
/// possibly-expired one.
pub async fn run_calendar_session(
    config: CalendarAccountConfig,
    credentials: std::sync::Arc<dyn CalendarCredentialProvider>,
    commands: async_channel::Receiver<CalendarCommand>,
    events: async_channel::Sender<CalendarSessionEvent>,
) {
    let cache = match CalendarCache::open(&config.account_id) {
        Ok(cache) => Some(cache),
        Err(e) => {
            tracing::warn!("couldn't open local calendar cache, continuing without it: {e}");
            None
        }
    };

    // Fast first paint: emit whatever's cached for the current month from a
    // previous session before the network connection even starts. This is
    // immediately superseded by live data once the sync lands - the cache is
    // never treated as authoritative (see `CalendarCache`'s doc comment).
    let current_month = first_of_month(chrono::Utc::now().date_naive());
    if let Some(cache) = &cache {
        if let Ok(Some(occurrences)) = cache.load_month(current_month) {
            let _ = events
                .send(CalendarSessionEvent::OccurrencesUpdated {
                    month: current_month,
                    occurrences,
                })
                .await;
        }
    }

    let mut backoff = Duration::from_secs(1);
    const MAX_BACKOFF: Duration = Duration::from_secs(60);

    loop {
        let _ = events.send(CalendarSessionEvent::ConnectionStateChanged(ConnectionState::Connecting)).await;
        match connect_and_run(&config, credentials.as_ref(), &commands, &events, cache.as_ref()).await {
            Ok(ShutdownReason::Requested) => {
                let _ = events.send(CalendarSessionEvent::ConnectionStateChanged(ConnectionState::Disconnected)).await;
                return;
            }
            Err(e) => {
                tracing::warn!("calendar session error, will reconnect: {e}");
                let _ = events.send(CalendarSessionEvent::Error(e.to_string())).await;
                let _ = events
                    .send(CalendarSessionEvent::ConnectionStateChanged(ConnectionState::Error {
                        message: e.to_string(),
                        retryable: true,
                    }))
                    .await;
            }
        }

        // Wait out the backoff delay, but cut it short if a command arrives
        // in the meantime (in particular `Reconnect` - see
        // `lookout_mail::session`'s identical pattern for why). `Shutdown`
        // received while disconnected exits immediately.
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            cmd = commands.recv() => {
                if matches!(cmd, Ok(CalendarCommand::Shutdown)) {
                    let _ = events.send(CalendarSessionEvent::ConnectionStateChanged(ConnectionState::Disconnected)).await;
                    return;
                }
            }
        }
        backoff = std::cmp::min(backoff * 2, MAX_BACKOFF);
    }
}

enum ShutdownReason {
    Requested,
}

async fn connect_and_run(
    config: &CalendarAccountConfig,
    credentials: &dyn CalendarCredentialProvider,
    commands: &async_channel::Receiver<CalendarCommand>,
    events: &async_channel::Sender<CalendarSessionEvent>,
    cache: Option<&CalendarCache>,
) -> Result<ShutdownReason> {
    let credential = credentials.calendar_credential().await.map_err(Error::LoginFailed)?;
    let client = DavClient::new(&config.base_url, config.accept_ssl_errors, config.username.clone())?;

    let home_href = client.discover_calendar_home(&credential).await?;
    let calendars = client.list_calendars(&home_href, &config.account_id, &credential).await?;
    let _ = events.send(CalendarSessionEvent::CalendarsUpdated(calendars.clone())).await;

    let mut current_month = first_of_month(chrono::Utc::now().date_naive());
    sync_month(&client, &calendars, &credential, current_month, events, cache).await;

    loop {
        let _ = events.send(CalendarSessionEvent::ConnectionStateChanged(ConnectionState::Idle)).await;

        enum Wake {
            Poll,
            Command(CalendarCommand),
            ChannelClosed,
        }
        let wake = tokio::select! {
            _ = tokio::time::sleep(POLL_INTERVAL) => Wake::Poll,
            c = commands.recv() => match c {
                Ok(cmd) => Wake::Command(cmd),
                Err(_) => Wake::ChannelClosed,
            },
        };

        let _ = events.send(CalendarSessionEvent::ConnectionStateChanged(ConnectionState::Busy)).await;

        let mut woke_on_command = None;
        match wake {
            Wake::Poll => sync_month(&client, &calendars, &credential, current_month, events, cache).await,
            Wake::Command(cmd) => woke_on_command = Some(cmd),
            Wake::ChannelClosed => return Ok(ShutdownReason::Requested),
        }

        // Process the command that woke us (if any), then drain any further
        // commands queued up while we were mid-sync.
        for command in woke_on_command.into_iter().chain(std::iter::from_fn(|| commands.try_recv().ok())) {
            match command {
                CalendarCommand::Shutdown => return Ok(ShutdownReason::Requested),
                CalendarCommand::Refresh => sync_month(&client, &calendars, &credential, current_month, events, cache).await,
                CalendarCommand::SyncMonth(date) => {
                    current_month = first_of_month(date);
                    sync_month(&client, &calendars, &credential, current_month, events, cache).await;
                }
                // Already connected - nothing to reconnect. Only useful
                // while backed off between connection attempts, see
                // `run_calendar_session`.
                CalendarCommand::Reconnect => {}
            }
        }
    }
}

/// Fetches events for every known calendar over a padded window (the
/// displayed month plus a week on each side, so multi-day events and RRULE
/// occurrences anchored just outside the month still get caught), expands
/// recurrences, clips back to the exact month, and emits the result.
///
/// The on-disk cache is served *first* - a month that's been synced before
/// renders immediately from what's stored, and the live fetch below
/// supersedes it as soon as it lands (stale-while-revalidate, so revisiting
/// a month doesn't sit empty during the network round-trip). The fresh
/// result is then written back to the cache.
///
/// A single calendar's fetch failing (e.g. a transient per-collection error)
/// is logged and skipped rather than aborting the whole sync - unlike
/// `lookout_mail::session`'s single-mailbox-at-a-time design, one account
/// here can have several independent calendar collections, and one flaky
/// collection shouldn't blank out every other calendar's events.
async fn sync_month(
    client: &DavClient,
    calendars: &[CalendarInfo],
    credential: &Credential,
    month: NaiveDate,
    events: &async_channel::Sender<CalendarSessionEvent>,
    cache: Option<&CalendarCache>,
) {
    if let Some(cache) = cache {
        if let Ok(Some(occurrences)) = cache.load_month(month) {
            let _ = events.send(CalendarSessionEvent::OccurrencesUpdated { month, occurrences }).await;
        }
    }

    let month_end = next_month(month);
    let fetch_start = (month - chrono::Duration::days(7)).and_time(NaiveTime::MIN).and_utc();
    let fetch_end = (month_end + chrono::Duration::days(7)).and_time(NaiveTime::MIN).and_utc();
    let window_start = month.and_time(NaiveTime::MIN).and_utc();
    let window_end = month_end.and_time(NaiveTime::MIN).and_utc();

    let mut occurrences = Vec::new();
    for calendar in calendars {
        match client.fetch_events_in_range(calendar, fetch_start, fetch_end, credential).await {
            Ok(calendar_events) => {
                for event in &calendar_events {
                    occurrences.extend(recurrence::expand_occurrences(event, window_start, window_end));
                }
            }
            Err(e) => {
                tracing::warn!("failed to fetch events for calendar {:?}: {e}", calendar.display_name);
            }
        }
    }

    let _ = events
        .send(CalendarSessionEvent::OccurrencesUpdated {
            month,
            occurrences: occurrences.clone(),
        })
        .await;
    if let Some(cache) = cache {
        if let Err(e) = cache.store_month(month, &occurrences) {
            tracing::warn!("failed to cache occurrences for {month}: {e}");
        }
    }
}

fn first_of_month(date: NaiveDate) -> NaiveDate {
    date.with_day(1).unwrap_or(date)
}

fn next_month(date: NaiveDate) -> NaiveDate {
    if date.month() == 12 {
        NaiveDate::from_ymd_opt(date.year() + 1, 1, 1).unwrap_or(date)
    } else {
        NaiveDate::from_ymd_opt(date.year(), date.month() + 1, 1).unwrap_or(date)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_of_month_normalizes_any_day() {
        assert_eq!(first_of_month(NaiveDate::from_ymd_opt(2026, 7, 17).unwrap()), NaiveDate::from_ymd_opt(2026, 7, 1).unwrap());
    }

    #[test]
    fn next_month_wraps_december_into_next_year() {
        assert_eq!(next_month(NaiveDate::from_ymd_opt(2026, 12, 1).unwrap()), NaiveDate::from_ymd_opt(2027, 1, 1).unwrap());
    }

    #[test]
    fn next_month_within_same_year() {
        assert_eq!(next_month(NaiveDate::from_ymd_opt(2026, 7, 1).unwrap()), NaiveDate::from_ymd_opt(2026, 8, 1).unwrap());
    }
}
