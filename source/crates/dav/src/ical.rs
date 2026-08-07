use chrono::{DateTime, Duration, NaiveTime, TimeZone, Utc};
use icalendar::{
    Alarm, Calendar, CalendarComponent, CalendarDateTime, Class, Component, DatePerhapsTime, EventLike, PartStat, Property, Role, Trigger,
    Attendee as IcalAttendee,
};
use lookout_core::{
    Attendee, AttendeeRole, AttendeeStatus, CalendarEvent, CalendarId, EmailAddress, EventSensitivity, EventTransparency, EventUid,
};

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
        attendees: parse_attendees(event),
        organizer: parse_organizer(event),
        categories: parse_categories(event),
        sensitivity: event.get_class().map(from_ical_class).unwrap_or_default(),
        transparency: match event.property_value("TRANSP") {
            Some("TRANSPARENT") => EventTransparency::Free,
            _ => EventTransparency::Busy,
        },
        conference_url: event.property_value("CONFERENCE").or_else(|| event.get_url()).map(str::to_string),
        reminder_minutes_before: parse_reminder(event),
    })
}

/// Strips a `mailto:`/`MAILTO:` scheme prefix, if present, from a CAL-ADDRESS.
fn strip_mailto(cal_address: &str) -> &str {
    cal_address.strip_prefix("mailto:").or_else(|| cal_address.strip_prefix("MAILTO:")).unwrap_or(cal_address)
}

fn parse_attendees(event: &icalendar::Event) -> Vec<Attendee> {
    event
        .get_attendees()
        .into_iter()
        .map(|a| Attendee {
            address: EmailAddress {
                name: a.cn.clone(),
                address: strip_mailto(&a.cal_address).to_string(),
            },
            role: match a.role {
                Some(Role::OptParticipant) => AttendeeRole::Optional,
                _ => AttendeeRole::Required,
            },
            status: match a.part_stat {
                Some(PartStat::Accepted) => AttendeeStatus::Accepted,
                Some(PartStat::Declined) => AttendeeStatus::Declined,
                Some(PartStat::Tentative) => AttendeeStatus::Tentative,
                _ => AttendeeStatus::NeedsAction,
            },
        })
        .collect()
}

/// `ORGANIZER` isn't given a typed accessor by `icalendar` - read via the
/// same generic `property_value` mechanism used for `RRULE`. The `CN`
/// parameter (display name) isn't exposed through `property_value`, so the
/// parsed organizer only ever carries a bare address.
fn parse_organizer(event: &icalendar::Event) -> Option<EmailAddress> {
    event.property_value("ORGANIZER").map(|v| EmailAddress::new(strip_mailto(v).to_string()))
}

/// `CATEGORIES` is one of `icalendar`'s hard-coded multi-value properties
/// (RFC 5545 §3.8.1.2 allows it to repeat), so it lives in
/// `multi_properties`, not the single-valued `properties()` map -
/// `property_value("CATEGORIES")` always returns `None`. Each occurrence's
/// value may itself be a comma-separated list, so every one is split.
fn parse_categories(event: &icalendar::Event) -> Vec<String> {
    event
        .multi_properties()
        .get("CATEGORIES")
        .map(|props| {
            props
                .iter()
                .flat_map(|p| p.value().split(','))
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn from_ical_class(class: Class) -> EventSensitivity {
    match class {
        Class::Public => EventSensitivity::Public,
        Class::Private => EventSensitivity::Private,
        Class::Confidential => EventSensitivity::Confidential,
    }
}

fn to_ical_class(sensitivity: EventSensitivity) -> Class {
    match sensitivity {
        EventSensitivity::Public => Class::Public,
        EventSensitivity::Private => Class::Private,
        EventSensitivity::Confidential => Class::Confidential,
    }
}

/// Reads the first `VALARM` child's `TRIGGER` as "minutes before start".
/// Multiple/repeating alarms and absolute-time or after-start/end triggers
/// aren't modeled - only a single simple "N minutes before" alarm.
///
/// Parses the raw duration value directly rather than going through
/// `icalendar::Trigger::try_from` - that round-trips `chrono::Duration`'s own
/// `Display` output (e.g. `"-PT900S"`) through the `iso8601` crate, which
/// doesn't accept a leading sign and always fails on exactly the negative
/// ("before start") durations a reminder needs (verified against
/// `icalendar` 0.17.13 with a standalone repro). Reusing
/// [`parse_ical_duration`] on the unsigned remainder sidesteps that bug.
fn parse_reminder(event: &icalendar::Event) -> Option<i64> {
    let alarm = event.components().iter().find(|c| c.component_kind() == "VALARM")?;
    let raw = alarm.property_value("TRIGGER")?;
    let unsigned = raw.strip_prefix('-')?;
    Some(parse_ical_duration(unsigned).num_minutes())
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
    append_attendees(&mut vevent, &event.attendees);
    // RFC 5545 §3.6.1 requires an ORGANIZER whenever ATTENDEEs are present;
    // omitted otherwise, and omitted even with attendees if the owning
    // account's own address couldn't be determined (see `CalendarEvent::organizer`).
    if !event.attendees.is_empty() {
        if let Some(organizer) = &event.organizer {
            let mut prop = Property::new("ORGANIZER", format!("mailto:{}", organizer.address));
            if let Some(name) = &organizer.name {
                prop.add_parameter("CN", name);
            }
            vevent.append_property(prop.done());
        }
    }
    if !event.categories.is_empty() {
        vevent.append_property(Property::new("CATEGORIES", event.categories.join(",")));
    }
    // `Class::Public` is the RFC default - omit it so a plain event's
    // serialized ICS doesn't grow noise it didn't have before this field
    // existed.
    if event.sensitivity != EventSensitivity::Public {
        vevent.class(to_ical_class(event.sensitivity));
    }
    if event.transparency == EventTransparency::Free {
        vevent.append_property(Property::new("TRANSP", "TRANSPARENT"));
    }
    if let Some(conference_url) = &event.conference_url {
        vevent.append_property(Property::new("CONFERENCE", conference_url.as_str()));
    }
    if let Some(minutes) = event.reminder_minutes_before {
        vevent.alarm(Alarm::display("Reminder", Trigger::before_start(Duration::minutes(minutes))));
    }

    calendar.push(vevent.done());
    format!("{calendar}")
}

fn append_attendees(vevent: &mut icalendar::Event, attendees: &[Attendee]) {
    for attendee in attendees {
        let mut ical_attendee = IcalAttendee::new(format!("mailto:{}", attendee.address.address))
            .role(match attendee.role {
                AttendeeRole::Required => Role::ReqParticipant,
                AttendeeRole::Optional => Role::OptParticipant,
            })
            .partstat(match attendee.status {
                AttendeeStatus::NeedsAction => PartStat::NeedsAction,
                AttendeeStatus::Accepted => PartStat::Accepted,
                AttendeeStatus::Declined => PartStat::Declined,
                AttendeeStatus::Tentative => PartStat::Tentative,
            });
        if let Some(name) = &attendee.address.name {
            ical_attendee = ical_attendee.cn(name.clone());
        }
        vevent.attendee(ical_attendee);
    }
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
            attendees: Vec::new(),
            organizer: None,
            categories: Vec::new(),
            sensitivity: EventSensitivity::default(),
            transparency: EventTransparency::default(),
            reminder_minutes_before: None,
            conference_url: None,
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

    #[test]
    fn parses_attendees_with_role_and_partstat() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\nBEGIN:VEVENT\r\nUID:att-1@example.com\r\nDTSTAMP:20260101T000000Z\r\nDTSTART:20260715T140000Z\r\nDTEND:20260715T150000Z\r\nSUMMARY:Sync\r\nATTENDEE;ROLE=REQ-PARTICIPANT;PARTSTAT=ACCEPTED;CN=Alice:mailto:alice@example.com\r\nATTENDEE;ROLE=OPT-PARTICIPANT:mailto:bob@example.com\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let events = parse_vevents(&cal_id(), ics);
        assert_eq!(events.len(), 1);
        let attendees = &events[0].attendees;
        assert_eq!(attendees.len(), 2);
        assert_eq!(attendees[0].address.address, "alice@example.com");
        assert_eq!(attendees[0].address.name.as_deref(), Some("Alice"));
        assert_eq!(attendees[0].role, AttendeeRole::Required);
        assert_eq!(attendees[0].status, AttendeeStatus::Accepted);
        assert_eq!(attendees[1].address.address, "bob@example.com");
        assert_eq!(attendees[1].role, AttendeeRole::Optional);
        assert_eq!(attendees[1].status, AttendeeStatus::NeedsAction);
    }

    #[test]
    fn parses_categories_as_comma_split_list() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\nBEGIN:VEVENT\r\nUID:cat-1@example.com\r\nDTSTAMP:20260101T000000Z\r\nDTSTART:20260715T140000Z\r\nDTEND:20260715T150000Z\r\nSUMMARY:Sync\r\nCATEGORIES:Work,Important\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let events = parse_vevents(&cal_id(), ics);
        assert_eq!(events[0].categories, vec!["Work".to_string(), "Important".to_string()]);
    }

    #[test]
    fn parses_class_and_transp() {
        let ics_absent = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\nBEGIN:VEVENT\r\nUID:cls-1@example.com\r\nDTSTAMP:20260101T000000Z\r\nDTSTART:20260715T140000Z\r\nDTEND:20260715T150000Z\r\nSUMMARY:Sync\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let events = parse_vevents(&cal_id(), ics_absent);
        assert_eq!(events[0].sensitivity, EventSensitivity::Public);
        assert_eq!(events[0].transparency, EventTransparency::Busy);

        let ics_set = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\nBEGIN:VEVENT\r\nUID:cls-2@example.com\r\nDTSTAMP:20260101T000000Z\r\nDTSTART:20260715T140000Z\r\nDTEND:20260715T150000Z\r\nSUMMARY:Sync\r\nCLASS:CONFIDENTIAL\r\nTRANSP:TRANSPARENT\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let events = parse_vevents(&cal_id(), ics_set);
        assert_eq!(events[0].sensitivity, EventSensitivity::Confidential);
        assert_eq!(events[0].transparency, EventTransparency::Free);
    }

    #[test]
    fn parses_conference_url_preferring_conference_over_url() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\nBEGIN:VEVENT\r\nUID:conf-1@example.com\r\nDTSTAMP:20260101T000000Z\r\nDTSTART:20260715T140000Z\r\nDTEND:20260715T150000Z\r\nSUMMARY:Sync\r\nCONFERENCE:https://example.com/join/abc\r\nURL:https://example.com/legacy\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let events = parse_vevents(&cal_id(), ics);
        assert_eq!(events[0].conference_url.as_deref(), Some("https://example.com/join/abc"));
    }

    #[test]
    fn parses_valarm_trigger_as_reminder_minutes() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\nBEGIN:VEVENT\r\nUID:alarm-1@example.com\r\nDTSTAMP:20260101T000000Z\r\nDTSTART:20260715T140000Z\r\nDTEND:20260715T150000Z\r\nSUMMARY:Sync\r\nBEGIN:VALARM\r\nACTION:DISPLAY\r\nDESCRIPTION:Reminder\r\nTRIGGER:-PT15M\r\nEND:VALARM\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let events = parse_vevents(&cal_id(), ics);
        assert_eq!(events[0].reminder_minutes_before, Some(15));
    }

    #[test]
    fn build_vcalendar_round_trips_attendees_categories_sensitivity_transparency_conference_and_reminder() {
        let mut event = full_event();
        event.attendees = vec![
            Attendee {
                address: EmailAddress {
                    name: Some("Alice".to_string()),
                    address: "alice@example.com".to_string(),
                },
                role: AttendeeRole::Required,
                status: AttendeeStatus::Accepted,
            },
            Attendee {
                address: EmailAddress::new("bob@example.com"),
                role: AttendeeRole::Optional,
                status: AttendeeStatus::NeedsAction,
            },
        ];
        event.organizer = Some(EmailAddress::new("organizer@example.com"));
        event.categories = vec!["Work".to_string(), "Important".to_string()];
        event.sensitivity = EventSensitivity::Private;
        event.transparency = EventTransparency::Free;
        event.conference_url = Some("https://example.com/join/abc".to_string());
        event.reminder_minutes_before = Some(15);

        let ics = build_vcalendar(&event);
        assert!(ics.contains("ORGANIZER"), "organizer should be written when attendees are present: {ics}");
        let events = parse_vevents(&cal_id(), &ics);
        assert_eq!(events.len(), 1);
        let round = &events[0];
        assert_eq!(round.attendees.len(), 2);
        assert_eq!(round.attendees[0].address.address, "alice@example.com");
        assert_eq!(round.attendees[0].address.name.as_deref(), Some("Alice"));
        assert_eq!(round.attendees[0].status, AttendeeStatus::Accepted);
        assert_eq!(round.attendees[1].role, AttendeeRole::Optional);
        assert_eq!(round.organizer.as_ref().map(|o| o.address.as_str()), Some("organizer@example.com"));
        assert_eq!(round.categories, event.categories);
        assert_eq!(round.sensitivity, EventSensitivity::Private);
        assert_eq!(round.transparency, EventTransparency::Free);
        assert_eq!(round.conference_url, event.conference_url);
        assert_eq!(round.reminder_minutes_before, Some(15));
    }

    #[test]
    fn build_vcalendar_omits_organizer_when_no_attendees() {
        let mut event = full_event();
        event.organizer = Some(EmailAddress::new("organizer@example.com"));
        let ics = build_vcalendar(&event);
        assert!(!ics.contains("ORGANIZER"), "organizer shouldn't be written without attendees: {ics}");
    }

    #[test]
    fn build_vcalendar_writes_default_class_and_transp_only_when_non_default() {
        let ics = build_vcalendar(&full_event());
        assert!(!ics.contains("CLASS"), "default Public sensitivity shouldn't add a CLASS line: {ics}");
        assert!(!ics.contains("TRANSP"), "default Busy transparency shouldn't add a TRANSP line: {ics}");
    }
}
