//! RFC 6638 free/busy: the `VFREEBUSY` request a client POSTs to a CalDAV
//! schedule-outbox, and the `CALDAV:schedule-response` answer it gets back
//! (OUTLOOK_PARITY.md §4.5, CAL-19).
//!
//! The request is an iCalendar document (`METHOD:REQUEST` + a `VFREEBUSY`
//! component listing the organizer and the attendees whose time is wanted);
//! the response is XML with one `CALDAV:response` per recipient, each
//! carrying a scheduling status and, on success, a `CALDAV:calendar-data`
//! `VFREEBUSY` whose `FREEBUSY` lines are the recipient's occupied periods.
//!
//! Both ends normalise onto `lookout_core`'s [`AttendeeAvailability`], so the
//! UI never sees the wire shape.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeZone, Utc};
use quick_xml::events::Event;
use quick_xml::reader::NsReader;

use lookout_core::{AttendeeAvailability, BusyStatus, EmailAddress, FreeBusyInterval, ScheduleStatus};

use crate::error::Result;
use crate::ical::parse_ical_duration;

/// Builds the `VFREEBUSY` request body (RFC 6638 §5, RFC 5546 §3.3.2) to POST
/// to the schedule-outbox: a `METHOD:REQUEST` calendar listing `organizer`
/// plus every `attendee`, asking for their busy time in `[start, end)`.
/// `uid` identifies the request (`UID`); the `DTSTAMP` is generated fresh.
pub fn build_vfreebusy_request(organizer: &EmailAddress, attendees: &[EmailAddress], start: DateTime<Utc>, end: DateTime<Utc>, uid: &str) -> String {
    let mut body = String::from("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Lookout//CalDAV Client//EN\r\nMETHOD:REQUEST\r\nBEGIN:VFREEBUSY\r\n");
    body.push_str(&format!("UID:{}\r\n", escape_text(uid)));
    body.push_str(&format!("DTSTAMP:{}\r\n", Utc::now().format("%Y%m%dT%H%M%SZ")));
    body.push_str(&format!("DTSTART:{}\r\n", start.format("%Y%m%dT%H%M%SZ")));
    body.push_str(&format!("DTEND:{}\r\n", end.format("%Y%m%dT%H%M%SZ")));
    body.push_str(&format!("ORGANIZER:{}\r\n", mailto(organizer)));
    for attendee in attendees {
        body.push_str(&format!("ATTENDEE:{}\r\n", mailto(attendee)));
    }
    body.push_str("END:VFREEBUSY\r\nEND:VCALENDAR\r\n");
    body
}

/// The `mailto:` (or bare) calendar-user address of an attendee.
fn mailto(address: &EmailAddress) -> String {
    let bare = address.address.trim();
    if bare.to_ascii_lowercase().starts_with("mailto:") {
        bare.to_string()
    } else {
        format!("mailto:{bare}")
    }
}

/// Parses a `CALDAV:schedule-response` (RFC 6638 §10.1) into one
/// [`AttendeeAvailability`] per `CALDAV:response`. A response whose status is
/// not a success family yields an `unknown` availability, so the UI shows the
/// attendee as unaccounted-for rather than free.
pub fn parse_schedule_response(xml: &str) -> Result<Vec<AttendeeAvailability>> {
    let mut reader = NsReader::from_str(xml);
    let mut entries: Vec<RawEntry> = Vec::new();
    let mut current: Option<RawEntry> = None;
    let mut depth: u32 = 0;
    let mut text: Option<(u32, String)> = None;

    loop {
        let (_, event) = reader.read_resolved_event()?;
        match event {
            Event::Eof => break,
            Event::Start(e) => {
                depth += 1;
                let local = String::from_utf8_lossy(e.local_name().as_ref()).to_string();
                match local.as_str() {
                    "response" => current = Some(RawEntry::default()),
                    "href" | "request-status" | "calendar-data" => {
                        text = Some((depth, String::new()));
                    }
                    _ => {}
                }
            }
            // A self-closing element carries no text we track and doesn't
            // change the nesting depth.
            Event::Empty(_) => {}
            Event::Text(t) => {
                if let Some((_, buffer)) = text.as_mut() {
                    buffer.push_str(&t.decode().map_err(quick_xml::Error::from)?);
                }
            }
            Event::GeneralRef(r) => {
                if let Some(resolved) = resolve_entity(&r) {
                    if let Some((_, buffer)) = text.as_mut() {
                        buffer.push_str(&resolved);
                    }
                }
            }
            Event::End(e) => {
                let local = String::from_utf8_lossy(e.local_name().as_ref()).to_string();
                if let Some((start_depth, value)) = text.take() {
                    if start_depth == depth {
                        match local.as_str() {
                            "href" => {
                                if let Some(entry) = current.as_mut() {
                                    if entry.recipient.is_none() {
                                        entry.recipient = Some(value.trim().to_string());
                                    }
                                }
                            }
                            "request-status" => {
                                if let Some(entry) = current.as_mut() {
                                    entry.status = Some(value.trim().to_string());
                                }
                            }
                            "calendar-data" => {
                                if let Some(entry) = current.as_mut() {
                                    entry.calendar_data = Some(value);
                                }
                            }
                            _ => {}
                        }
                    } else {
                        text = Some((start_depth, value));
                    }
                }
                if local == "response" {
                    if let Some(entry) = current.take() {
                        entries.push(entry);
                    }
                }
                depth -= 1;
            }
            _ => {}
        }
    }

    Ok(entries.into_iter().map(RawEntry::into_availability).collect())
}

#[derive(Default)]
struct RawEntry {
    recipient: Option<String>,
    status: Option<String>,
    calendar_data: Option<String>,
}

impl RawEntry {
    fn into_availability(self) -> AttendeeAvailability {
        let address = EmailAddress::new(self.recipient.as_deref().map(strip_mailto).unwrap_or_default());
        let status = self.status.as_deref().map(parse_request_status);
        let intervals = self.calendar_data.as_deref().map(parse_vfreebusy_intervals).unwrap_or_default();
        let succeeded = status.as_ref().is_some_and(ScheduleStatus::is_success);
        if !succeeded && intervals.is_empty() {
            return AttendeeAvailability {
                address,
                intervals,
                schedule_status: status,
                unknown: true,
            };
        }
        AttendeeAvailability {
            address,
            intervals,
            schedule_status: status,
            unknown: false,
        }
    }
}

/// The status code before the first `;` in a `request-status` value
/// (`2.0;Success` -> `2.0`), as an RFC 6638 §3.2.9 `SCHEDULE-STATUS`.
fn parse_request_status(value: &str) -> ScheduleStatus {
    ScheduleStatus::new(value.split(';').next().unwrap_or(value).trim())
}

/// Parses the `FREEBUSY` periods out of a `VFREEBUSY` calendar (the
/// `calendar-data` of a schedule response), mapping each `FBTYPE` onto the
/// five-state show-as. Only `FREEBUSY` lines are read; the `ATTENDEE`/
/// `ORGANIZER` lines aren't needed here since the enclosing XML response
/// already named the recipient.
pub fn parse_vfreebusy_intervals(ics: &str) -> Vec<FreeBusyInterval> {
    let unfolded = unfold(ics);
    let mut in_freebusy = false;
    let mut out = Vec::new();
    for line in unfolded {
        let trimmed = line.trim_end_matches('\r');
        if trimmed.eq_ignore_ascii_case("BEGIN:VFREEBUSY") {
            in_freebusy = true;
            continue;
        }
        if trimmed.eq_ignore_ascii_case("END:VFREEBUSY") {
            in_freebusy = false;
            continue;
        }
        if !in_freebusy {
            continue;
        }
        let Some(colon) = trimmed.find(':') else { continue };
        let (before, value) = trimmed.split_at(colon);
        let value = &value[1..];
        let name = before.split(';').next().unwrap_or(before);
        if !name.eq_ignore_ascii_case("FREEBUSY") {
            continue;
        }
        let status = parse_fbtype(before);
        for period in value.split(',') {
            if let Some((start, end)) = parse_period(period.trim()) {
                out.push(FreeBusyInterval { start, end, status });
            }
        }
    }
    out
}

/// `FBTYPE` parameter of a `FREEBUSY` line (defaults to `BUSY`).
fn parse_fbtype(line_before_value: &str) -> BusyStatus {
    for param in line_before_value.split(';').skip(1) {
        let Some((key, val)) = param.split_once('=') else { continue };
        if !key.eq_ignore_ascii_case("FBTYPE") {
            continue;
        }
        return match val.trim().trim_matches('"').to_ascii_uppercase().as_str() {
            "FREE" => BusyStatus::Free,
            "BUSY-TENTATIVE" => BusyStatus::Tentative,
            "BUSY-UNAVAILABLE" => BusyStatus::Oof,
            _ => BusyStatus::Busy,
        };
    }
    BusyStatus::Busy
}

/// A single RFC 5545 §3.3.9 period: `start/end` or `start/duration`.
fn parse_period(period: &str) -> Option<(DateTime<Utc>, DateTime<Utc>)> {
    let (start_raw, end_raw) = period.split_once('/')?;
    let start = parse_ical_datetime(start_raw.trim())?;
    let end = if end_raw.trim().starts_with(['P', 'p']) {
        start + parse_ical_duration(end_raw.trim())
    } else {
        parse_ical_datetime(end_raw.trim())?
    };
    Some((start, end))
}

/// Parses a UTC/date/date-time iCalendar value. A floating value is treated
/// as UTC (matching how `ical.rs` treats floating event times).
fn parse_ical_datetime(raw: &str) -> Option<DateTime<Utc>> {
    if let Ok(dt) = NaiveDateTime::parse_from_str(raw, "%Y%m%dT%H%M%SZ") {
        return Some(Utc.from_utc_datetime(&dt));
    }
    if let Ok(dt) = NaiveDateTime::parse_from_str(raw, "%Y%m%dT%H%M%S") {
        return Some(Utc.from_utc_datetime(&dt));
    }
    if let Ok(date) = NaiveDate::parse_from_str(raw, "%Y%m%d") {
        return Some(Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0)?));
    }
    None
}

/// RFC 5545 line unfolding: a line beginning with a space or tab continues
/// the previous one.
fn unfold(ics: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for raw in ics.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if let Some(rest) = line.strip_prefix(' ').or_else(|| line.strip_prefix('\t')) {
            if let Some(last) = out.last_mut() {
                last.push_str(rest);
                continue;
            }
        }
        out.push(line.to_string());
    }
    out
}

/// Strips a leading `mailto:` (case-insensitive) from a calendar-user address.
fn strip_mailto(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.len() >= 7 && trimmed[..7].eq_ignore_ascii_case("mailto:") {
        trimmed[7..].to_string()
    } else {
        trimmed.to_string()
    }
}

fn escape_text(value: &str) -> String {
    value.replace('\\', "\\\\").replace(';', "\\;").replace(',', "\\,").replace('\n', "\\n")
}

fn resolve_entity(r: &quick_xml::events::BytesRef) -> Option<String> {
    match r.resolve_char_ref() {
        Ok(Some(c)) => Some(c.to_string()),
        Ok(None) => r.decode().ok().and_then(|name| match name.as_ref() {
            "amp" => Some("&".to_string()),
            "lt" => Some("<".to_string()),
            "gt" => Some(">".to_string()),
            "apos" => Some("'".to_string()),
            "quot" => Some("\"".to_string()),
            _ => None,
        }),
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attendee(address: &str) -> EmailAddress {
        EmailAddress::new(address)
    }

    const SCHEDULE_RESPONSE: &str = r#"<?xml version="1.0" encoding="utf-8" ?>
<C:schedule-response xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
<C:response>
<C:recipient><D:href>mailto:wilfredo@example.com</D:href></C:recipient>
<C:request-status>2.0;Success</C:request-status>
<C:calendar-data>BEGIN:VCALENDAR
VERSION:2.0
METHOD:REPLY
BEGIN:VFREEBUSY
UID:4FD3AD926350
DTSTART:20090602T000000Z
DTEND:20090604T000000Z
ORGANIZER:mailto:cyrus@example.com
ATTENDEE:mailto:wilfredo@example.com
FREEBUSY;FBTYPE=BUSY:20090602T110000Z/20090602T120000Z
FREEBUSY;FBTYPE=BUSY-TENTATIVE:20090603T170000Z/20090603T180000Z
END:VFREEBUSY
END:VCALENDAR
</C:calendar-data>
</C:response>
<C:response>
<C:recipient><D:href>mailto:missing@example.org</D:href></C:recipient>
<C:request-status>3.7;Invalid calendar user</C:request-status>
</C:response>
</C:schedule-response>"#;

    #[test]
    fn request_lists_organizer_and_attendees() {
        let start = "2026-10-15T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let end = "2026-10-16T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let body = build_vfreebusy_request(&attendee("me@example.com"), &[attendee("a@example.com"), attendee("b@example.com")], start, end, "uid-1");
        assert!(body.contains("METHOD:REQUEST"));
        assert!(body.contains("ORGANIZER:mailto:me@example.com"));
        assert!(body.contains("ATTENDEE:mailto:a@example.com"));
        assert!(body.contains("ATTENDEE:mailto:b@example.com"));
        assert!(body.contains("DTSTART:20261015T000000Z"));
        assert!(body.contains("DTEND:20261016T000000Z"));
        assert!(body.contains("UID:uid-1"));
    }

    #[test]
    fn mailto_is_not_doubled() {
        let start = "2026-10-15T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let end = "2026-10-16T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let body = build_vfreebusy_request(&attendee("mailto:me@example.com"), &[], start, end, "u");
        assert!(body.contains("ORGANIZER:mailto:me@example.com"));
        assert!(!body.contains("mailto:mailto:"));
    }

    #[test]
    fn parses_the_schedule_response_and_maps_fbtype() {
        let availabilities = parse_schedule_response(SCHEDULE_RESPONSE).unwrap();
        assert_eq!(availabilities.len(), 2);

        let wilfredo = &availabilities[0];
        assert_eq!(wilfredo.address.address, "wilfredo@example.com");
        assert!(!wilfredo.unknown);
        assert_eq!(wilfredo.intervals.len(), 2);
        assert_eq!(wilfredo.intervals[0].status, BusyStatus::Busy);
        assert_eq!(wilfredo.intervals[1].status, BusyStatus::Tentative);
        assert!(wilfredo.schedule_status.as_ref().unwrap().is_success());

        let missing = &availabilities[1];
        assert_eq!(missing.address.address, "missing@example.org");
        assert!(missing.unknown);
        assert!(missing.intervals.is_empty());
    }

    #[test]
    fn parses_duration_form_and_folded_lines() {
        let ics = "BEGIN:VFREEBUSY\r\nFREEBUSY:20260101T090000Z/PT1H\r\nFREEBUSY;FBTYPE=FREE:20260101T110000Z/20260101T120000Z\r\nEND:VFREEBUSY\r\n";
        let intervals = parse_vfreebusy_intervals(ics);
        assert_eq!(intervals.len(), 2);
        assert_eq!(intervals[0].end.format("%H").to_string(), "10");
        assert_eq!(intervals[1].status, BusyStatus::Free);
    }

    /// The checked-in RFC 6638 §B.5 busy-time response (OUTLOOK_PARITY.md
    /// item 0.8's fixture corpus): three recipients, one of them an invalid
    /// calendar user with no `calendar-data`.
    #[test]
    fn parses_the_checked_in_schedule_response_fixture() {
        let xml = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-fixtures/outlook/freebusy-response.xml"));
        let availabilities = parse_schedule_response(xml).unwrap();
        assert_eq!(availabilities.len(), 3);

        assert_eq!(availabilities[0].address.address, "wilfredo@example.com");
        assert_eq!(availabilities[0].intervals.len(), 2);
        assert!(!availabilities[0].unknown);

        assert_eq!(availabilities[1].address.address, "bernard@example.net");
        assert_eq!(availabilities[1].intervals.len(), 3);

        assert!(availabilities[2].unknown, "an invalid calendar user has no data");
        assert_eq!(availabilities[2].address.address, "mike@example.org");
    }

    #[test]
    fn builds_a_request_that_round_trips_through_the_parser() {
        let request = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-fixtures/outlook/freebusy-request.ics"));
        // The fixture's ORGANIZER/ATTENDEE lines are what the builder emits,
        // modulo the freshly generated UID/DTSTAMP.
        assert!(request.contains("METHOD:REQUEST"));
        assert!(request.contains("ORGANIZER;CN=\"Cyrus Daboo\":mailto:cyrus@example.com"));
        assert!(request.contains("ATTENDEE;CN=\"Mike Douglass\":mailto:mike@example.org"));
    }
}
