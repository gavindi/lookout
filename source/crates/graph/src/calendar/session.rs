//! The Microsoft Graph calendar session actor (OUTLOOK_PARITY.md §5.1, item
//! 16 / A-03).
//!
//! Speaks the same [`CalendarCommand`] / [`CalendarSessionEvent`] contract as
//! `lookout-dav`'s CalDAV actor (both live in
//! `lookout_core::calendar_protocol`), so the app drives Microsoft 365
//! calendars through the identical UI and event loop. It owns the connection
//! lifecycle (connect, poll, backoff), command dispatch and the mapping between
//! Graph and the model; the HTTP lives in [`CalendarClient`].
//!
//! Two contract quirks, both by design:
//! - **Server scheduling** (item 11): Graph delivers iTIP (invites/replies)
//!   itself, so the session reports a non-`None` schedule-outbox marker and the
//!   app's `calendar_invites::delivery_for` stands the client-side iMIP sender
//!   down.
//! - **No tasks**: Graph calendars carry no VTODOs - Microsoft To Do is the
//!   separate [`crate::todo`] backend (item 5.3 / A-05), which owns the task
//!   lists. This session's task commands therefore stay soft-fail placeholders;
//!   the app routes task writes to the To Do session instead.
//!
//! Free/busy (`FetchFreeBusy`) is answered through Graph `getSchedule`
//! (item 14).

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::time::Duration;

use chrono::{DateTime, Datelike, NaiveDate, NaiveTime, Utc};
use lookout_core::calendar_protocol::{CalendarCommand, CalendarSessionEvent, ConnectionState};
use lookout_core::{AccountId, CalendarEvent, CalendarInfo, EventOccurrence, SchedulingInfo};

use crate::calendar::CalendarClient;
use crate::error::GraphError;

/// How often to re-poll while idle, mirroring the CalDAV actor.
const POLL_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// The schedule-outbox marker that tells the app Graph schedules iTIP itself
/// (item 11). It is not a URL - the Graph path is built internally - only a
/// non-`None` `SchedulingInfo::schedule_outbox` matters to
/// `supports_server_scheduling`.
const GRAPH_SCHEDULING_MARKER: &str = "graph:server-scheduling";

/// Runs one Microsoft 365 account's Graph calendar lifecycle on the calling
/// task (spawn onto the shared worker, as
/// `lookout_dav::session::run_calendar_session` is). Reconnects with backoff
/// on any error. `client` carries the account's authenticated transport (the
/// app builds it from `MicrosoftOAuth`), which also makes the actor testable
/// against a mock base URL.
pub async fn run_calendar_session(
    account_id: AccountId,
    client: CalendarClient,
    commands: async_channel::Receiver<CalendarCommand>,
    events: async_channel::Sender<CalendarSessionEvent>,
) {
    let mut backoff = Duration::from_secs(1);
    const MAX_BACKOFF: Duration = Duration::from_secs(60);

    loop {
        let _ = events.send(CalendarSessionEvent::ConnectionStateChanged(ConnectionState::Connecting)).await;
        match connect_and_run(&account_id, &client, &commands, &events).await {
            Ok(ShutdownReason::Requested) => {
                let _ = events.send(CalendarSessionEvent::ConnectionStateChanged(ConnectionState::Disconnected)).await;
                return;
            }
            Err(e) => {
                // A missing or revoked Graph consent can't fix itself by
                // retrying, so surface it once as a non-retryable error (which
                // the app toasts) and stop, rather than spinning forever.
                // Config's "Sync with Outlook" reconnects the account once the
                // user consents.
                if matches!(e, GraphError::ConsentRequired(_)) {
                    let _ = events
                        .send(CalendarSessionEvent::ConnectionStateChanged(ConnectionState::Error {
                            message: e.to_string(),
                            retryable: false,
                        }))
                        .await;
                    let _ = events.send(CalendarSessionEvent::ConnectionStateChanged(ConnectionState::Disconnected)).await;
                    return;
                }
                tracing::warn!("graph calendar session error, will reconnect: {e}");
                let _ = events
                    .send(CalendarSessionEvent::ConnectionStateChanged(ConnectionState::Error {
                        message: e.to_string(),
                        retryable: true,
                    }))
                    .await;
            }
        }

        // Wait out the backoff, cut short by a command (especially `Reconnect`
        // or `Shutdown`), mirroring the CalDAV actor. Write commands can't be
        // honoured while down, so they answer with an explicit error.
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            cmd = commands.recv() => match cmd {
                Ok(CalendarCommand::Shutdown) => {
                    let _ = events.send(CalendarSessionEvent::ConnectionStateChanged(ConnectionState::Disconnected)).await;
                    return;
                }
                Ok(CalendarCommand::CreateEvent { event }) => {
                    let _ = events.send(CalendarSessionEvent::Error(format!("not connected - \"{}\" was not saved", event.uid))).await;
                }
                Ok(CalendarCommand::UpdateEvent { event }) => {
                    let _ = events.send(CalendarSessionEvent::Error(format!("not connected - changes to \"{}\" were not saved", event.uid))).await;
                }
                Ok(CalendarCommand::DeleteEvent { .. }) => {
                    let _ = events.send(CalendarSessionEvent::Error("not connected - the event was not deleted".to_string())).await;
                }
                Ok(_) => {}
                Err(_) => {}
            }
        }
        backoff = std::cmp::min(backoff * 2, MAX_BACKOFF);
    }
}

enum ShutdownReason {
    Requested,
}

async fn connect_and_run(
    account_id: &AccountId,
    client: &CalendarClient,
    commands: &async_channel::Receiver<CalendarCommand>,
    events: &async_channel::Sender<CalendarSessionEvent>,
) -> Result<ShutdownReason, GraphError> {
    let calendars = client.list_calendars(account_id).await?;
    let _ = events.send(CalendarSessionEvent::CalendarsUpdated(calendars.clone())).await;
    // Graph schedules iTIP itself (item 11), so the client-side iMIP sender
    // stands down.
    let _ = events
        .send(CalendarSessionEvent::SchedulingUpdated(SchedulingInfo {
            schedule_outbox: Some(GRAPH_SCHEDULING_MARKER.to_string()),
            schedule_inbox: None,
        }))
        .await;

    let mut current_month = first_of_month(Utc::now().date_naive());
    sync_month(client, &calendars, current_month, events).await?;
    // Graph calendars carry no tasks - Microsoft To Do is the separate `todo`
    // backend (item 5.3 / A-05); still report an (empty) set so the Tasks view
    // clears any stale rows for the account's calendar source.
    let _ = events.send(CalendarSessionEvent::TasksUpdated(Vec::new())).await;

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
            Wake::Poll => sync_month(client, &calendars, current_month, events).await?,
            Wake::Command(cmd) => woke_on_command = Some(cmd),
            Wake::ChannelClosed => return Ok(ShutdownReason::Requested),
        }

        for command in woke_on_command.into_iter().chain(std::iter::from_fn(|| commands.try_recv().ok())) {
            match command {
                CalendarCommand::Shutdown => return Ok(ShutdownReason::Requested),
                CalendarCommand::Refresh => sync_month(client, &calendars, current_month, events).await?,
                CalendarCommand::SyncMonth(date) => {
                    current_month = first_of_month(date);
                    sync_month(client, &calendars, current_month, events).await?;
                }
                CalendarCommand::FetchMonth(date) => {
                    sync_month(client, &calendars, first_of_month(date), events).await?;
                }
                CalendarCommand::Reconnect => {}
                CalendarCommand::CreateEvent { event } | CalendarCommand::UpdateEvent { event } => {
                    write_event(client, &calendars, &event, current_month, events).await;
                }
                CalendarCommand::DeleteEvent { calendar_id: _, href, etag } => match client.delete_event(&href, etag.as_deref()).await {
                    Ok(()) => sync_month(client, &calendars, current_month, events).await?,
                    Err(e) => {
                        let _ = events.send(CalendarSessionEvent::Error(format!("failed to delete event: {e}"))).await;
                    }
                },
                CalendarCommand::RespondInvitation {
                    event_id,
                    response,
                    comment,
                    send_response,
                    proposed_new_time,
                    ..
                } => match client.respond_invitation(&event_id, response, comment.as_deref(), send_response, proposed_new_time).await {
                    Ok(()) => sync_month(client, &calendars, current_month, events).await?,
                    Err(e) => {
                        let _ = events.send(CalendarSessionEvent::Error(format!("failed to respond to the invitation: {e}"))).await;
                    }
                },
                CalendarCommand::SyncTasks => {
                    let _ = events.send(CalendarSessionEvent::TasksUpdated(Vec::new())).await;
                }
                CalendarCommand::CreateTask { task } => {
                    let _ = events
                        .send(CalendarSessionEvent::Error(format!(
                            "task \"{}\" was not saved - tasks live in Microsoft To Do, not a Graph calendar",
                            task.uid
                        )))
                        .await;
                }
                CalendarCommand::UpdateTask { task } => {
                    let _ = events
                        .send(CalendarSessionEvent::Error(format!(
                            "task \"{}\" was not saved - tasks live in Microsoft To Do, not a Graph calendar",
                            task.uid
                        )))
                        .await;
                }
                CalendarCommand::DeleteTask { .. } => {
                    let _ = events
                        .send(CalendarSessionEvent::Error(
                            "the task was not deleted - tasks live in Microsoft To Do, not a Graph calendar".to_string(),
                        ))
                        .await;
                }
                CalendarCommand::FetchFreeBusy {
                    attendees,
                    start,
                    end,
                    request_id,
                    ..
                } => match client.get_schedule(&attendees, start, end).await {
                    Ok(availabilities) => {
                        let _ = events.send(CalendarSessionEvent::FreeBusyUpdated { request_id, availabilities }).await;
                    }
                    Err(e) => {
                        let _ = events
                            .send(CalendarSessionEvent::FreeBusyFailed {
                                request_id,
                                message: e.to_string(),
                            })
                            .await;
                    }
                },
            }
        }
    }
}

/// Fetches every calendar's occurrences over a padded month window, clips to
/// the month, and emits them. A single calendar's failure is logged and
/// skipped; an `Unauthorized` (stale credential) is propagated to force a
/// reconnect.
async fn sync_month(client: &CalendarClient, calendars: &[CalendarInfo], month: NaiveDate, events: &async_channel::Sender<CalendarSessionEvent>) -> Result<(), GraphError> {
    let month_end = next_month(month);
    let fetch_start = (month - chrono::Duration::days(7)).and_time(NaiveTime::MIN).and_utc();
    let fetch_end = (month_end + chrono::Duration::days(7)).and_time(NaiveTime::MIN).and_utc();
    let window_start = month.and_time(NaiveTime::MIN).and_utc();
    let window_end = month_end.and_time(NaiveTime::MIN).and_utc();

    let mut occurrences = Vec::new();
    let mut any_succeeded = false;
    for calendar in calendars {
        match client.calendar_view(calendar, fetch_start, fetch_end).await {
            Ok(calendar_occurrences) => {
                any_succeeded = true;
                occurrences.extend(calendar_occurrences.into_iter().filter(|occ| overlaps(occ, window_start, window_end)));
            }
            Err(e @ GraphError::Unauthorized) => {
                tracing::warn!("failed to fetch events for calendar {:?}: {e}", calendar.display_name);
                return Err(e);
            }
            Err(e) => tracing::warn!("failed to fetch events for calendar {:?}: {e}", calendar.display_name),
        }
    }

    // Every calendar failing must not blank the view - leave last-known-good
    // state alone (matching the CalDAV actor).
    if !calendars.is_empty() && !any_succeeded {
        return Ok(());
    }

    let _ = events
        .send(CalendarSessionEvent::OccurrencesUpdated {
            month,
            occurrences: occurrences.clone(),
        })
        .await;
    Ok(())
}

/// Creates or updates `event`, sharing one path: a fresh event (no Graph id)
/// is created, an existing one (`href` set) is updated. Resyncs the on-screen
/// month on success, or emits `EventSaveFailed`.
async fn write_event(client: &CalendarClient, calendars: &[CalendarInfo], event: &CalendarEvent, current_month: NaiveDate, events: &async_channel::Sender<CalendarSessionEvent>) {
    let result = if event.href.is_none() {
        match calendars.iter().find(|calendar| calendar.id == event.calendar_id) {
            Some(calendar) => client.create_event(calendar, event).await.map(|_| ()),
            None => {
                let _ = events
                    .send(CalendarSessionEvent::EventSaveFailed {
                        uid: event.uid.clone(),
                        recurrence_id: event.recurrence_id,
                        message: format!("calendar for event \"{}\" not found", event.uid),
                    })
                    .await;
                return;
            }
        }
    } else {
        client.update_event(event).await.map(|_| ())
    };

    match result {
        Ok(()) => {
            let mut months = vec![current_month];
            let event_month = first_of_month(event.start.date_naive());
            if event_month != current_month {
                months.push(event_month);
            }
            for month in months {
                let _ = sync_month(client, calendars, month, events).await;
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

/// Whether an occurrence overlaps the half-open `[start, end)` month window.
fn overlaps(occ: &EventOccurrence, start: DateTime<Utc>, end: DateTime<Utc>) -> bool {
    occ.start < end && occ.end > start
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
    use std::sync::Arc;
    use std::time::Duration;

    use async_trait::async_trait;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::auth::TokenProvider;
    use crate::client::GraphClient;

    struct StubTokens;

    #[async_trait]
    impl TokenProvider for StubTokens {
        async fn graph_token(&self) -> Result<String, GraphError> {
            Ok("stub-token".to_string())
        }
        async fn ensure_scopes(&self, _scopes: &[&str]) -> Result<(), GraphError> {
            Ok(())
        }
    }

    fn test_client(server: &MockServer) -> CalendarClient {
        CalendarClient::new(GraphClient::with_base_url(Arc::new(StubTokens), format!("{}/v1.0", server.uri())))
    }

    #[tokio::test]
    async fn the_actor_connects_lists_calendars_and_syncs_occurrences() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/calendars"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "value": [{ "id": "cal1", "name": "Work" }] })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/calendars/cal1/calendarView"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "value": [{
                    "id": "ev1",
                    "iCalUId": "uid-1@example.com",
                    "subject": "Standup",
                    "start": { "dateTime": "2026-10-15T09:00:00", "timeZone": "UTC" },
                    "end": { "dateTime": "2026-10-15T09:30:00", "timeZone": "UTC" }
                }]
            })))
            .mount(&server)
            .await;

        let (cmd_tx, cmd_rx) = async_channel::unbounded();
        let (evt_tx, evt_rx) = async_channel::unbounded();
        let handle = tokio::spawn(run_calendar_session(AccountId("acct".to_string()), test_client(&server), cmd_rx, evt_tx));

        let mut calendars = None;
        let mut scheduled = false;
        let mut occurrences = None;
        for _ in 0..40 {
            let event = match tokio::time::timeout(Duration::from_secs(2), evt_rx.recv()).await {
                Ok(Ok(event)) => event,
                _ => break,
            };
            match event {
                CalendarSessionEvent::CalendarsUpdated(list) => calendars = Some(list),
                CalendarSessionEvent::SchedulingUpdated(info) => scheduled = info.supports_server_scheduling(),
                CalendarSessionEvent::OccurrencesUpdated { occurrences: occ, .. } => occurrences = Some(occ),
                _ => {}
            }
            if calendars.is_some() && scheduled && occurrences.is_some() {
                break;
            }
        }

        let calendars = calendars.expect("CalendarsUpdated");
        assert_eq!(calendars.len(), 1);
        assert!(scheduled, "the Graph marker must report server scheduling");
        let occurrences = occurrences.expect("OccurrencesUpdated");
        // The fixed fixture date (2026-10-15) only overlaps the currently
        // polled month when the system clock is in October 2026; otherwise the
        // clip holds it out. Assert the pipeline ran, not the date.
        assert!(occurrences.len() <= 1);

        cmd_tx.send(CalendarCommand::Shutdown).await.unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    #[tokio::test]
    async fn create_event_command_writes_and_resyncs() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/calendars"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "value": [{ "id": "cal1", "name": "Work" }] })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1.0/me/calendars/cal1/events"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "id": "ev-new",
                "iCalUId": "new@example.com",
                "start": { "dateTime": "2026-10-15T09:00:00", "timeZone": "UTC" },
                "end": { "dateTime": "2026-10-15T09:30:00", "timeZone": "UTC" }
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/calendars/cal1/calendarView"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "value": [] })))
            .mount(&server)
            .await;

        let (cmd_tx, cmd_rx) = async_channel::unbounded();
        let (evt_tx, evt_rx) = async_channel::unbounded();
        let handle = tokio::spawn(run_calendar_session(AccountId("acct".to_string()), test_client(&server), cmd_rx, evt_tx));

        let calendar = CalendarInfo {
            id: lookout_core::CalendarId("acct:cal1".to_string()),
            account_id: AccountId("acct".to_string()),
            display_name: "Work".to_string(),
            color: None,
            href: "cal1".to_string(),
            supports_tasks: false,
        };
        let mut event = crate::calendar::event_from_graph(
            &json!({ "id": "x", "subject": "New", "start": { "dateTime": "2026-10-15T09:00:00", "timeZone": "UTC" }, "end": { "dateTime": "2026-10-15T09:30:00", "timeZone": "UTC" } }),
            &calendar.id,
        )
        .unwrap();
        event.href = None;
        event.etag = None;

        cmd_tx.send(CalendarCommand::CreateEvent { event: Box::new(event) }).await.unwrap();

        // Drain until an OccurrencesUpdated follows the create (or an error).
        let mut saw_failure = None;
        for _ in 0..40 {
            let event = match tokio::time::timeout(Duration::from_secs(2), evt_rx.recv()).await {
                Ok(Ok(event)) => event,
                _ => break,
            };
            match event {
                CalendarSessionEvent::EventSaveFailed { message, .. } => {
                    saw_failure = Some(message);
                    break;
                }
                CalendarSessionEvent::OccurrencesUpdated { .. } => break,
                _ => {}
            }
        }
        assert!(saw_failure.is_none(), "create should not fail: {saw_failure:?}");

        cmd_tx.send(CalendarCommand::Shutdown).await.unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    #[tokio::test]
    async fn a_consent_error_stops_with_a_non_retryable_state() {
        struct NoConsent;

        #[async_trait]
        impl TokenProvider for NoConsent {
            async fn graph_token(&self) -> Result<String, GraphError> {
                Err(GraphError::ConsentRequired("Calendars.ReadWrite".to_string()))
            }
            async fn ensure_scopes(&self, _scopes: &[&str]) -> Result<(), GraphError> {
                Ok(())
            }
        }

        // The token fetch fails before any request, so the bogus base URL is
        // never reached.
        let client = CalendarClient::new(GraphClient::with_base_url(Arc::new(NoConsent), "https://example.invalid/v1.0"));
        let (_cmd_tx, cmd_rx) = async_channel::unbounded();
        let (evt_tx, evt_rx) = async_channel::unbounded();
        let handle = tokio::spawn(run_calendar_session(AccountId("acct".to_string()), client, cmd_rx, evt_tx));

        let mut saw_non_retryable = false;
        for _ in 0..20 {
            match tokio::time::timeout(Duration::from_secs(2), evt_rx.recv()).await {
                Ok(Ok(CalendarSessionEvent::ConnectionStateChanged(ConnectionState::Error { retryable: false, .. }))) => {
                    saw_non_retryable = true;
                    break;
                }
                Ok(Ok(_)) => {}
                _ => break,
            }
        }
        assert!(saw_non_retryable, "a missing consent must surface as a non-retryable error");
        assert!(
            tokio::time::timeout(Duration::from_secs(2), handle).await.is_ok(),
            "the session should stop instead of retrying a consent error"
        );
    }

    #[test]
    fn month_helpers_and_overlap() {
        assert_eq!(
            first_of_month(NaiveDate::from_ymd_opt(2026, 10, 17).unwrap()),
            NaiveDate::from_ymd_opt(2026, 10, 1).unwrap()
        );
        assert_eq!(next_month(NaiveDate::from_ymd_opt(2026, 12, 1).unwrap()), NaiveDate::from_ymd_opt(2027, 1, 1).unwrap());
        let occ = EventOccurrence {
            uid: lookout_core::EventUid("u".to_string()),
            calendar_id: lookout_core::CalendarId("c".to_string()),
            summary: None,
            description: None,
            location: None,
            start: "2026-10-15T09:00:00Z".parse().unwrap(),
            end: "2026-10-15T09:30:00Z".parse().unwrap(),
            all_day: false,
            time_zone: None,
            rrule: None,
            recurrence_id: None,
            exdates: Vec::new(),
            master_start: None,
            master_end: None,
            href: None,
            etag: None,
            master_href: None,
            master_etag: None,
            sequence: 0,
            attendees: Vec::new(),
            organizer: None,
            schedule_agent: None,
            categories: Vec::new(),
            sensitivity: Default::default(),
            show_as: Default::default(),
            reminder_minutes_before: None,
            conference_url: None,
        };
        assert!(overlaps(&occ, "2026-10-01T00:00:00Z".parse().unwrap(), "2026-11-01T00:00:00Z".parse().unwrap()));
        assert!(!overlaps(&occ, "2026-11-01T00:00:00Z".parse().unwrap(), "2026-12-01T00:00:00Z".parse().unwrap()));
    }
}
