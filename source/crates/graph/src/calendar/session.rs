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
                Ok(CalendarCommand::FetchEventAttachment { request_id, .. }) => {
                    let _ = events
                        .send(CalendarSessionEvent::EventAttachmentFetchFailed {
                            request_id,
                            message: "not connected - the attachment could not be downloaded".to_string(),
                        })
                        .await;
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
    let mut calendars = client.list_calendars(account_id, None).await?;
    // The other users' mailboxes whose calendars this account also shows
    // (item 5.6 / CAL-25), from `CalendarCommand::SetSharedCalendars`.
    let mut shared_owners: Vec<String> = Vec::new();
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
                CalendarCommand::Refresh => {
                    calendars = rebuild_calendars(client, account_id, &shared_owners, events).await?;
                    sync_month(client, &calendars, current_month, events).await?;
                }
                CalendarCommand::SyncMonth(date) => {
                    current_month = first_of_month(date);
                    sync_month(client, &calendars, current_month, events).await?;
                }
                CalendarCommand::FetchMonth(date) => {
                    sync_month(client, &calendars, first_of_month(date), events).await?;
                }
                CalendarCommand::Reconnect => {}
                CalendarCommand::SetSharedCalendars { owners } => {
                    shared_owners = owners;
                    calendars = rebuild_calendars(client, account_id, &shared_owners, events).await?;
                    sync_month(client, &calendars, current_month, events).await?;
                }
                CalendarCommand::CreateEvent { event } | CalendarCommand::UpdateEvent { event } => {
                    write_event(client, &calendars, &event, current_month, events).await;
                }
                CalendarCommand::DeleteEvent { calendar_id, href, etag } => {
                    let owner = owner_of(&calendars, &calendar_id);
                    match client.delete_event(&href, etag.as_deref(), owner.as_deref()).await {
                        Ok(()) => sync_month(client, &calendars, current_month, events).await?,
                        Err(e) => {
                            let _ = events.send(CalendarSessionEvent::Error(format!("failed to delete event: {e}"))).await;
                        }
                    }
                }
                CalendarCommand::RespondInvitation {
                    calendar_id,
                    event_id,
                    response,
                    comment,
                    send_response,
                    proposed_new_time,
                } => {
                    let owner = owner_of(&calendars, &calendar_id);
                    match client
                        .respond_invitation(&event_id, owner.as_deref(), response, comment.as_deref(), send_response, proposed_new_time)
                        .await
                    {
                        Ok(()) => sync_month(client, &calendars, current_month, events).await?,
                        Err(e) => {
                            let _ = events.send(CalendarSessionEvent::Error(format!("failed to respond to the invitation: {e}"))).await;
                        }
                    }
                }
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
                // Task lists on Microsoft 365 live in To Do, which has its own
                // actor (`lookout_graph::todo`); the Graph calendar actor never
                // creates or deletes task lists (item 4.8 / T-01). A rename,
                // though, comes from the Calendar sidebar's row menu (item
                // 3.13) - To Do list renames route to the To Do actor - so it
                // renames the calendar itself.
                CalendarCommand::CreateCalendar { .. } | CalendarCommand::DeleteCalendar { .. } => {
                    let _ = events
                        .send(CalendarSessionEvent::Error("task lists live in Microsoft To Do, not a Graph calendar".to_string()))
                        .await;
                }
                CalendarCommand::UpdateCalendar { calendar_id, display_name } => match calendars.iter().find(|c| c.id == calendar_id) {
                    Some(calendar) => match client.rename_calendar(calendar, &display_name).await {
                        Ok(()) => calendars = rebuild_calendars(client, account_id, &shared_owners, events).await?,
                        Err(e) => {
                            let _ = events.send(CalendarSessionEvent::Error(format!("couldn't rename the calendar: {e}"))).await;
                        }
                    },
                    None => {
                        let _ = events
                            .send(CalendarSessionEvent::Error("couldn't rename the calendar: it no longer exists".to_string()))
                            .await;
                    }
                },
                // Colours stay on the device for Microsoft 365 calendars
                // (item 3.13 / CAL-32): Graph's calendar `hexColor` is
                // read-only, so there is nothing to write. The app has already
                // applied the picked colour locally.
                CalendarCommand::SetCalendarColor { .. } => {}
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
                CalendarCommand::FetchEventAttachment {
                    calendar_id,
                    event_href,
                    attachment,
                    request_id,
                } => {
                    let owner = owner_of(&calendars, &calendar_id);
                    fetch_event_attachment(client, owner.as_deref(), event_href.as_deref(), &attachment, request_id, events).await;
                }
            }
        }
    }
}

/// Re-lists the account's own calendars plus every configured shared owner's
/// (item 5.6 / CAL-25), emits the merged set and returns it. A shared owner
/// that fails to resolve is logged and skipped, so one bad address can't blank
/// the whole calendar list; a stale credential still forces a reconnect.
async fn rebuild_calendars(
    client: &CalendarClient,
    account_id: &AccountId,
    owners: &[String],
    events: &async_channel::Sender<CalendarSessionEvent>,
) -> Result<Vec<CalendarInfo>, GraphError> {
    let mut calendars = client.list_calendars(account_id, None).await?;
    for owner in owners {
        match client.list_calendars(account_id, Some(owner)).await {
            Ok(mut shared) => {
                for calendar in &mut shared {
                    // Label the row with the other user so the sidebar
                    // distinguishes their calendar from the user's own.
                    calendar.display_name = format!("{owner} · {}", calendar.display_name);
                }
                calendars.append(&mut shared);
            }
            Err(e @ GraphError::Unauthorized) => return Err(e),
            Err(e) => tracing::warn!("failed to list shared calendars for {owner}: {e}"),
        }
    }
    let _ = events.send(CalendarSessionEvent::CalendarsUpdated(calendars.clone())).await;
    Ok(calendars)
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
///
/// Attachments (CAL-21) are Graph child resources, not part of the event
/// body: after the event has an id, each newly attached file is POSTed to
/// `/me/events/{id}/attachments`, and an update removes the child resources
/// the user deleted from the form. A failure is reported after the event
/// itself was saved, so the meeting is never rolled back over a file.
async fn write_event(client: &CalendarClient, calendars: &[CalendarInfo], event: &CalendarEvent, current_month: NaiveDate, events: &async_channel::Sender<CalendarSessionEvent>) {
    let owner = owner_of(calendars, &event.calendar_id);
    let result = if event.href.is_none() {
        match calendars.iter().find(|calendar| calendar.id == event.calendar_id) {
            Some(calendar) => client.create_event(calendar, event).await,
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
        client.update_event(event, owner.as_deref()).await
    };

    match result {
        Ok(saved) => {
            if let Some(event_id) = saved.href.as_deref() {
                sync_attachments(client, owner.as_deref(), event_id, event, events).await;
            }
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

/// Reconciles an event's Graph attachment children with the form's list
/// (CAL-21): delete every server attachment the form dropped, then upload
/// every inline attachment that has no server id yet. Failures are reported
/// as toasts; the event write itself has already succeeded.
async fn sync_attachments(client: &CalendarClient, owner: Option<&str>, event_id: &str, event: &CalendarEvent, events: &async_channel::Sender<CalendarSessionEvent>) {
    let wanted: std::collections::HashSet<&str> = event.attachments.iter().filter_map(|attachment| attachment.graph_id()).collect();
    match client.list_attachments(event_id, owner).await {
        Ok(existing) => {
            for attachment in existing {
                let Some(id) = attachment.graph_id() else { continue };
                if !wanted.contains(id) {
                    if let Err(e) = client.delete_attachment(event_id, owner, id).await {
                        tracing::warn!("failed to remove Graph event attachment {id}: {e}");
                    }
                }
            }
        }
        Err(e) => tracing::warn!("couldn't list Graph event attachments for reconciliation: {e}"),
    }
    for attachment in event.attachments.iter().filter(|attachment| attachment.is_inline() && attachment.graph_id().is_none()) {
        if let Err(e) = client.add_attachment(event_id, owner, attachment).await {
            let _ = events
                .send(CalendarSessionEvent::Error(format!(
                    "event \"{}\" was saved, but \"{}\" couldn't be attached: {e}",
                    event.uid, attachment.name
                )))
                .await;
        }
    }
}

/// Answers a `FetchEventAttachment` (CAL-21): inline contents decode straight
/// from the document; a Graph attachment id is read through
/// `GET /me/events/{id}/attachments/{id}`.
async fn fetch_event_attachment(
    client: &CalendarClient,
    owner: Option<&str>,
    event_id: Option<&str>,
    attachment: &lookout_core::EventAttachment,
    request_id: u64,
    events: &async_channel::Sender<CalendarSessionEvent>,
) {
    let fetched = if let Some(data) = attachment.bytes() {
        Some(Ok(data))
    } else if let (Some(event_id), Some(attachment_id)) = (event_id, attachment.graph_id()) {
        Some(client.fetch_attachment(event_id, owner, attachment_id).await)
    } else {
        None
    };
    match fetched {
        Some(Ok(data)) => {
            let _ = events
                .send(CalendarSessionEvent::EventAttachmentFetched {
                    request_id,
                    name: attachment.name.clone(),
                    data,
                })
                .await;
        }
        Some(Err(e)) => {
            let _ = events
                .send(CalendarSessionEvent::EventAttachmentFetchFailed {
                    request_id,
                    message: format!("couldn't download \"{}\": {e}", attachment.name),
                })
                .await;
        }
        None => {
            let _ = events
                .send(CalendarSessionEvent::EventAttachmentFetchFailed {
                    request_id,
                    message: format!("attachment \"{}\" has no location", attachment.name),
                })
                .await;
        }
    }
}

/// Whether an occurrence overlaps the half-open `[start, end)` month window.
fn overlaps(occ: &EventOccurrence, start: DateTime<Utc>, end: DateTime<Utc>) -> bool {
    occ.start < end && occ.end > start
}

/// The Graph owner (another user's id/address) of the calendar with `id`, for
/// a shared/delegated calendar (item 5.6 / CAL-25); `None` for the user's own.
fn owner_of(calendars: &[CalendarInfo], id: &lookout_core::CalendarId) -> Option<String> {
    calendars.iter().find(|calendar| &calendar.id == id).and_then(|calendar| calendar.owner.clone())
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
            owner: None,
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
    async fn create_event_uploads_its_attachments() {
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
        // No existing children, then the upload.
        Mock::given(method("GET"))
            .and(path("/v1.0/me/events/ev-new/attachments"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "value": [] })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1.0/me/events/ev-new/attachments"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": "att-1", "name": "notes.txt" })))
            .expect(1)
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

        let calendar_id = lookout_core::CalendarId("acct:cal1".to_string());
        let mut event = crate::calendar::event_from_graph(
            &json!({ "id": "x", "subject": "New", "start": { "dateTime": "2026-10-15T09:00:00", "timeZone": "UTC" }, "end": { "dateTime": "2026-10-15T09:30:00", "timeZone": "UTC" } }),
            &calendar_id,
        )
        .unwrap();
        event.href = None;
        event.etag = None;
        event.attachments = vec![lookout_core::EventAttachment::from_bytes("notes.txt", Some("text/plain".to_string()), b"Hello agenda!").unwrap()];
        cmd_tx.send(CalendarCommand::CreateEvent { event: Box::new(event) }).await.unwrap();

        let mut failed = None;
        for _ in 0..40 {
            let event = match tokio::time::timeout(Duration::from_secs(2), evt_rx.recv()).await {
                Ok(Ok(event)) => event,
                _ => break,
            };
            match event {
                CalendarSessionEvent::EventSaveFailed { message, .. } => {
                    failed = Some(message);
                    break;
                }
                CalendarSessionEvent::OccurrencesUpdated { .. } => break,
                _ => {}
            }
        }
        assert!(failed.is_none(), "create should not fail: {failed:?}");

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

    #[tokio::test]
    async fn set_shared_calendars_lists_the_owners_and_tags_them() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/calendars"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "value": [{ "id": "own1", "name": "Calendar" }] })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1.0/users/ada%40example.com/calendars"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "value": [{ "id": "shared1", "name": "Ada's Calendar" }] })))
            .mount(&server)
            .await;
        // The month sync that follows the rebuild.
        Mock::given(method("GET"))
            .and(path("/v1.0/me/calendars/own1/calendarView"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "value": [] })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1.0/users/ada%40example.com/calendars/shared1/calendarView"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "value": [] })))
            .mount(&server)
            .await;

        let (cmd_tx, cmd_rx) = async_channel::unbounded();
        let (evt_tx, evt_rx) = async_channel::unbounded();
        let handle = tokio::spawn(run_calendar_session(AccountId("acct".to_string()), test_client(&server), cmd_rx, evt_tx));

        // Wait for the initial list, then ask for the shared owner.
        let mut first = None;
        for _ in 0..20 {
            match tokio::time::timeout(Duration::from_secs(2), evt_rx.recv()).await {
                Ok(Ok(CalendarSessionEvent::CalendarsUpdated(list))) => {
                    first = Some(list);
                    break;
                }
                Ok(Ok(_)) => {}
                _ => break,
            }
        }
        assert_eq!(first.expect("initial CalendarsUpdated").len(), 1);

        cmd_tx
            .send(CalendarCommand::SetSharedCalendars {
                owners: vec!["ada@example.com".to_string()],
            })
            .await
            .unwrap();

        let mut merged = None;
        for _ in 0..40 {
            match tokio::time::timeout(Duration::from_secs(2), evt_rx.recv()).await {
                Ok(Ok(CalendarSessionEvent::CalendarsUpdated(list))) if list.len() == 2 => {
                    merged = Some(list);
                    break;
                }
                Ok(Ok(_)) => {}
                _ => break,
            }
        }
        let merged = merged.expect("merged CalendarsUpdated");
        let shared = merged.iter().find(|c| c.owner.is_some()).expect("a shared calendar");
        assert_eq!(shared.owner.as_deref(), Some("ada@example.com"));
        assert_eq!(shared.display_name, "ada@example.com · Ada's Calendar");

        cmd_tx.send(CalendarCommand::Shutdown).await.unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
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
            status: Default::default(),
            reminder_minutes_before: None,
            conference_url: None,
            attachments: Vec::new(),
        };
        assert!(overlaps(&occ, "2026-10-01T00:00:00Z".parse().unwrap(), "2026-11-01T00:00:00Z".parse().unwrap()));
        assert!(!overlaps(&occ, "2026-11-01T00:00:00Z".parse().unwrap(), "2026-12-01T00:00:00Z".parse().unwrap()));
    }
}
