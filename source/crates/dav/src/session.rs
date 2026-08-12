use std::time::Duration;

use chrono::{DateTime, Datelike, NaiveDate, NaiveTime, Utc};
use lookout_core::{CalendarEvent, CalendarId, CalendarInfo, CalendarTask, EventOccurrence, EventUid};

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
    /// Fetch one month's occurrences without making it the polled month -
    /// the Lookout dashboard's "upcoming events" horizon reaches into next
    /// month without hijacking `current_month`, so the 5-minute poll keeps
    /// re-syncing whatever month the calendar view is showing.
    FetchMonth(NaiveDate),
    /// Force a resync of the currently-displayed month outside the poll cadence.
    Refresh,
    /// A hint that it's worth retrying the connection now rather than
    /// waiting out the current backoff delay. A no-op if already connected.
    Reconnect,
    /// Store a brand-new event: PUT the serialized VEVENT as a new calendar
    /// object under `event.calendar_id`'s collection (a client-generated
    /// `<uid>.ics` href, `If-None-Match: *`). Resyncs the on-screen month on
    /// success so the new occurrence renders.
    CreateEvent {
        event: Box<CalendarEvent>,
    },
    /// Store an edited event in place: PUT to `event.href` with `event.etag`
    /// as `If-Match` (fails with a surfaced error rather than clobbering a
    /// concurrent change if the etag is stale). Resyncs on success.
    UpdateEvent {
        event: Box<CalendarEvent>,
    },
    /// Delete the calendar object at `href` (with an optional `etag` as
    /// `If-Match`). Resyncs on success.
    DeleteEvent {
        calendar_id: CalendarId,
        href: String,
        etag: Option<String>,
    },
    /// Force a resync of every task in every calendar, outside the poll
    /// cadence (e.g. right after a task edit lands).
    SyncTasks,
    /// Store a brand-new task: PUT the serialized VTODO as a new calendar
    /// object under `task.calendar_id`'s collection (a client-generated
    /// `<uid>.ics` href, `If-None-Match: *`). Resyncs tasks on success.
    CreateTask {
        task: Box<CalendarTask>,
    },
    /// Store an edited task in place: PUT to `task.href` with `task.etag`
    /// as `If-Match`. Resyncs tasks on success.
    UpdateTask {
        task: Box<CalendarTask>,
    },
    /// Delete the calendar object at `href` (with an optional `etag` as
    /// `If-Match`). Resyncs tasks on success.
    DeleteTask {
        calendar_id: CalendarId,
        href: String,
        etag: Option<String>,
    },
    Shutdown,
}

/// Named `CalendarSessionEvent` rather than `CalendarEvent` to avoid
/// colliding with `lookout_core::CalendarEvent` when both are in scope
/// unqualified (e.g. in `crates/app/src/window.rs`).
#[derive(Debug)]
pub enum CalendarSessionEvent {
    ConnectionStateChanged(ConnectionState),
    CalendarsUpdated(Vec<CalendarInfo>),
    OccurrencesUpdated {
        month: NaiveDate,
        occurrences: Vec<EventOccurrence>,
    },
    TasksUpdated(Vec<CalendarTask>),
    Error(String),
    /// A `CreateEvent`/`UpdateEvent` request failed. Kept distinct from
    /// `Error` so the UI can roll back exactly the occurrence it
    /// optimistically moved (if any - harmless no-op otherwise), mirroring
    /// `lookout_mail::session::AccountEvent::MoveFailed`.
    EventSaveFailed {
        uid: EventUid,
        recurrence_id: Option<DateTime<Utc>>,
        message: String,
    },
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
                // Connection failures are warning-level: the loop below
                // retries with backoff, so only the connection-lifecycle
                // event is sent (no duplicate `CalendarSessionEvent::Error`).
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
                match cmd {
                    Ok(CalendarCommand::Shutdown) => {
                        let _ = events.send(CalendarSessionEvent::ConnectionStateChanged(ConnectionState::Disconnected)).await;
                        return;
                    }
                    // A write command can't be honoured while the connection
                    // is down, and silently dropping it would lose the user's
                    // edit with no trace - answer with an explicit error the
                    // UI can toast. Read-only hints (SyncMonth/Refresh) are
                    // safe to drop: the reconnect sync supersedes them.
                    Ok(CalendarCommand::CreateEvent { event }) => {
                        let _ = events.send(CalendarSessionEvent::Error(format!("not connected - \"{}\" was not saved", event.uid))).await;
                    }
                    Ok(CalendarCommand::UpdateEvent { event }) => {
                        let _ = events.send(CalendarSessionEvent::Error(format!("not connected - changes to \"{}\" were not saved", event.uid))).await;
                    }
                    Ok(CalendarCommand::DeleteEvent { .. }) => {
                        let _ = events.send(CalendarSessionEvent::Error("not connected - the event was not deleted".to_string())).await;
                    }
                    Ok(CalendarCommand::CreateTask { task }) => {
                        let _ = events.send(CalendarSessionEvent::Error(format!("not connected - \"{}\" was not saved", task.uid))).await;
                    }
                    Ok(CalendarCommand::UpdateTask { task }) => {
                        let _ = events.send(CalendarSessionEvent::Error(format!("not connected - changes to \"{}\" were not saved", task.uid))).await;
                    }
                    Ok(CalendarCommand::DeleteTask { .. }) => {
                        let _ = events.send(CalendarSessionEvent::Error("not connected - the task was not deleted".to_string())).await;
                    }
                    Ok(_) => {}
                    Err(_) => {}
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
    sync_tasks(&client, &calendars, &credential, events, cache).await;

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
            Wake::Poll => {
                sync_month(&client, &calendars, &credential, current_month, events, cache).await;
                sync_tasks(&client, &calendars, &credential, events, cache).await;
            }
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
                CalendarCommand::FetchMonth(date) => {
                    sync_month(&client, &calendars, &credential, first_of_month(date), events, cache).await;
                }
                CalendarCommand::CreateEvent { event } => {
                    write_event(&client, &calendars, &credential, &event, current_month, events, cache).await;
                }
                CalendarCommand::UpdateEvent { event } => {
                    write_event(&client, &calendars, &credential, &event, current_month, events, cache).await;
                }
                CalendarCommand::DeleteEvent { calendar_id: _, href, etag } => match client.delete_calendar_object(&href, &credential, etag.as_deref()).await {
                    Ok(()) => sync_month(&client, &calendars, &credential, current_month, events, cache).await,
                    Err(e) => {
                        let _ = events.send(CalendarSessionEvent::Error(format!("failed to delete event: {e}"))).await;
                    }
                },
                CalendarCommand::SyncTasks => sync_tasks(&client, &calendars, &credential, events, cache).await,
                CalendarCommand::CreateTask { task } => {
                    write_task(&client, &calendars, &credential, &task, events, cache).await;
                }
                CalendarCommand::UpdateTask { task } => {
                    write_task(&client, &calendars, &credential, &task, events, cache).await;
                }
                CalendarCommand::DeleteTask { calendar_id: _, href, etag } => match client.delete_calendar_object(&href, &credential, etag.as_deref()).await {
                    Ok(()) => sync_tasks(&client, &calendars, &credential, events, cache).await,
                    Err(e) => {
                        let _ = events.send(CalendarSessionEvent::Error(format!("failed to delete task: {e}"))).await;
                    }
                },
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
                // Group by UID: a recurring master and its per-occurrence
                // overrides (VEVENTs sharing the UID with RECURRENCE-ID) must
                // be expanded together, or the override would double-render
                // next to the master instance it replaces. A master without
                // overrides (the common case) expands exactly as before.
                let mut masters: Vec<&CalendarEvent> = Vec::new();
                let mut overrides: std::collections::HashMap<&EventUid, Vec<&CalendarEvent>> = std::collections::HashMap::new();
                for event in &calendar_events {
                    if event.recurrence_id.is_some() {
                        overrides.entry(&event.uid).or_default().push(event);
                    } else {
                        masters.push(event);
                    }
                }
                for master in masters {
                    occurrences.extend(recurrence::expand_master_with_overrides(
                        master,
                        overrides.remove(&master.uid).unwrap_or_default(),
                        window_start,
                        window_end,
                    ));
                }
                // Stray overrides whose master didn't arrive in this fetch
                // (e.g. split across responses) still render, alone.
                for events in overrides.values() {
                    for event in events {
                        occurrences.extend(recurrence::expand_occurrences(event, window_start, window_end));
                    }
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

/// Creates or updates `event` on the server, sharing one code path: serialize
/// to iCalendar, resolve the target calendar collection, PUT the VEVENT, then
/// resync the on-screen month (and the event's own month, if different) so the
/// change renders immediately. The create/update distinction falls out of the
/// event's metadata - a fresh event has no `href` (so a client-generated
/// `<uid>.ics` href under the collection and `If-None-Match: *`), an edit
/// keeps its `href`/`etag` (so `If-Match` guards against clobbering).
///
/// Any failure (calendar not found, HTTP 412 on a stale etag, auth, ...) is
/// reported via `CalendarSessionEvent::Error` rather than aborting the session;
/// the caller's toast shows it.
async fn write_event(
    client: &DavClient,
    calendars: &[CalendarInfo],
    credential: &Credential,
    event: &CalendarEvent,
    current_month: NaiveDate,
    events: &async_channel::Sender<CalendarSessionEvent>,
    cache: Option<&CalendarCache>,
) {
    let Some(calendar) = calendars.iter().find(|c| c.id == event.calendar_id) else {
        let _ = events
            .send(CalendarSessionEvent::EventSaveFailed {
                uid: event.uid.clone(),
                recurrence_id: event.recurrence_id,
                message: format!("calendar for event \"{}\" not found", event.uid),
            })
            .await;
        return;
    };

    let ics = crate::ical::build_vcalendar(event);
    let href = match &event.href {
        Some(href) => href.clone(),
        None => format!("{}{}.ics", calendar.href, url_safe_uid(&event.uid.0)),
    };

    match client.put_calendar_object(&href, &ics, credential, event.etag.as_deref()).await {
        Ok(_new_etag) => {
            let mut months = vec![current_month];
            let event_month = first_of_month(event.start.date_naive());
            if event_month != current_month {
                months.push(event_month);
            }
            for month in months {
                sync_month(client, calendars, credential, month, events, cache).await;
            }
        }
        Err(e) => {
            let _ = events
                .send(CalendarSessionEvent::EventSaveFailed {
                    uid: event.uid.clone(),
                    recurrence_id: event.recurrence_id,
                    message: format!("failed to save event \"{}\": {e}", event.uid),
                })
                .await;
        }
    }
}

/// Fetches every task across every calendar and emits the merged result.
/// Unlike [`sync_month`] there's no month window - tasks have no guaranteed
/// temporal span, so the whole set is always refetched (they're small).
/// Same cache-first fast-paint and per-collection failure isolation as
/// `sync_month`.
async fn sync_tasks(client: &DavClient, calendars: &[CalendarInfo], credential: &Credential, events: &async_channel::Sender<CalendarSessionEvent>, cache: Option<&CalendarCache>) {
    if let Some(cache) = cache {
        if let Ok(Some(tasks)) = cache.load_tasks() {
            let _ = events.send(CalendarSessionEvent::TasksUpdated(tasks)).await;
        }
    }

    let mut tasks = Vec::new();
    for calendar in calendars {
        if !calendar.supports_tasks {
            tracing::debug!("skipping task sync for calendar {:?}: server advertises no VTODO support", calendar.display_name);
            continue;
        }
        match client.fetch_tasks(calendar, credential).await {
            Ok(calendar_tasks) => tasks.extend(calendar_tasks),
            Err(e) => {
                tracing::warn!("failed to fetch tasks for calendar {:?}: {e}", calendar.display_name);
            }
        }
    }

    let _ = events.send(CalendarSessionEvent::TasksUpdated(tasks.clone())).await;
    if let Some(cache) = cache {
        if let Err(e) = cache.store_tasks(&tasks) {
            tracing::warn!("failed to cache tasks: {e}");
        }
    }
}

/// Creates or updates `task` on the server - the `write_event` counterpart,
/// sharing its create/update-by-metadata convention: a fresh task has no
/// `href` (client-generated `<uid>.ics` href + `If-None-Match: *`), an edit
/// keeps its `href`/`etag` (`If-Match` against clobbering). Resyncs tasks on
/// success so the change renders immediately.
async fn write_task(
    client: &DavClient,
    calendars: &[CalendarInfo],
    credential: &Credential,
    task: &CalendarTask,
    events: &async_channel::Sender<CalendarSessionEvent>,
    cache: Option<&CalendarCache>,
) {
    let Some(calendar) = calendars.iter().find(|c| c.id == task.calendar_id) else {
        let _ = events.send(CalendarSessionEvent::Error(format!("calendar for task \"{}\" not found", task.uid))).await;
        return;
    };
    if !calendar.supports_tasks {
        // A server that advertises no VTODO support (e.g. Google's CalDAV,
        // which answers a task PUT with a bare 403) can never store this
        // task - surface a clear message instead of the raw server error.
        let _ = events
            .send(CalendarSessionEvent::Error(format!(
                "task \"{}\" was not saved - calendar \"{}\" does not support tasks",
                task.uid, calendar.display_name
            )))
            .await;
        return;
    }

    let ics = crate::ical::build_vtodo_calendar(task);
    let href = match &task.href {
        Some(href) => href.clone(),
        None => format!("{}{}.ics", calendar.href, url_safe_uid(&task.uid.0)),
    };

    match client.put_calendar_object(&href, &ics, credential, task.etag.as_deref()).await {
        Ok(_new_etag) => sync_tasks(client, calendars, credential, events, cache).await,
        Err(e) => {
            let _ = events.send(CalendarSessionEvent::Error(format!("failed to save task \"{}\": {e}", task.uid))).await;
        }
    }
}

/// Makes a UID safe to use as a URL path segment in a client-generated
/// calendar-object href. UIDs are normally email-style (`evt-1@example.com`),
/// and `@`/`%`/`/` are legal iCalendar text but need encoding in a URI - the
/// round-trip only cares that the server stores the object under some
/// collection-relative name; it never parses the UID back out of the href.
fn url_safe_uid(uid: &str) -> String {
    let mut out = String::with_capacity(uid.len());
    let mut kept_alnum = false;
    for c in uid.chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
            kept_alnum |= c.is_ascii_alphanumeric();
            out.push(c);
        } else {
            out.push('_');
        }
    }
    // A UID of nothing but URL-unsafe characters (or an empty string) would
    // produce a bare-underscore name; fall back to something meaningful.
    if !kept_alnum {
        out = "event".to_string();
    }
    out
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

    #[test]
    fn url_safe_uid_preserves_ascii_and_underscores_the_rest() {
        assert_eq!(url_safe_uid("evt-1@example.com"), "evt-1_example_com");
        assert_eq!(url_safe_uid("plain"), "plain");
        assert_eq!(url_safe_uid("###"), "event");
        assert_eq!(url_safe_uid(""), "event");
    }
}
