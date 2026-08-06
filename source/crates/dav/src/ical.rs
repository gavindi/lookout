use chrono::{DateTime, Duration, NaiveTime, TimeZone, Utc};
use icalendar::{Calendar, CalendarComponent, CalendarDateTime, Component, DatePerhapsTime, EventLike};
use lookout_core::{CalendarEvent, CalendarId, EventUid};

/// Parses every VEVENT out of a raw iCalendar document (as returned by a
/// CalDAV `calendar-data` property) into [`CalendarEvent`]s, attaching no
/// href/etag metadata (those live in the enclosing `multistatus` response,
/// not the `calendar-data`). Prefer [`parse_vevents_with_meta`] when the
/// caller has the response's `<href>`/`getetag` at hand.
pub fn parse_vevents(calendar_id: &CalendarId, ics: &str) -> Vec<CalendarEvent> {
    parse_vevents_with_meta(calendar_id, ics, None, None)
}

/// Like [`parse_vevents`], but stamps every parsed event with the given
/// resource href/etag - the CalDAV `multistatus` `<href>` and `<getetag>`
/// that accompany a `calendar-data`, which the write path needs to PUT/DELETE
/// the resource back.
///
/// Known, accepted simplifications for this pass (consistent with
/// recurring-edit-scopes already being an out-of-scope Phase 3 TODO item):
/// - A VEVENT with `RECURRENCE-ID` (a per-occurrence override of one
///   instance of a recurring series) is parsed as a standalone event rather
///   than merged against its master - it may double-render alongside the
///   RRULE-expanded occurrence it overrides, or show stale data.
/// - `TZID`-qualified `DTSTART`/`DTEND` are resolved to UTC via
///   `CalendarDateTime::try_into_utc` (icalendar's own `chrono-tz`
///   integration, enabled via this crate's `chrono-tz` feature). A
///   "floating" (no `Z`, no `TZID`) datetime - which icalendar's own docs
///   call "a red flag" - falls back to being treated as UTC, with a
///   warning, rather than silently dropping the event.
pub fn parse_vevents_with_meta(calendar_id: &CalendarId, ics: &str, href: Option<&str>, etag: Option<&str>) -> Vec<CalendarEvent> {
    let calendar: Calendar = match ics.parse() {
        Ok(cal) => cal,
        Err(e) => {
            tracing::warn!("failed to parse iCalendar data: {e}");
            return Vec::new();
        }
    };

    calendar
        .components
        .iter()
        .filter_map(|c| match c {
            CalendarComponent::Event(event) => convert_event(calendar_id, event),
            _ => None,
        })
        .map(|mut event| {
            event.href = href.map(str::to_string);
            event.etag = etag.map(str::to_string);
            event
        })
        .collect()
}

fn convert_event(calendar_id: &CalendarId, event: &icalendar::Event) -> Option<CalendarEvent> {
    let uid = event.get_uid()?.to_string();
    let start = event.get_start()?;
    let (start_utc, all_day) = to_utc(&start)?;

    let end_utc = if let Some(end) = event.get_end() {
        to_utc(&end)?.0
    } else if let Some(duration_str) = event.property_value("DURATION") {
        start_utc + parse_ical_duration(duration_str)
    } else if all_day {
        // RFC 5545 §3.6.1: no DTEND/DURATION on an all-day DTSTART means a
        // single-day span.
        start_utc + Duration::days(1)
    } else {
        // ...and no DTEND/DURATION on a DATE-TIME DTSTART means a
        // zero-length event.
        start_utc
    };

    Some(CalendarEvent {
        uid: EventUid(uid),
        calendar_id: calendar_id.clone(),
        summary: event.get_summary().map(|s| s.to_string()),
        description: event.get_description().map(|s| s.to_string()),
        location: event.get_location().map(|s| s.to_string()),
        start: start_utc,
        end: end_utc,
        all_day,
        rrule: event.property_value("RRULE").map(|s| s.to_string()),
        // The parse side has no href/etag (those come from the enclosing
        // `multistatus` `<href>`/`getetag`, not the `calendar-data`) - the
        // caller fills them in after the fact via
        // [`parse_vevents_with_meta`].
        href: None,
        etag: None,
    })
}

fn to_utc(dpt: &DatePerhapsTime) -> Option<(DateTime<Utc>, bool)> {
    match dpt {
        DatePerhapsTime::Date(date) => Some((Utc.from_utc_datetime(&date.and_time(NaiveTime::MIN)), true)),
        DatePerhapsTime::DateTime(cdt) => {
            let utc = cdt.try_into_utc().or_else(|| match cdt {
                CalendarDateTime::Floating(naive) => {
                    tracing::warn!("event has a floating (no timezone) date-time; treating as UTC");
                    Some(Utc.from_utc_datetime(naive))
                }
                _ => None,
            })?;
            Some((utc, false))
        }
    }
}

/// Parses an RFC 5545 `DURATION` value (e.g. `"PT1H30M"`) into a
/// [`chrono::Duration`]. RFC 5545's `DURATION` value type never actually
/// carries year/month components (only week, or day+time) - but the
/// underlying `iso8601` parser accepts the fuller ISO 8601 grammar, so a
/// year/month component (if a non-conformant server ever emits one) is
/// approximated as 365/30 days respectively rather than rejected outright.
fn parse_ical_duration(raw: &str) -> Duration {
    match iso8601::duration(raw) {
        Ok(iso8601::Duration::Weeks(w)) => Duration::weeks(w as i64),
        Ok(iso8601::Duration::YMDHMS {
            year,
            month,
            day,
            hour,
            minute,
            second,
            millisecond,
        }) => {
            Duration::days(i64::from(year) * 365 + i64::from(month) * 30 + i64::from(day))
                + Duration::hours(i64::from(hour))
                + Duration::minutes(i64::from(minute))
                + Duration::seconds(i64::from(second))
                + Duration::milliseconds(i64::from(millisecond))
        }
        Err(e) => {
            tracing::warn!("failed to parse DURATION {raw:?}: {e}; treating event as zero-length");
            Duration::zero()
        }
    }
}

/// Serializes a [`CalendarEvent`] into a single-VEVENT iCalendar document
/// suitable for a CalDAV `PUT` (the create/update body). Round-trips through
/// [`parse_vevents`]: UID/DTSTART/DTEND are preserved, all-day events become
/// `VALUE=DATE` values and timed events UTC `DATE-TIME`, and an existing RRULE
/// is carried over verbatim so editing a recurring master edits the whole
/// series rather than flattening it into a one-off (per-occurrence overrides
/// remain out of scope, see the parse-side note on [`parse_vevents_with_meta`]).
///
/// The `icalendar` builder emits RFC 5545-correct CRLF line folding and
/// escaping for us; `DTSTAMP` is generated fresh on every build (the editor
/// intentionally never edits it).
pub fn build_vcalendar(event: &CalendarEvent) -> String {
    let mut calendar = icalendar::Calendar::new();
    let mut vevent = icalendar::Event::new();

    vevent.uid(&event.uid.0);
    if let Some(summary) = &event.summary {
        vevent.summary(summary);
    }
    if let Some(description) = &event.description {
        vevent.description(description);
    }
    if let Some(location) = &event.location {
        vevent.location(location);
    }
    if event.all_day {
        // RFC 5545 §3.6.1: all-day DTSTART/DTEND are DATE values, and DTEND is
        // exclusive (a one-day event is `DTSTART;VALUE=DATE:...` /
        // `DTEND;VALUE=DATE:...+1`), matching what `convert_event`'s parser
        // reconstructs (`end - start == 1 day` round-trips exactly).
        vevent.all_day(event.start.date_naive());
        vevent.ends(event.end.date_naive());
    } else {
        vevent.starts(event.start);
        vevent.ends(event.end);
    }
    if let Some(rrule) = &event.rrule {
        // `EventLike::recurrence` would need a parsed RRule; we already hold
        // the raw RFC 5545 `RECUR` string the master was stored with, so put
        // it back verbatim to avoid a parse/re-serialize round trip.
        vevent.append_property(icalendar::Property::new("RRULE", rrule.as_str()));
    }

    calendar.push(vevent.done());
    format!("{calendar}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cal_id() -> CalendarId {
        CalendarId("test-account:test-calendar".to_string())
    }

    #[test]
    fn parses_plain_datetime_event() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\nBEGIN:VEVENT\r\nUID:plain-1@example.com\r\nDTSTAMP:20260101T000000Z\r\nDTSTART:20260715T140000Z\r\nDTEND:20260715T150000Z\r\nSUMMARY:Plain meeting\r\nLOCATION:Room 1\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let events = parse_vevents(&cal_id(), ics);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].uid.0, "plain-1@example.com");
        assert_eq!(events[0].summary.as_deref(), Some("Plain meeting"));
        assert_eq!(events[0].location.as_deref(), Some("Room 1"));
        assert!(!events[0].all_day);
        assert_eq!(events[0].start, "2026-07-15T14:00:00Z".parse::<DateTime<Utc>>().unwrap());
        assert_eq!(events[0].end, "2026-07-15T15:00:00Z".parse::<DateTime<Utc>>().unwrap());
        assert!(events[0].rrule.is_none());
    }

    #[test]
    fn parses_recurring_event_rrule_string() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\nBEGIN:VEVENT\r\nUID:weekly-1@example.com\r\nDTSTAMP:20260101T000000Z\r\nDTSTART:20260706T090000Z\r\nDTEND:20260706T093000Z\r\nSUMMARY:Weekly standup\r\nRRULE:FREQ=WEEKLY;COUNT=10\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let events = parse_vevents(&cal_id(), ics);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].rrule.as_deref(), Some("FREQ=WEEKLY;COUNT=10"));
    }

    #[test]
    fn parses_all_day_event_as_one_day_span() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\nBEGIN:VEVENT\r\nUID:allday-1@example.com\r\nDTSTAMP:20260101T000000Z\r\nDTSTART;VALUE=DATE:20260720\r\nDTEND;VALUE=DATE:20260721\r\nSUMMARY:Conference day\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let events = parse_vevents(&cal_id(), ics);
        assert_eq!(events.len(), 1);
        assert!(events[0].all_day);
        assert_eq!(events[0].start, "2026-07-20T00:00:00Z".parse::<DateTime<Utc>>().unwrap());
        assert_eq!(events[0].end, "2026-07-21T00:00:00Z".parse::<DateTime<Utc>>().unwrap());
    }

    #[test]
    fn resolves_tzid_qualified_datetime_to_utc() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\nBEGIN:VTIMEZONE\r\nTZID:America/New_York\r\nBEGIN:STANDARD\r\nDTSTART:19701101T020000\r\nTZOFFSETFROM:-0400\r\nTZOFFSETTO:-0500\r\nEND:STANDARD\r\nBEGIN:DAYLIGHT\r\nDTSTART:19700308T020000\r\nTZOFFSETFROM:-0500\r\nTZOFFSETTO:-0400\r\nEND:DAYLIGHT\r\nEND:VTIMEZONE\r\nBEGIN:VEVENT\r\nUID:tzid-1@example.com\r\nDTSTAMP:20260101T000000Z\r\nDTSTART;TZID=America/New_York:20260710T090000\r\nDTEND;TZID=America/New_York:20260710T100000\r\nSUMMARY:NY morning call\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let events = parse_vevents(&cal_id(), ics);
        assert_eq!(events.len(), 1);
        // 2026-07-10 is in EDT (UTC-4), so 09:00 local is 13:00 UTC.
        assert_eq!(events[0].start, "2026-07-10T13:00:00Z".parse::<DateTime<Utc>>().unwrap());
        assert_eq!(events[0].end, "2026-07-10T14:00:00Z".parse::<DateTime<Utc>>().unwrap());
    }

    #[test]
    fn falls_back_to_zero_length_when_no_dtend_or_duration() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\nBEGIN:VEVENT\r\nUID:no-end@example.com\r\nDTSTAMP:20260101T000000Z\r\nDTSTART:20260715T140000Z\r\nSUMMARY:Point in time\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let events = parse_vevents(&cal_id(), ics);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].start, events[0].end);
    }

    #[test]
    fn uses_duration_property_when_no_dtend() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\nBEGIN:VEVENT\r\nUID:dur-1@example.com\r\nDTSTAMP:20260101T000000Z\r\nDTSTART:20260715T140000Z\r\nDURATION:PT1H30M\r\nSUMMARY:Duration-based\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let events = parse_vevents(&cal_id(), ics);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].end - events[0].start, Duration::minutes(90));
    }

    #[test]
    fn malformed_calendar_data_yields_no_events_rather_than_panicking() {
        let events = parse_vevents(&cal_id(), "this is not iCalendar data at all");
        assert!(events.is_empty());
    }

    fn full_event() -> CalendarEvent {
        CalendarEvent {
            uid: EventUid("evt-2@example.com".to_string()),
            calendar_id: cal_id(),
            summary: Some("Team sync".to_string()),
            description: Some("Agenda & notes <html>".to_string()),
            location: Some("Room 3".to_string()),
            start: "2026-07-15T14:00:00Z".parse().unwrap(),
            end: "2026-07-15T15:00:00Z".parse().unwrap(),
            all_day: false,
            rrule: None,
            href: None,
            etag: None,
        }
    }

    #[test]
    fn build_vcalendar_round_trips_a_timed_event() {
        let ics = build_vcalendar(&full_event());
        assert!(ics.starts_with("BEGIN:VCALENDAR\r\n"), "should be CRLF line endings, got: {ics}");
        let events = parse_vevents(&cal_id(), &ics);
        assert_eq!(events.len(), 1);
        let round = &events[0];
        assert_eq!(round.uid, full_event().uid);
        assert_eq!(round.summary, full_event().summary);
        assert_eq!(round.description, full_event().description);
        assert_eq!(round.location, full_event().location);
        assert_eq!(round.start, full_event().start);
        assert_eq!(round.end, full_event().end);
        assert!(!round.all_day);
    }

    #[test]
    fn build_vcalendar_round_trips_an_all_day_event() {
        let mut event = full_event();
        event.all_day = true;
        event.start = "2026-07-20T00:00:00Z".parse().unwrap();
        event.end = "2026-07-21T00:00:00Z".parse().unwrap();
        let ics = build_vcalendar(&event);
        assert!(ics.contains("DTSTART;VALUE=DATE:20260720"), "all-day start should be a DATE value: {ics}");
        assert!(ics.contains("DTEND;VALUE=DATE:20260721"), "all-day end should be a DATE value: {ics}");
        let events = parse_vevents(&cal_id(), &ics);
        assert_eq!(events.len(), 1);
        assert!(events[0].all_day);
        assert_eq!(events[0].start, event.start);
        assert_eq!(events[0].end, event.end);
    }

    #[test]
    fn build_vcalendar_preserves_the_rrule_string() {
        let mut event = full_event();
        event.rrule = Some("FREQ=WEEKLY;COUNT=10".to_string());
        let ics = build_vcalendar(&event);
        assert!(ics.contains("RRULE:FREQ=WEEKLY;COUNT=10"), "RRULE should be serialized verbatim: {ics}");
        let events = parse_vevents(&cal_id(), &ics);
        assert_eq!(events[0].rrule.as_deref(), Some("FREQ=WEEKLY;COUNT=10"));
    }

    #[test]
    fn build_vcalendar_stamps_meta_from_parse_with_meta() {
        let ics = build_vcalendar(&full_event());
        let events = parse_vevents_with_meta(&cal_id(), &ics, Some("/cal/events/evt-2.ics"), Some("\"etag1\""));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].href.as_deref(), Some("/cal/events/evt-2.ics"));
        assert_eq!(events[0].etag.as_deref(), Some("\"etag1\""));
    }
}
