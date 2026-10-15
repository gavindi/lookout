//! Microsoft Graph ⇄ `lookout-core` calendar mapping (OUTLOOK_PARITY.md §5.1,
//! item 5.1 / A-03).
//!
//! Graph's `/me/calendars`, `/me/calendarView` and `/me/events` speak a
//! JSON event vocabulary, and the calendar UI speaks `lookout_core`'s
//! `CalendarInfo` / `CalendarEvent` / `EventOccurrence`. This module is the
//! one place the two are translated, so neither the session actor nor the
//! feature code hard-codes Graph property names (§10 "Interop strings").
//!
//! This module is deliberately pure - no HTTP, no `GraphClient` - so every
//! rule is unit-testable against inline JSON. The HTTP transport lives in
//! [`client`] (`CalendarClient`), and the actor that drives it
//! (`calendar::session`, a later item) owns the command loop.
//!
//! Deferred to their own items, and therefore mapped as `None`/ignored here:
//! - **Recurrence** (`recurrence` ⇄ the model's raw `RRULE`): item 9. A
//!   recurring event read through `calendarView` still renders (Graph expands
//!   occurrences for us), but its series rule isn't yet carried back.
//! - **Creating a Teams meeting** (`isOnlineMeeting` on write): item 10. The
//!   join URL of a meeting *created in Outlook* is still read into
//!   `conference_url`.
//! - **Event importance**: the model has no per-event importance field, so
//!   Graph's `importance` is left at its server default on write and ignored
//!   on read.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
pub mod client;

use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone, Utc};
use lookout_core::{
    AccountId, Attendee, AttendeeRole, AttendeeStatus, BusyStatus, CalendarEvent, CalendarId, CalendarInfo, EmailAddress, EventOccurrence, EventSensitivity, EventUid,
};
use serde_json::{json, Value};

/// Maps a Graph `calendar` object to a [`CalendarInfo`]. `href` carries the
/// Graph calendar id (the path segment the session builds
/// `/me/calendars/{href}/…` from); `id` is the account-scoped `<account>:<id>`
/// the rest of the app keys calendars by. Graph calendars never carry VTODOs,
/// so `supports_tasks` is always `false` (Microsoft To Do is item 5.3).
pub fn calendar_from_graph(value: &Value, account_id: &AccountId) -> Option<CalendarInfo> {
    let graph_id = value.get("id")?.as_str()?;
    let display_name = value.get("name").and_then(Value::as_str).unwrap_or("Calendar").to_string();
    let color = value.get("hexColor").and_then(Value::as_str).map(str::to_string).filter(|c| !c.is_empty());
    Some(CalendarInfo {
        id: CalendarId::new(account_id, graph_id),
        account_id: account_id.clone(),
        display_name,
        color,
        href: graph_id.to_string(),
        supports_tasks: false,
    })
}

/// Maps a Graph `event` (from `/me/events`, or a create/update response) to
/// the full [`CalendarEvent`] form. `uid` is the iCalendar `UID` Graph echoes
/// (`iCalUId`), falling back to the Graph id, so an event created in Outlook
/// or Lookout keeps one identity; `href`/`etag` carry the Graph id/`changeKey`
/// for later updates and deletes.
pub fn event_from_graph(value: &Value, calendar_id: &CalendarId) -> Option<CalendarEvent> {
    let uid = event_uid(value)?;
    let start = parse_graph_date_time(value.get("start")?)?;
    let end = parse_graph_date_time(value.get("end")?)?;
    let time_zone = start.1.clone().or_else(|| end.1.clone());
    Some(CalendarEvent {
        uid,
        calendar_id: calendar_id.clone(),
        summary: non_empty_str(value, "subject"),
        description: body_text(value),
        location: location_text(value),
        start: start.0,
        end: end.0,
        all_day: is_all_day(value),
        time_zone,
        rrule: None,
        recurrence_id: None,
        recurrence_range: Default::default(),
        exdates: Vec::new(),
        rdates: Vec::new(),
        href: graph_id(value),
        etag: change_key(value),
        sequence: 0,
        attendees: attendees_from_graph(value),
        organizer: organizer_from_graph(value),
        schedule_agent: None,
        categories: string_array(value, "categories"),
        sensitivity: sensitivity_from_graph(value.get("sensitivity").and_then(Value::as_str)),
        show_as: show_as_from_graph(value),
        reminder_minutes_before: reminder_minutes(value),
        conference_url: conference_url(value),
    })
}

/// Maps a Graph `event` as returned by `/me/calendarView` to the render form
/// [`EventOccurrence`]. `calendarView` already expands a series for us, so
/// each returned object is one instance; `href`/`etag` are that instance's own
/// Graph id/`changeKey` (so an edit targets the exact occurrence).
pub fn occurrence_from_graph(value: &Value, calendar_id: &CalendarId) -> Option<EventOccurrence> {
    let uid = event_uid(value)?;
    let start = parse_graph_date_time(value.get("start")?)?;
    let end = parse_graph_date_time(value.get("end")?)?;
    let time_zone = start.1.clone().or_else(|| end.1.clone());
    Some(EventOccurrence {
        uid,
        calendar_id: calendar_id.clone(),
        summary: non_empty_str(value, "subject"),
        description: body_text(value),
        location: location_text(value),
        start: start.0,
        end: end.0,
        all_day: is_all_day(value),
        time_zone,
        rrule: None,
        recurrence_id: None,
        exdates: Vec::new(),
        master_start: None,
        master_end: None,
        href: graph_id(value),
        etag: change_key(value),
        master_href: None,
        master_etag: None,
        sequence: 0,
        attendees: attendees_from_graph(value),
        organizer: organizer_from_graph(value),
        schedule_agent: None,
        categories: string_array(value, "categories"),
        sensitivity: sensitivity_from_graph(value.get("sensitivity").and_then(Value::as_str)),
        show_as: show_as_from_graph(value),
        reminder_minutes_before: reminder_minutes(value),
        conference_url: conference_url(value),
    })
}

/// The JSON body to `POST /me/calendars/{id}/events` for a new event.
/// Includes a `transactionId` (the event's UID) so a retried create after a
/// timeout can't duplicate the event.
pub fn event_create_body(event: &CalendarEvent) -> Value {
    let mut body = event_fields(event);
    body["transactionId"] = json!(event.uid.0);
    body
}

/// The JSON body to `PATCH /me/events/{id}` for an edit: the same fields as
/// [`event_create_body`] minus `transactionId` (Graph rejects it on update).
pub fn event_patch_body(event: &CalendarEvent) -> Value {
    event_fields(event)
}

/// The mutable event fields shared by create and update.
fn event_fields(event: &CalendarEvent) -> Value {
    let zone = event.time_zone.as_deref();
    let mut body = json!({
        "subject": event.summary.clone().unwrap_or_default(),
        "start": graph_date_time(event.start, zone),
        "end": graph_date_time(event.end, zone),
        "isAllDay": event.all_day,
        "showAs": event.show_as.as_graph_str(),
        "sensitivity": sensitivity_to_graph(event.sensitivity),
    });
    if let Some(description) = &event.description {
        if !description.is_empty() {
            body["body"] = json!({ "contentType": "text", "content": description });
        }
    }
    if let Some(location) = event.location.as_deref().filter(|l| !l.trim().is_empty()) {
        body["location"] = json!({ "displayName": location });
    }
    if !event.categories.is_empty() {
        body["categories"] = json!(event.categories);
    }
    let attendees: Vec<Value> = event.attendees.iter().map(attendee_to_graph).collect();
    if !attendees.is_empty() {
        body["attendees"] = json!(attendees);
    }
    match event.reminder_minutes_before {
        Some(minutes) => {
            body["isReminderOn"] = json!(true);
            body["reminderMinutesBeforeStart"] = json!(minutes);
        }
        None => body["isReminderOn"] = json!(false),
    }
    body
}

/// Maps a core [`Attendee`] to a Graph `attendee` for writing. The response
/// status is not written - Graph derives it and rejects a client-supplied one
/// on create.
pub fn attendee_to_graph(attendee: &Attendee) -> Value {
    json!({
        "emailAddress": {
            "name": attendee.address.name.clone().unwrap_or_default(),
            "address": attendee.address.address,
        },
        "type": match attendee.role {
            AttendeeRole::Required => "required",
            AttendeeRole::Optional => "optional",
        },
    })
}

/// Parses a Graph `attendee`, or `None` when it has no address.
pub fn attendee_from_graph(value: &Value) -> Option<Attendee> {
    let email = value.get("emailAddress")?;
    let address = email.get("address")?.as_str()?;
    if address.trim().is_empty() {
        return None;
    }
    let name = email.get("name").and_then(Value::as_str).filter(|n| !n.trim().is_empty()).map(str::to_string);
    let role = match value.get("type").and_then(Value::as_str) {
        Some("optional") => AttendeeRole::Optional,
        // "resource" (a room/equipment mailbox) has no distinct role in the
        // model yet; treat it as a required participant.
        _ => AttendeeRole::Required,
    };
    let status = match value.get("status").and_then(|status| status.get("response")).and_then(Value::as_str) {
        Some("accepted") | Some("organizer") => AttendeeStatus::Accepted,
        Some("declined") => AttendeeStatus::Declined,
        Some("tentativelyAccepted") => AttendeeStatus::Tentative,
        _ => AttendeeStatus::NeedsAction,
    };
    Some(Attendee {
        address: EmailAddress {
            name,
            address: address.to_string(),
        },
        role,
        status,
    })
}

/// Parses a Graph `dateTimeTimeZone` (`{ dateTime, timeZone }`) into a UTC
/// instant plus the event's IANA zone (if it has one). The zone may be a
/// Windows name (`"Pacific Standard Time"`) or an IANA name; Windows names
/// resolve through the shared `lookout_core::windows_to_iana`. A UTC/floating
/// value yields `None` for the zone (matching the model's convention).
pub fn parse_graph_date_time(value: &Value) -> Option<(DateTime<Utc>, Option<String>)> {
    let raw = value.get("dateTime")?.as_str()?;
    let time_zone = value.get("timeZone").and_then(Value::as_str);
    parse_graph_date_time_str(raw, time_zone)
}

/// The string form of [`parse_graph_date_time`], split out for testability.
pub fn parse_graph_date_time_str(raw: &str, time_zone: Option<&str>) -> Option<(DateTime<Utc>, Option<String>)> {
    // A trailing `Z` means the wall time is already UTC, whatever `timeZone`
    // says; otherwise `dateTime` is a local wall time in `timeZone`.
    let (body, explicit_utc) = match raw.strip_suffix('Z') {
        Some(stripped) => (stripped, true),
        None => (raw, false),
    };
    // Graph emits trailing fractional seconds ("…:00.0000000"); some all-day
    // values are date-only.
    let body = body.split('.').next().unwrap_or(body);
    let naive = NaiveDateTime::parse_from_str(body, "%Y-%m-%dT%H:%M:%S")
        .ok()
        .or_else(|| NaiveDate::parse_from_str(body, "%Y-%m-%d").ok().and_then(|date| date.and_hms_opt(0, 0, 0)))?;

    if explicit_utc {
        return Some((naive.and_utc(), None));
    }
    let Some(name) = time_zone else {
        return Some((naive.and_utc(), None));
    };
    let iana = to_iana(name)?;
    if iana == "Etc/UTC" || iana == "UTC" {
        return Some((naive.and_utc(), None));
    }
    let zone: chrono_tz::Tz = iana.parse().ok()?;
    // A DST fall-back makes the wall time ambiguous; the earlier instant is
    // the conventional choice (RFC 5545 §3.3.5).
    let local = zone.from_local_datetime(&naive).earliest()?;
    Some((local.with_timezone(&Utc), Some(iana)))
}

/// Builds a Graph `dateTimeTimeZone` for a UTC instant, rendered as a wall
/// time in `zone` (an IANA or Windows name; unknown/absent → UTC, emitted as
/// the IANA `"UTC"` Graph accepts).
pub fn graph_date_time(instant: DateTime<Utc>, zone: Option<&str>) -> Value {
    if let Some(name) = zone.filter(|z| !z.trim().is_empty()) {
        if let Some(iana) = to_iana(name) {
            if let Ok(tz) = iana.parse::<chrono_tz::Tz>() {
                let wall = instant.with_timezone(&tz).naive_local();
                return json!({ "dateTime": wall.format("%Y-%m-%dT%H:%M:%S").to_string(), "timeZone": iana });
            }
        }
    }
    json!({ "dateTime": instant.naive_utc().format("%Y-%m-%dT%H:%M:%S").to_string(), "timeZone": "UTC" })
}

/// Resolves a Graph/Outlook time-zone name to its IANA name: an IANA name
/// passes through, a Windows name (`"W. Europe Standard Time"`) maps through
/// the shared CLDR table; anything else is `None`.
fn to_iana(name: &str) -> Option<String> {
    if name.parse::<chrono_tz::Tz>().is_ok() {
        Some(name.to_string())
    } else {
        lookout_core::windows_to_iana(name).map(str::to_string)
    }
}

/// The event's iCalendar `UID`: Graph's `iCalUId` when present, else the Graph
/// id (so identity survives even for an object Graph didn't stamp a UID on).
fn event_uid(value: &Value) -> Option<EventUid> {
    let uid = value
        .get("iCalUId")
        .and_then(Value::as_str)
        .filter(|u| !u.is_empty())
        .or_else(|| value.get("id").and_then(Value::as_str))?;
    Some(EventUid(uid.to_string()))
}

fn graph_id(value: &Value) -> Option<String> {
    value.get("id").and_then(Value::as_str).map(str::to_string)
}

fn change_key(value: &Value) -> Option<String> {
    value.get("changeKey").and_then(Value::as_str).map(str::to_string)
}

fn is_all_day(value: &Value) -> bool {
    value.get("isAllDay").and_then(Value::as_bool).unwrap_or(false)
}

fn show_as_from_graph(value: &Value) -> BusyStatus {
    value.get("showAs").and_then(Value::as_str).and_then(BusyStatus::from_graph).unwrap_or_default()
}

fn sensitivity_from_graph(value: Option<&str>) -> EventSensitivity {
    match value {
        Some("private") => EventSensitivity::Private,
        Some("confidential") => EventSensitivity::Confidential,
        // "normal" and Outlook's "personal" both show as the default.
        _ => EventSensitivity::Public,
    }
}

fn sensitivity_to_graph(sensitivity: EventSensitivity) -> &'static str {
    match sensitivity {
        EventSensitivity::Public => "normal",
        EventSensitivity::Private => "private",
        EventSensitivity::Confidential => "confidential",
    }
}

/// The event's reminder in minutes before start, or `None` when reminders are
/// off/unset.
fn reminder_minutes(value: &Value) -> Option<i64> {
    if value.get("isReminderOn").and_then(Value::as_bool) == Some(false) {
        return None;
    }
    value.get("reminderMinutesBeforeStart").and_then(Value::as_i64)
}

fn body_text(value: &Value) -> Option<String> {
    value
        .get("body")
        .and_then(|body| body.get("content"))
        .and_then(Value::as_str)
        .filter(|c| !c.is_empty())
        .map(str::to_string)
}

fn location_text(value: &Value) -> Option<String> {
    value
        .get("location")
        .and_then(|location| location.get("displayName"))
        .and_then(Value::as_str)
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
}

fn non_empty_str(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string)
}

fn string_array(value: &Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default()
}

fn attendees_from_graph(value: &Value) -> Vec<Attendee> {
    value
        .get("attendees")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(attendee_from_graph).collect())
        .unwrap_or_default()
}

fn organizer_from_graph(value: &Value) -> Option<EmailAddress> {
    let email = value.get("organizer")?.get("emailAddress")?;
    let address = email.get("address")?.as_str()?;
    if address.trim().is_empty() {
        return None;
    }
    let name = email.get("name").and_then(Value::as_str).filter(|n| !n.trim().is_empty()).map(str::to_string);
    Some(EmailAddress {
        name,
        address: address.to_string(),
    })
}

/// The join URL of a meeting created elsewhere in Outlook/Teams, taken from
/// `onlineMeeting.joinUrl` first and the legacy `onlineMeetingUrl` second.
fn conference_url(value: &Value) -> Option<String> {
    value
        .get("onlineMeeting")
        .and_then(|meeting| meeting.get("joinUrl"))
        .and_then(Value::as_str)
        .filter(|u| !u.is_empty())
        .or_else(|| value.get("onlineMeetingUrl").and_then(Value::as_str).filter(|u| !u.is_empty()))
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account() -> AccountId {
        AccountId("acct".to_string())
    }

    fn calendar_id() -> CalendarId {
        CalendarId("acct:graph-cal".to_string())
    }

    fn sample_event() -> Value {
        json!({
            "id": "AAMkAD-event",
            "changeKey": "ck-1",
            "iCalUId": "040000008200E00074C5B7101A82E008@example.com",
            "subject": "Design review",
            "body": { "contentType": "html", "content": "<p>Agenda attached</p>" },
            "start": { "dateTime": "2026-10-15T09:00:00.0000000", "timeZone": "Pacific Standard Time" },
            "end": { "dateTime": "2026-10-15T10:00:00.0000000", "timeZone": "Pacific Standard Time" },
            "isAllDay": false,
            "location": { "displayName": "Room 4" },
            "showAs": "tentative",
            "sensitivity": "private",
            "categories": ["Projects", "Urgent"],
            "isReminderOn": true,
            "reminderMinutesBeforeStart": 15,
            "organizer": { "emailAddress": { "name": "Ada", "address": "ada@example.com" } },
            "attendees": [
                { "emailAddress": { "name": "Bob", "address": "bob@example.com" }, "type": "required", "status": { "response": "accepted" } },
                { "emailAddress": { "address": "carol@example.com" }, "type": "optional", "status": { "response": "tentativelyAccepted" } }
            ],
            "onlineMeeting": { "joinUrl": "https://teams.microsoft.com/l/meetup-join/abc" }
        })
    }

    #[test]
    fn calendar_maps_id_name_colour_and_href() {
        let value = json!({ "id": "graph-cal", "name": "Work", "hexColor": "#008000" });
        let info = calendar_from_graph(&value, &account()).unwrap();
        assert_eq!(info.id, CalendarId::new(&account(), "graph-cal"));
        assert_eq!(info.display_name, "Work");
        assert_eq!(info.color.as_deref(), Some("#008000"));
        assert_eq!(info.href, "graph-cal");
        assert!(!info.supports_tasks, "Graph calendars never carry VTODOs");
        assert_eq!(info.account_id, account());
    }

    #[test]
    fn calendar_defaults_a_missing_name_and_ignores_an_empty_colour() {
        let value = json!({ "id": "graph-cal", "hexColor": "" });
        let info = calendar_from_graph(&value, &account()).unwrap();
        assert_eq!(info.display_name, "Calendar");
        assert_eq!(info.color, None);
    }

    #[test]
    fn event_reads_every_mapped_field() {
        let event = event_from_graph(&sample_event(), &calendar_id()).unwrap();
        assert_eq!(event.uid.0, "040000008200E00074C5B7101A82E008@example.com");
        assert_eq!(event.href.as_deref(), Some("AAMkAD-event"));
        assert_eq!(event.etag.as_deref(), Some("ck-1"));
        assert_eq!(event.summary.as_deref(), Some("Design review"));
        assert_eq!(event.description.as_deref(), Some("<p>Agenda attached</p>"));
        assert_eq!(event.location.as_deref(), Some("Room 4"));
        assert_eq!(event.show_as, BusyStatus::Tentative);
        assert_eq!(event.sensitivity, EventSensitivity::Private);
        assert_eq!(event.categories, vec!["Projects".to_string(), "Urgent".to_string()]);
        assert_eq!(event.reminder_minutes_before, Some(15));
        assert_eq!(event.conference_url.as_deref(), Some("https://teams.microsoft.com/l/meetup-join/abc"));
        // Pacific 09:00 on 15 Oct 2026 (PDT, UTC-7) is 16:00 UTC; the Windows
        // name resolves to the IANA zone, kept on the event.
        assert_eq!(event.start.to_rfc3339(), "2026-10-15T16:00:00+00:00");
        assert_eq!(event.time_zone.as_deref(), Some("America/Los_Angeles"));
        assert_eq!(event.organizer.as_ref().unwrap().address, "ada@example.com");
        assert_eq!(event.attendees.len(), 2);
        assert_eq!(event.attendees[0].role, AttendeeRole::Required);
        assert_eq!(event.attendees[0].status, AttendeeStatus::Accepted);
        assert_eq!(event.attendees[1].role, AttendeeRole::Optional);
        assert_eq!(event.attendees[1].status, AttendeeStatus::Tentative);
    }

    #[test]
    fn occurrence_reads_the_same_fields_without_master_metadata() {
        let occurrence = occurrence_from_graph(&sample_event(), &calendar_id()).unwrap();
        assert_eq!(occurrence.uid.0, "040000008200E00074C5B7101A82E008@example.com");
        assert_eq!(occurrence.href.as_deref(), Some("AAMkAD-event"));
        assert_eq!(occurrence.master_href, None);
        assert_eq!(occurrence.rrule, None);
        assert_eq!(occurrence.show_as, BusyStatus::Tentative);
        assert_eq!(occurrence.start, event_from_graph(&sample_event(), &calendar_id()).unwrap().start);
    }

    #[test]
    fn an_event_without_ical_uid_falls_back_to_the_graph_id() {
        let value = json!({
            "id": "graph-only-id",
            "start": { "dateTime": "2026-10-15T09:00:00", "timeZone": "UTC" },
            "end": { "dateTime": "2026-10-15T10:00:00", "timeZone": "UTC" }
        });
        let event = event_from_graph(&value, &calendar_id()).unwrap();
        assert_eq!(event.uid.0, "graph-only-id");
        assert_eq!(event.time_zone, None, "a UTC value has no zone");
        assert_eq!(event.reminder_minutes_before, None);
        assert!(event.attendees.is_empty());
    }

    #[test]
    fn all_day_events_keep_the_date_and_flag() {
        let value = json!({
            "id": "e",
            "subject": "Holiday",
            "isAllDay": true,
            "start": { "dateTime": "2026-12-25T00:00:00.0000000", "timeZone": "Europe/Berlin" },
            "end": { "dateTime": "2026-12-26T00:00:00.0000000", "timeZone": "Europe/Berlin" }
        });
        let event = event_from_graph(&value, &calendar_id()).unwrap();
        assert!(event.all_day);
        // Midnight Berlin (CET, UTC+1) on the 25th is 23:00 UTC on the 24th.
        assert_eq!(event.start.to_rfc3339(), "2026-12-24T23:00:00+00:00");
    }

    #[test]
    fn show_as_unknown_and_free_default_and_map() {
        let value = json!({ "id": "e", "showAs": "free", "start": { "dateTime": "2026-10-15T09:00:00", "timeZone": "UTC" }, "end": { "dateTime": "2026-10-15T10:00:00", "timeZone": "UTC" } });
        assert_eq!(event_from_graph(&value, &calendar_id()).unwrap().show_as, BusyStatus::Free);
        let value = json!({ "id": "e", "showAs": "unknown", "start": { "dateTime": "2026-10-15T09:00:00", "timeZone": "UTC" }, "end": { "dateTime": "2026-10-15T10:00:00", "timeZone": "UTC" } });
        assert_eq!(event_from_graph(&value, &calendar_id()).unwrap().show_as, BusyStatus::Busy);
    }

    #[test]
    fn a_body_edit_round_trips_through_the_graph_fields() {
        let event = event_from_graph(&sample_event(), &calendar_id()).unwrap();
        let body = event_patch_body(&event);
        assert_eq!(body["subject"], "Design review");
        assert_eq!(body["start"]["timeZone"], "America/Los_Angeles");
        assert_eq!(body["start"]["dateTime"], "2026-10-15T09:00:00");
        assert_eq!(body["end"]["dateTime"], "2026-10-15T10:00:00");
        assert_eq!(body["body"]["contentType"], "text");
        assert_eq!(body["body"]["content"], "<p>Agenda attached</p>");
        assert_eq!(body["location"]["displayName"], "Room 4");
        assert_eq!(body["showAs"], "tentative");
        assert_eq!(body["sensitivity"], "private");
        assert_eq!(body["isReminderOn"], true);
        assert_eq!(body["reminderMinutesBeforeStart"], 15);
        assert_eq!(body["categories"], json!(["Projects", "Urgent"]));
        assert_eq!(body["attendees"][0]["emailAddress"]["address"], "bob@example.com");
        assert_eq!(body["attendees"][0]["type"], "required");
        assert_eq!(body["attendees"][1]["type"], "optional");
        assert!(body.get("transactionId").is_none(), "transactionId is create-only");
    }

    #[test]
    fn create_body_adds_the_transaction_id() {
        let event = event_from_graph(&sample_event(), &calendar_id()).unwrap();
        assert_eq!(event_create_body(&event)["transactionId"], "040000008200E00074C5B7101A82E008@example.com");
    }

    #[test]
    fn a_zone_less_event_writes_utc() {
        let value = json!({ "id": "e", "start": { "dateTime": "2026-10-15T09:00:00", "timeZone": "UTC" }, "end": { "dateTime": "2026-10-15T10:00:00", "timeZone": "UTC" } });
        let event = event_from_graph(&value, &calendar_id()).unwrap();
        let body = event_patch_body(&event);
        assert_eq!(body["start"]["timeZone"], "UTC");
        assert_eq!(body["start"]["dateTime"], "2026-10-15T09:00:00");
        assert_eq!(body["isReminderOn"], false, "no reminder clears the flag");
    }

    #[test]
    fn unknown_zones_drop_the_event_like_caldav_does() {
        let value = json!({ "id": "e", "start": { "dateTime": "2026-10-15T09:00:00", "timeZone": "Mars Standard Time" }, "end": { "dateTime": "2026-10-15T10:00:00", "timeZone": "Mars Standard Time" } });
        assert!(event_from_graph(&value, &calendar_id()).is_none());
    }

    #[test]
    fn parse_accepts_a_z_suffix_and_fractional_seconds() {
        let (utc, zone) = parse_graph_date_time_str("2026-10-15T09:00:00.0000000Z", None).unwrap();
        assert_eq!(utc.to_rfc3339(), "2026-10-15T09:00:00+00:00");
        assert_eq!(zone, None);
    }

    #[test]
    fn attendee_role_resource_is_treated_as_required() {
        let attendee = attendee_from_graph(&json!({
            "emailAddress": { "address": "room@example.com" },
            "type": "resource",
            "status": { "response": "none" }
        }))
        .unwrap();
        assert_eq!(attendee.role, AttendeeRole::Required);
        assert_eq!(attendee.status, AttendeeStatus::NeedsAction);
    }
}
