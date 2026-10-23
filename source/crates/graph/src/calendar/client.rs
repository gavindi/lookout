//! The Microsoft Graph calendar transport: the HTTP operations the calendar
//! session drives, layered on [`GraphClient`] (OUTLOOK_PARITY.md §5.1, items
//! 6–8 / A-03).
//!
//! Every method returns `lookout_core` types through the pure mapping in the
//! parent module, so the session never touches Graph JSON. `@odata.nextLink`
//! paging is handled by [`GraphClient::get_all`] (and [`GraphClient::delta`]);
//! an incremental sync passes a previously returned delta link to
//! [`CalendarClient::calendar_view_delta`], whose `DeltaPage::delta_link` the
//! session persists.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use base64::Engine as _;
use chrono::{DateTime, Utc};
use lookout_core::calendar_protocol::{InvitationResponse, ProposedTime};
use lookout_core::{AccountId, AttendeeAvailability, BusyStatus, CalendarEvent, CalendarInfo, EmailAddress, EventOccurrence, FreeBusyInterval};
use serde_json::{json, Value};

use crate::client::{DeltaPage, GraphClient};
use crate::error::{GraphError, Result};

use super::{
    attachment_from_graph, calendar_from_graph, event_create_body, event_from_graph, event_patch_body, graph_date_time, occurrence_from_graph, parse_graph_date_time,
    CalendarPermission, CalendarRole,
};

/// One room from the tenant directory (`/places/microsoft.graph.room`,
/// item 15 / CAL-19).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Room {
    pub display_name: String,
    pub address: String,
}

/// The `availabilityViewInterval` (minutes) Graph quantises free/busy to. The
/// app ignores the `availabilityView` string and uses the exact
/// `scheduleItems`, so this only shapes Graph's response.
const AVAILABILITY_INTERVAL_MINUTES: u32 = 30;

/// Most event instances read for one calendar-view window - a guard against an
/// unbounded recurring series.
const MAX_EVENT_INSTANCES: usize = 5000;
/// Most calendars read for one account.
const MAX_CALENDARS: usize = 500;
/// Most rooms read from the tenant directory.
const MAX_ROOMS: usize = 1000;
/// Most calendar-permission grants read for one calendar (`calendarPermissions`
/// is small in practice). Item 5.6 / CAL-24.
const MAX_PERMISSIONS: usize = 500;
/// Most attachments read for one event (Graph caps an event below this in
/// practice; a guard against an unexpected page loop). Item 3.16 / CAL-21.
const MAX_EVENT_ATTACHMENTS: usize = 500;

/// The `$select` fields the calendar-view read needs (exactly what the mapper
/// consumes, so full bodies aren't fetched).
const EVENT_SELECT: &str = "$select=id,changeKey,iCalUId,subject,body,start,end,isAllDay,location,showAs,sensitivity,categories,isCancelled,isReminderOn,reminderMinutesBeforeStart,organizer,attendees,onlineMeeting,onlineMeetingUrl";
/// Attachment metadata (never `contentBytes`, which only a single-attachment
/// read returns) expanded onto every calendar-view event (CAL-21).
const EVENT_EXPAND: &str = "&$expand=attachments($select=id,name,contentType,size,isInline)";

/// The calendar HTTP operations, over a shared [`GraphClient`].
pub struct CalendarClient {
    graph: GraphClient,
}

impl CalendarClient {
    pub fn new(graph: GraphClient) -> Self {
        CalendarClient { graph }
    }

    /// The calendars under `owner`'s mailbox: `/me/calendars` for the user's
    /// own, or `/users/{owner}/calendars` for a shared/delegated calendar
    /// (item 5.6 / CAL-25). `owner` is a Graph user id or address, or `None`
    /// for the signed-in user.
    pub async fn list_calendars(&self, account_id: &AccountId, owner: Option<&str>) -> Result<Vec<CalendarInfo>> {
        let path = format!("{}?$top=100&$select=id,name,hexColor,isDefaultCalendar", self.calendars_root(owner));
        let items = self.graph.get_all(&path, MAX_CALENDARS).await?;
        Ok(items.iter().filter_map(|value| calendar_from_graph(value, account_id, owner)).collect())
    }

    /// Renames `calendar` (the row menu's Rename action, item 3.13): a `PATCH`
    /// of the calendar's `name` at `/me/calendars/{id}` (or
    /// `/users/{owner}/calendars/{id}` for a shared/delegated calendar, taken
    /// from `CalendarInfo::owner`). Graph's calendar `hexColor` is read-only,
    /// so a colour pick on Microsoft 365 stays on the device - see
    /// [`lookout_core::calendar_protocol::CalendarCommand::SetCalendarColor`].
    pub async fn rename_calendar(&self, calendar: &CalendarInfo, name: &str) -> Result<()> {
        let path = format!("{}/{}", self.calendars_root(calendar.owner.as_deref()), encode_segment(&calendar.href));
        self.graph.patch(&path, &serde_json::json!({ "name": name })).await?;
        Ok(())
    }

    /// The expanded occurrences of `calendar` over `[start, end)`
    /// (`/me/calendars/{id}/calendarView`). Graph already expands recurrences
    /// for this endpoint, so the result is render-ready.
    pub async fn calendar_view(&self, calendar: &CalendarInfo, start: DateTime<Utc>, end: DateTime<Utc>) -> Result<Vec<EventOccurrence>> {
        let items = self.graph.get_all(&self.view_path(calendar, start, end), MAX_EVENT_INSTANCES).await?;
        Ok(items.iter().filter_map(|value| occurrence_from_graph(value, &calendar.id)).collect())
    }

    /// An incremental calendar-view sync: pass `None` for the initial read
    /// over `[start, end)`, or a previously returned [`DeltaPage::delta_link`]
    /// for the next one. The returned link is the caller's to persist.
    pub async fn calendar_view_delta(&self, calendar: &CalendarInfo, start: DateTime<Utc>, end: DateTime<Utc>, delta_link: Option<&str>) -> Result<DeltaPage> {
        self.graph.delta(&self.view_path_delta(calendar, start, end), delta_link).await
    }

    /// Creates `event` in `calendar` and returns the server's copy (with its
    /// Graph id/`changeKey`).
    pub async fn create_event(&self, calendar: &CalendarInfo, event: &CalendarEvent) -> Result<CalendarEvent> {
        ensure_recurrence_supported(event)?;
        let path = format!("{}/{}/events", self.calendars_root(calendar.owner.as_deref()), encode_segment(&calendar.href));
        let created = self.graph.post(&path, &event_create_body(event)).await?;
        event_from_graph(&created, &calendar.id).ok_or_else(malformed_event)
    }

    /// Updates `event` in place, guarded by its `changeKey` (`If-Match`) so a
    /// concurrent change fails loudly (HTTP 412) rather than being clobbered.
    /// `Prefer: return=representation` asks Graph for the updated resource so
    /// its new `changeKey` is available; if the server omits the body, the
    /// input event is returned unchanged (the session resyncs after a write).
    /// `owner` is the event's calendar owner for a shared/delegated calendar
    /// (item 5.6 / CAL-25), or `None` for the user's own.
    pub async fn update_event(&self, event: &CalendarEvent, owner: Option<&str>) -> Result<CalendarEvent> {
        ensure_recurrence_supported(event)?;
        let id = event.href.as_deref().ok_or_else(missing_event_id)?;
        let path = format!("{}/{}", self.events_root(owner), encode_segment(id));
        let mut headers: Vec<(&str, &str)> = vec![("Prefer", "return=representation")];
        if let Some(etag) = event.etag.as_deref() {
            headers.push(("If-Match", etag));
        }
        let updated = self.graph.patch_with_headers(&path, &event_patch_body(event), &headers).await?;
        Ok(event_from_graph(&updated, &event.calendar_id).unwrap_or_else(|| event.clone()))
    }

    /// Deletes the event with Graph `id`, guarded by `etag` (`If-Match`) when
    /// known. `owner` selects a shared/delegated calendar's mailbox
    /// (item 5.6 / CAL-25), or `None` for the user's own.
    pub async fn delete_event(&self, id: &str, etag: Option<&str>, owner: Option<&str>) -> Result<()> {
        let path = format!("{}/{}", self.events_root(owner), encode_segment(id));
        let headers: Vec<(&str, &str)> = etag.map(|etag| vec![("If-Match", etag)]).unwrap_or_default();
        self.graph.delete_with_headers(&path, &headers).await?;
        Ok(())
    }

    /// The event's attachments (`GET /me/events/{id}/attachments`, CAL-21).
    /// Metadata only - Graph returns `contentBytes` only from
    /// [`Self::fetch_attachment`].
    pub async fn list_attachments(&self, event_id: &str, owner: Option<&str>) -> Result<Vec<lookout_core::EventAttachment>> {
        let path = format!(
            "{}/{}/attachments?$select=id,name,contentType,size,isInline",
            self.events_root(owner),
            encode_segment(event_id)
        );
        let items = self.graph.get_all(&path, MAX_EVENT_ATTACHMENTS).await?;
        Ok(items.iter().filter_map(attachment_from_graph).collect())
    }

    /// Adds one file attachment to an existing event (`POST /me/events/{id}/
    /// attachments`, CAL-21). Graph caps a single file attachment at 3 MB;
    /// larger files surface Graph's own error (degrade softly, §10).
    pub async fn add_attachment(&self, event_id: &str, owner: Option<&str>, attachment: &lookout_core::EventAttachment) -> Result<()> {
        let bytes = attachment.bytes().ok_or_else(|| GraphError::Status {
            status: 0,
            message: format!("attachment \"{}\" has no readable contents", attachment.name),
            request_id: None,
        })?;
        let path = format!("{}/{}/attachments", self.events_root(owner), encode_segment(event_id));
        let body = json!({
            "@odata.type": "#microsoft.graph.fileAttachment",
            "name": attachment.name,
            "contentType": attachment.mime_type.clone().unwrap_or_else(|| "application/octet-stream".to_string()),
            "contentBytes": base64::engine::general_purpose::STANDARD.encode(bytes),
        });
        self.graph.post(&path, &body).await?;
        Ok(())
    }

    /// Removes an attachment (`DELETE /me/events/{id}/attachments/{id}`).
    pub async fn delete_attachment(&self, event_id: &str, owner: Option<&str>, attachment_id: &str) -> Result<()> {
        let path = format!("{}/{}/attachments/{}", self.events_root(owner), encode_segment(event_id), encode_segment(attachment_id));
        self.graph.delete(&path).await.map(|_| ())
    }

    /// Downloads one attachment's bytes (`GET /me/events/{id}/attachments/
    /// {id}` → `contentBytes`). Used by `CalendarCommand::FetchEventAttachment`
    /// when the editor or the invitation card saves a file.
    pub async fn fetch_attachment(&self, event_id: &str, owner: Option<&str>, attachment_id: &str) -> Result<Vec<u8>> {
        let path = format!("{}/{}/attachments/{}", self.events_root(owner), encode_segment(event_id), encode_segment(attachment_id));
        let value = self.graph.get(&path).await?;
        let encoded = value.get("contentBytes").and_then(Value::as_str).ok_or_else(|| GraphError::Status {
            status: 0,
            message: format!("Graph returned attachment {attachment_id} without contents"),
            request_id: None,
        })?;
        base64::engine::general_purpose::STANDARD.decode(encoded).map_err(|e| GraphError::Status {
            status: 0,
            message: format!("attachment {attachment_id} isn't valid base64: {e}"),
            request_id: None,
        })
    }

    /// Responds to a meeting invitation through Graph's own RSVP action
    /// (item 12 / CAL-15): `POST /me/events/{id}/{accept|tentativelyAccept|decline}`.
    /// `send_response` asks Graph to email the organizer; `proposed_new_time`
    /// carries an iTIP COUNTER (item 3.7). `owner` selects a shared/delegated
    /// calendar's mailbox (item 5.6 / CAL-25), or `None` for the user's own.
    pub async fn respond_invitation(
        &self,
        event_id: &str,
        owner: Option<&str>,
        response: InvitationResponse,
        comment: Option<&str>,
        send_response: bool,
        proposed_new_time: Option<ProposedTime>,
    ) -> Result<()> {
        let action = match response {
            InvitationResponse::Accept => "accept",
            InvitationResponse::Tentative => "tentativelyAccept",
            InvitationResponse::Decline => "decline",
        };
        let mut body = json!({ "sendResponse": send_response });
        if let Some(comment) = comment.filter(|c| !c.trim().is_empty()) {
            body["comment"] = json!(comment);
        }
        if let Some(proposed) = proposed_new_time {
            body["proposedNewTime"] = json!({
                "start": graph_date_time(proposed.start, None),
                "end": graph_date_time(proposed.end, None),
            });
        }
        self.graph
            .post(&format!("{}/{}/{}", self.events_root(owner), encode_segment(event_id), action), &body)
            .await?;
        Ok(())
    }

    /// Attendees' availability over `[start, end)` via
    /// `POST /me/calendar/getSchedule` (item 14 / CAL-19). Returns one
    /// [`AttendeeAvailability`] per requested attendee, in request order; an
    /// attendee Graph didn't resolve (no `scheduleId` in the answer) is
    /// `unknown`, so the UI shows "no data" rather than "free".
    pub async fn get_schedule(&self, attendees: &[EmailAddress], start: DateTime<Utc>, end: DateTime<Utc>) -> Result<Vec<AttendeeAvailability>> {
        // Deduplicate the requested addresses (Graph rejects/duplicates dupes)
        // while preserving per-attendee result order below.
        let schedules: Vec<&str> = {
            let mut seen = std::collections::HashSet::new();
            attendees
                .iter()
                .map(|a| a.address.as_str())
                .filter(|address| seen.insert(address.to_ascii_lowercase()))
                .collect()
        };
        if schedules.is_empty() {
            return Ok(Vec::new());
        }
        let body = json!({
            "schedules": schedules,
            "startTime": graph_date_time(start, None),
            "endTime": graph_date_time(end, None),
            "availabilityViewInterval": AVAILABILITY_INTERVAL_MINUTES,
        });
        let response = self.graph.post("me/calendar/getSchedule", &body).await?;

        // Map each returned schedule to its busy intervals, keyed
        // case-insensitively by address.
        let mut resolved: std::collections::HashMap<String, Vec<FreeBusyInterval>> = std::collections::HashMap::new();
        for entry in response.get("value").and_then(Value::as_array).into_iter().flatten() {
            let Some(schedule_id) = entry.get("scheduleId").and_then(Value::as_str) else {
                continue;
            };
            let intervals = entry
                .get("scheduleItems")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(free_busy_interval)
                .collect();
            resolved.insert(schedule_id.to_ascii_lowercase(), intervals);
        }

        Ok(attendees
            .iter()
            .map(|attendee| match resolved.get(&attendee.address.to_ascii_lowercase()) {
                Some(intervals) => AttendeeAvailability::new(attendee.clone(), intervals.clone()),
                None => AttendeeAvailability::unknown(attendee.clone()),
            })
            .collect())
    }

    /// The tenant's bookable rooms (`GET /places/microsoft.graph.room`,
    /// item 15 / CAL-19), for the editor's room suggestions. Needs the
    /// `Place.Read.All` scope (`GraphFeature::Rooms`).
    pub async fn list_rooms(&self) -> Result<Vec<Room>> {
        let items = self
            .graph
            .get_all("places/microsoft.graph.room?$top=100&$select=id,displayName,emailAddress", MAX_ROOMS)
            .await?;
        Ok(items
            .iter()
            .filter_map(|value| {
                let address = value.get("emailAddress")?.as_str()?.to_string();
                if address.trim().is_empty() {
                    return None;
                }
                let display_name = value.get("displayName").and_then(Value::as_str).unwrap_or(&address).to_string();
                Some(Room { display_name, address })
            })
            .collect())
    }

    /// `/calendars/{id}/calendarView?startDateTime=…`, encoded and windowed.
    fn view_path(&self, calendar: &CalendarInfo, start: DateTime<Utc>, end: DateTime<Utc>) -> String {
        format!(
            "{}/{}/calendarView?startDateTime={}&endDateTime={}&{EVENT_SELECT}&$top=100{EVENT_EXPAND}",
            self.calendars_root(calendar.owner.as_deref()),
            encode_segment(&calendar.href),
            iso8601(start),
            iso8601(end),
        )
    }

    /// The `/delta` form of [`Self::view_path`].
    fn view_path_delta(&self, calendar: &CalendarInfo, start: DateTime<Utc>, end: DateTime<Utc>) -> String {
        format!(
            "{}/{}/calendarView/delta?startDateTime={}&endDateTime={}&{EVENT_SELECT}&$top=100",
            self.calendars_root(calendar.owner.as_deref()),
            encode_segment(&calendar.href),
            iso8601(start),
            iso8601(end),
        )
    }

    /// The calendars collection root for `owner`: `me/calendars` for the
    /// signed-in user, or `users/{owner}/calendars` for another user's mailbox
    /// (item 5.6 / CAL-25).
    fn calendars_root(&self, owner: Option<&str>) -> String {
        match owner {
            Some(owner) => format!("users/{}/calendars", encode_segment(owner)),
            None => "me/calendars".to_string(),
        }
    }

    /// The events collection root for `owner`: `me/events`, or
    /// `users/{owner}/events` for another user's mailbox (item 5.6 / CAL-25).
    fn events_root(&self, owner: Option<&str>) -> String {
        match owner {
            Some(owner) => format!("users/{}/events", encode_segment(owner)),
            None => "me/events".to_string(),
        }
    }

    /// The calendar's permission grants
    /// (`/me/calendars/{id}/calendarPermissions`, item 5.6 / CAL-24) - who the
    /// signed-in user has shared this calendar with, and at what role.
    pub async fn list_permissions(&self, calendar: &CalendarInfo) -> Result<Vec<CalendarPermission>> {
        let path = format!("{}/{}/calendarPermissions", self.calendars_root(None), encode_segment(&calendar.href));
        let items = self.graph.get_all(&path, MAX_PERMISSIONS).await?;
        Ok(items.iter().filter_map(super::permission_from_graph).collect())
    }

    /// Grants `email` the given `role` on `calendar`, optionally emailing them
    /// an invitation (`send_notification`).
    pub async fn create_permission(&self, calendar: &CalendarInfo, email: &str, role: CalendarRole, send_notification: bool) -> Result<CalendarPermission> {
        let path = format!("{}/{}/calendarPermissions", self.calendars_root(None), encode_segment(&calendar.href));
        let body = super::permission_create_body(email, role, send_notification);
        let created = self.graph.post(&path, &body).await?;
        super::permission_from_graph(&created).ok_or_else(malformed_permission)
    }

    /// Changes an existing grant's role, guarded by nothing (Graph has no
    /// `changeKey` on a permission) - `permission_id` is the grant's own id.
    pub async fn update_permission(&self, calendar: &CalendarInfo, permission_id: &str, role: CalendarRole) -> Result<CalendarPermission> {
        let path = format!(
            "{}/{}/calendarPermissions/{}",
            self.calendars_root(None),
            encode_segment(&calendar.href),
            encode_segment(permission_id)
        );
        let updated = self.graph.patch(&path, &super::permission_role_body(role)).await?;
        Ok(super::permission_from_graph(&updated).unwrap_or(CalendarPermission {
            id: Some(permission_id.to_string()),
            email: None,
            name: None,
            role,
            allowed_roles: vec![role],
        }))
    }

    /// Revokes a grant.
    pub async fn delete_permission(&self, calendar: &CalendarInfo, permission_id: &str) -> Result<()> {
        let path = format!(
            "{}/{}/calendarPermissions/{}",
            self.calendars_root(None),
            encode_segment(&calendar.href),
            encode_segment(permission_id)
        );
        self.graph.delete(&path).await.map(|_| ())
    }
}

/// One Graph `scheduleItem` as a [`FreeBusyInterval`], or `None` when it has no
/// resolvable time span.
fn free_busy_interval(value: &Value) -> Option<FreeBusyInterval> {
    let start = parse_graph_date_time(value.get("start")?)?.0;
    let end = parse_graph_date_time(value.get("end")?)?.0;
    let status = value.get("status").and_then(Value::as_str).and_then(BusyStatus::from_graph).unwrap_or_default();
    Some(FreeBusyInterval { start, end, status })
}

/// A Graph-required calendar-view bound: `2026-10-15T00:00:00Z`.
fn iso8601(instant: DateTime<Utc>) -> String {
    instant.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Percent-encodes a path segment (a Graph calendar/event id), leaving RFC
/// 3986 unreserved characters. Graph ids are base64-ish and may contain `+`,
/// `/` or `=`, which must not leak into the URL path.
fn encode_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Refuses a write whose RRULE Graph's recurrence model can't represent.
/// Without this, `event_fields` would omit `recurrence` and Graph would store
/// a non-recurring event, silently losing the series.
fn ensure_recurrence_supported(event: &CalendarEvent) -> Result<()> {
    if super::event_recurrence_supported(event) {
        Ok(())
    } else {
        Err(GraphError::UnsupportedRecurrence(event.rrule.clone().unwrap_or_default()))
    }
}

/// A malformed create/update response (status `0` distinguishes it from a
/// real HTTP status, as in `graph_rules`).
fn malformed_event() -> GraphError {
    GraphError::Status {
        status: 0,
        message: "Graph returned a calendar event without usable times".to_string(),
        request_id: None,
    }
}

/// An update attempted on an event that was never stored (no Graph id).
fn missing_event_id() -> GraphError {
    GraphError::Status {
        status: 0,
        message: "cannot update a calendar event without a Graph id".to_string(),
        request_id: None,
    }
}

/// A malformed permission response (no id), mirroring `malformed_event`.
fn malformed_permission() -> GraphError {
    GraphError::Status {
        status: 0,
        message: "Graph returned a calendar permission without an id".to_string(),
        request_id: None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use serde_json::{json, Value};
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::auth::TokenProvider;
    use crate::error::Result as GraphResult;

    struct StubTokens;

    #[async_trait]
    impl TokenProvider for StubTokens {
        async fn graph_token(&self) -> GraphResult<String> {
            Ok("stub-token".to_string())
        }
        async fn ensure_scopes(&self, _scopes: &[&str]) -> GraphResult<()> {
            Ok(())
        }
    }

    fn client(server: &MockServer) -> CalendarClient {
        CalendarClient::new(GraphClient::with_base_url(Arc::new(StubTokens), format!("{}/v1.0", server.uri())))
    }

    fn calendar() -> CalendarInfo {
        CalendarInfo {
            id: lookout_core::CalendarId("acct:cal1".to_string()),
            account_id: AccountId("acct".to_string()),
            display_name: "Work".to_string(),
            color: None,
            href: "cal1".to_string(),
            supports_tasks: false,
            owner: None,
        }
    }

    fn utc(y: i32, m: u32, d: u32) -> DateTime<Utc> {
        use chrono::TimeZone;
        Utc.with_ymd_and_hms(y, m, d, 0, 0, 0).unwrap()
    }

    fn event_json() -> Value {
        json!({
            "id": "ev1",
            "changeKey": "ck1",
            "iCalUId": "uid-1@example.com",
            "subject": "Standup",
            "start": { "dateTime": "2026-10-15T09:00:00", "timeZone": "UTC" },
            "end": { "dateTime": "2026-10-15T09:30:00", "timeZone": "UTC" }
        })
    }

    #[tokio::test]
    async fn list_calendars_maps_every_calendar() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/calendars"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "value": [
                    { "id": "cal1", "name": "Work", "hexColor": "#008000" },
                    { "id": "cal2", "name": "Personal" }
                ]
            })))
            .mount(&server)
            .await;

        let calendars = client(&server).list_calendars(&AccountId("acct".to_string()), None).await.unwrap();
        assert_eq!(calendars.len(), 2);
        assert_eq!(calendars[0].display_name, "Work");
        assert_eq!(calendars[0].href, "cal1");
        assert_eq!(calendars[0].id, lookout_core::CalendarId("acct:cal1".to_string()));
    }

    #[tokio::test]
    async fn list_calendars_of_another_mailbox_uses_the_users_path_and_tags_the_owner() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/users/owner%40example.com/calendars"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "value": [{ "id": "cal1", "name": "Ada's Calendar" }]
            })))
            .mount(&server)
            .await;

        let calendars = client(&server).list_calendars(&AccountId("acct".to_string()), Some("owner@example.com")).await.unwrap();
        assert_eq!(calendars.len(), 1);
        assert_eq!(calendars[0].owner.as_deref(), Some("owner@example.com"));
    }

    #[tokio::test]
    async fn calendar_view_returns_expanded_occurrences() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/calendars/cal1/calendarView"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "value": [event_json()] })))
            .mount(&server)
            .await;

        let occurrences = client(&server).calendar_view(&calendar(), utc(2026, 10, 1), utc(2026, 11, 1)).await.unwrap();
        assert_eq!(occurrences.len(), 1);
        assert_eq!(occurrences[0].uid.0, "uid-1@example.com");
        assert_eq!(occurrences[0].href.as_deref(), Some("ev1"));
    }

    #[tokio::test]
    async fn calendar_view_delta_returns_the_cursor() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/calendars/cal1/calendarView/delta"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "value": [event_json()],
                "@odata.deltaLink": "https://graph.microsoft.com/v1.0/delta-cursor"
            })))
            .mount(&server)
            .await;

        let page = client(&server).calendar_view_delta(&calendar(), utc(2026, 10, 1), utc(2026, 11, 1), None).await.unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.delta_link.as_deref(), Some("https://graph.microsoft.com/v1.0/delta-cursor"));
    }

    #[tokio::test]
    async fn create_event_posts_and_reads_the_server_copy() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1.0/me/calendars/cal1/events"))
            .respond_with(ResponseTemplate::new(201).set_body_json(event_json()))
            .mount(&server)
            .await;

        let mut event = event_from_graph(&event_json(), &calendar().id).unwrap();
        event.href = None;
        event.etag = None;
        let created = client(&server).create_event(&calendar(), &event).await.unwrap();
        assert_eq!(created.uid.0, "uid-1@example.com");
        assert_eq!(created.href.as_deref(), Some("ev1"));
        assert_eq!(created.etag.as_deref(), Some("ck1"));
    }

    #[tokio::test]
    async fn update_event_sends_if_match_and_prefer() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/v1.0/me/events/ev1"))
            .and(header("If-Match", "ck1"))
            .and(header("Prefer", "return=representation"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "ev1",
                "changeKey": "ck2",
                "iCalUId": "uid-1@example.com",
                "subject": "Standup (moved)",
                "start": { "dateTime": "2026-10-15T10:00:00", "timeZone": "UTC" },
                "end": { "dateTime": "2026-10-15T10:30:00", "timeZone": "UTC" }
            })))
            .mount(&server)
            .await;

        let event = event_from_graph(&event_json(), &calendar().id).unwrap();
        let updated = client(&server).update_event(&event, None).await.unwrap();
        assert_eq!(updated.etag.as_deref(), Some("ck2"));
        assert_eq!(updated.summary.as_deref(), Some("Standup (moved)"));
    }

    #[tokio::test]
    async fn update_event_falls_back_when_the_server_omits_the_body() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/v1.0/me/events/ev1"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;

        let event = event_from_graph(&event_json(), &calendar().id).unwrap();
        let updated = client(&server).update_event(&event, None).await.unwrap();
        assert_eq!(updated.href, event.href);
        assert_eq!(updated.summary.as_deref(), Some("Standup"));
    }

    #[tokio::test]
    async fn update_event_without_a_graph_id_is_an_error() {
        let mut event = event_from_graph(&event_json(), &calendar().id).unwrap();
        event.href = None;
        // No request is made; the client needs a server only to build the transport.
        let server = MockServer::start().await;
        assert!(client(&server).update_event(&event, None).await.is_err());
    }

    #[tokio::test]
    async fn create_event_refuses_a_recurrence_graph_cannot_express() {
        let server = MockServer::start().await;
        let mut event = event_from_graph(&event_json(), &calendar().id).unwrap();
        event.href = None;
        event.etag = None;
        event.rrule = Some("FREQ=MONTHLY;BYDAY=MO,TU,WE,TH,FR;BYSETPOS=-1".to_string());
        // The guard runs before any HTTP request; no mock is mounted.
        let result = client(&server).create_event(&calendar(), &event).await;
        assert!(
            matches!(result, Err(GraphError::UnsupportedRecurrence(ref rule)) if rule.contains("BYSETPOS")),
            "expected the recurrence to be refused"
        );
    }

    #[tokio::test]
    async fn update_event_refuses_a_recurrence_graph_cannot_express() {
        let server = MockServer::start().await;
        let mut event = event_from_graph(&event_json(), &calendar().id).unwrap();
        event.rrule = Some("FREQ=MONTHLY;BYDAY=MO".to_string());
        let result = client(&server).update_event(&event, None).await;
        assert!(matches!(result, Err(GraphError::UnsupportedRecurrence(_))), "expected the recurrence to be refused");
    }

    #[tokio::test]
    async fn delete_event_sends_if_match() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/v1.0/me/events/ev1"))
            .and(header("If-Match", "ck1"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;

        client(&server).delete_event("ev1", Some("ck1"), None).await.unwrap();
    }

    #[tokio::test]
    async fn a_delegated_event_writes_under_the_users_events_path() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/v1.0/users/owner%40example.com/events/ev1"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;

        client(&server).delete_event("ev1", None, Some("owner@example.com")).await.unwrap();
    }

    // --- CAL-21: Graph event attachments ------------------------------

    #[tokio::test]
    async fn list_attachments_reads_the_event_children() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/events/ev1/attachments"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "value": [{
                    "@odata.type": "#microsoft.graph.fileAttachment",
                    "id": "att-1",
                    "name": "agenda.pdf",
                    "contentType": "application/pdf",
                    "size": 1024
                }]
            })))
            .mount(&server)
            .await;

        let attachments = client(&server).list_attachments("ev1", None).await.unwrap();
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].name, "agenda.pdf");
        assert_eq!(attachments[0].graph_id(), Some("att-1"));
    }

    #[tokio::test]
    async fn add_attachment_posts_a_file_attachment() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1.0/me/events/ev1/attachments"))
            .and(body_partial_json(json!({
                "@odata.type": "#microsoft.graph.fileAttachment",
                "name": "notes.txt",
                "contentType": "text/plain",
                "contentBytes": "SGVsbG8gYWdlbmRhIQ=="
            })))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "@odata.type": "#microsoft.graph.fileAttachment",
                "id": "att-2",
                "name": "notes.txt"
            })))
            .mount(&server)
            .await;

        let attachment = lookout_core::EventAttachment::from_bytes("notes.txt", Some("text/plain".to_string()), b"Hello agenda!").unwrap();
        client(&server).add_attachment("ev1", None, &attachment).await.unwrap();
    }

    #[tokio::test]
    async fn fetch_attachment_decodes_content_bytes() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/events/ev1/attachments/att-2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "@odata.type": "#microsoft.graph.fileAttachment",
                "id": "att-2",
                "name": "notes.txt",
                "contentBytes": "SGVsbG8gYWdlbmRhIQ=="
            })))
            .mount(&server)
            .await;

        let bytes = client(&server).fetch_attachment("ev1", None, "att-2").await.unwrap();
        assert_eq!(bytes, b"Hello agenda!");
    }

    #[tokio::test]
    async fn delete_attachment_removes_the_child() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/v1.0/me/events/ev1/attachments/att-2"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        client(&server).delete_attachment("ev1", None, "att-2").await.unwrap();
    }

    #[tokio::test]
    async fn respond_invitation_posts_the_action_and_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1.0/me/events/ev1/tentativelyAccept"))
            .and(body_partial_json(json!({ "sendResponse": true, "comment": "running late" })))
            .respond_with(ResponseTemplate::new(202))
            .mount(&server)
            .await;

        client(&server)
            .respond_invitation("ev1", None, InvitationResponse::Tentative, Some("running late"), true, None)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn respond_invitation_carries_a_proposed_new_time() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1.0/me/events/ev1/decline"))
            .and(body_partial_json(json!({
                "sendResponse": false,
                "proposedNewTime": { "start": { "dateTime": "2026-10-15T00:00:00", "timeZone": "UTC" } }
            })))
            .respond_with(ResponseTemplate::new(202))
            .mount(&server)
            .await;

        let proposed = ProposedTime {
            start: utc(2026, 10, 15),
            end: utc(2026, 10, 16),
        };
        client(&server)
            .respond_invitation("ev1", None, InvitationResponse::Decline, None, false, Some(proposed))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn get_schedule_maps_resolved_attendees_and_marks_others_unknown() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1.0/me/calendar/getSchedule"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "value": [{
                    "scheduleId": "bob@example.com",
                    "scheduleItems": [{
                        "status": "busy",
                        "start": { "dateTime": "2026-10-15T09:00:00", "timeZone": "UTC" },
                        "end": { "dateTime": "2026-10-15T10:00:00", "timeZone": "UTC" }
                    }]
                }]
            })))
            .mount(&server)
            .await;

        let attendees = vec![EmailAddress::new("bob@example.com"), EmailAddress::new("ghost@example.com")];
        let availabilities = client(&server).get_schedule(&attendees, utc(2026, 10, 15), utc(2026, 10, 16)).await.unwrap();
        assert_eq!(availabilities.len(), 2, "one per requested attendee, in order");
        assert!(!availabilities[0].unknown);
        assert_eq!(availabilities[0].intervals.len(), 1);
        assert_eq!(availabilities[0].intervals[0].status, BusyStatus::Busy);
        assert!(availabilities[1].unknown, "an unresolved attendee is unknown, not free");
    }

    #[tokio::test]
    async fn list_rooms_maps_the_directory() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/places/microsoft.graph.room"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "value": [{ "id": "r1", "displayName": "Room 4", "emailAddress": "room4@example.com" }]
            })))
            .mount(&server)
            .await;

        let rooms = client(&server).list_rooms().await.unwrap();
        assert_eq!(rooms.len(), 1);
        assert_eq!(rooms[0].display_name, "Room 4");
        assert_eq!(rooms[0].address, "room4@example.com");
    }

    #[test]
    fn encode_segment_escapes_path_specials() {
        assert_eq!(encode_segment("cal1"), "cal1");
        assert_eq!(encode_segment("a+b/c="), "a%2Bb%2Fc%3D");
    }

    #[tokio::test]
    async fn list_permissions_maps_the_grants() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/calendars/cal1/calendarPermissions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "value": [{
                    "id": "perm1",
                    "emailAddress": { "name": "Ada", "address": "ada@example.com" },
                    "role": "read",
                    "allowedRoles": ["freeBusyRead", "limitedRead", "read"]
                }]
            })))
            .mount(&server)
            .await;

        let permissions = client(&server).list_permissions(&calendar()).await.unwrap();
        assert_eq!(permissions.len(), 1);
        assert_eq!(permissions[0].id.as_deref(), Some("perm1"));
        assert_eq!(permissions[0].role, CalendarRole::Read);
        assert_eq!(permissions[0].allowed_roles.len(), 3);
    }

    #[tokio::test]
    async fn create_permission_posts_the_recipient_and_role() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1.0/me/calendars/cal1/calendarPermissions"))
            .and(body_partial_json(json!({
                "emailAddress": { "address": "ada@example.com" },
                "role": "write",
                "sendNotification": true
            })))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "id": "perm2",
                "emailAddress": { "address": "ada@example.com" },
                "role": "write"
            })))
            .mount(&server)
            .await;

        let permission = client(&server).create_permission(&calendar(), "ada@example.com", CalendarRole::Write, true).await.unwrap();
        assert_eq!(permission.id.as_deref(), Some("perm2"));
        assert_eq!(permission.role, CalendarRole::Write);
    }

    /// Item 3.13: the row menu's Rename PATCHes the calendar's `name` at
    /// `/me/calendars/{id}`.
    #[tokio::test]
    async fn rename_calendar_patches_the_name() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/v1.0/me/calendars/cal1"))
            .and(body_partial_json(json!({ "name": "Renamed" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "cal1", "name": "Renamed" })))
            .mount(&server)
            .await;

        client(&server).rename_calendar(&calendar(), "Renamed").await.unwrap();
    }

    #[tokio::test]
    async fn update_permission_patches_the_role() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/v1.0/me/calendars/cal1/calendarPermissions/perm2"))
            .and(body_partial_json(json!({ "role": "limitedRead" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "perm2",
                "role": "limitedRead"
            })))
            .mount(&server)
            .await;

        let permission = client(&server).update_permission(&calendar(), "perm2", CalendarRole::LimitedRead).await.unwrap();
        assert_eq!(permission.role, CalendarRole::LimitedRead);
    }

    #[tokio::test]
    async fn delete_permission_deletes_the_grant() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/v1.0/me/calendars/cal1/calendarPermissions/perm2"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;

        client(&server).delete_permission(&calendar(), "perm2").await.unwrap();
    }
}
