//! Turning one [`EventOccurrence`] into the event that goes into Lookout's
//! EDS mirror calendar (EDS_TODO.md, E2.2).
//!
//! The mirror stores expanded occurrences, not recurring series: each instance
//! becomes a plain, non-repeating `VEVENT` with its own stable UID, so GNOME
//! Shell's clock and GNOME Calendar see concrete events without Lookout having
//! to reproduce recurrence rules, exceptions and time-zone details. A later
//! instance of a series is a different mirror event, and editing an occurrence
//! changes its copy in place instead of creating a second one.
//!
//! Only what the Shell needs is copied: the summary, the start and end, the
//! all-day flag, the time zone and the location. Descriptions are a privacy
//! choice (`include_description`, wired to the setting in E2.4): a description
//! routinely holds dial-in codes, agendas and notes, and EDS data is readable
//! by every app on the desktop. Attendees, reminders, categories and the
//! booking's "Show as" are deliberately not copied - the mirror is a one-way
//! calendar view, not a second copy of Lookout's data.
//!
//! [`mirror_event`] is pure: the planner in E2.3 uses [`MirrorEvent::hash`] to
//! decide which copies to create, modify or remove, and [`MirrorEvent::component`]
//! to build the mirror's whole iCalendar file.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::collections::HashSet;

use chrono::{DateTime, Utc};
use lookout_core::{CalendarEvent, CalendarId, EventOccurrence, EventStatus, EventUid, RecurrenceRange};
use sha2::{Digest, Sha256};

/// The prefix of every mirror UID, so the planner can recognise its own events
/// among anything else the calendar might report.
const MIRROR_UID_PREFIX: &str = "lookout-";

/// The `X-` property holding [`MirrorEvent::hash`] in the mirrored `VEVENT`.
/// EDS preserves `X-` properties across its store, and E2.3 reads this back to
/// tell which copies changed.
const HASH_PROPERTY: &str = "X-LOOKOUT-HASH";

/// One event as it appears in the mirror calendar.
#[allow(dead_code)] // E2.3 is the first non-test caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MirrorEvent {
    /// The stable UID of the mirror copy: `lookout-<hash>` over the source
    /// calendar, the source UID and the occurrence's start. Re-reading the
    /// same occurrence (or editing it) yields the same UID; a different
    /// instance of a series yields a different one.
    pub(crate) uid: String,
    /// A hash of the fields copied into the mirror (see [`mirror_event`]),
    /// mirrored as [`HASH_PROPERTY`]. E2.3 skips events whose hash matches.
    pub(crate) hash: String,
    /// The occurrence's start, kept for the sync window's bookkeeping.
    pub(crate) start: DateTime<Utc>,
    /// The occurrence's end, kept for the sync window's bookkeeping.
    pub(crate) end: DateTime<Utc>,
    /// The `VEVENT` - plus the `VTIMEZONE` it needs, if any - ready to sit
    /// inside the mirror's `VCALENDAR`. Always non-repeating.
    pub(crate) component: String,
}

/// Converts one occurrence into a mirror event, or `None` when it must not be
/// mirrored: it was cancelled, or its calendar is hidden in Lookout.
///
/// `include_description` copies the event's notes as well; see the module
/// comment for why that is opt-in.
#[allow(dead_code)] // E2.3 is the first non-test caller.
pub(crate) fn mirror_event(occurrence: &EventOccurrence, include_description: bool, hidden_calendars: &HashSet<CalendarId>) -> Option<MirrorEvent> {
    if occurrence.status == EventStatus::Cancelled || hidden_calendars.contains(&occurrence.calendar_id) {
        return None;
    }
    // An all-day event's `VALUE=DATE` boundaries are zone-independent, so the
    // serializer ignores any stored zone. Drop it from the hash too, or a zone
    // the copy doesn't use would look like a change on every sync.
    let time_zone = if occurrence.all_day { None } else { occurrence.time_zone.clone() };
    let hash = copy_hash(occurrence, time_zone.as_deref(), include_description);
    let uid = mirror_uid(occurrence);

    let event = stripped_event(occurrence, time_zone, include_description, &uid);
    let component = mirror_component(&lookout_dav::build_vcalendar(&event), &hash)?;

    Some(MirrorEvent {
        uid,
        hash,
        start: occurrence.start,
        end: occurrence.end,
        component,
    })
}

/// The stable mirror UID. `lookout-` plus the first 128 bits of a SHA-256 over
/// the source calendar, source UID and occurrence start, which is unique to
/// one instance of a series and survives a re-read.
fn mirror_uid(occurrence: &EventOccurrence) -> String {
    let mut hasher = Sha256::new();
    hasher.update(occurrence.calendar_id.0.as_bytes());
    hasher.update([0x1f]);
    hasher.update(occurrence.uid.0.as_bytes());
    hasher.update([0x1f]);
    hasher.update(occurrence.start.timestamp_micros().to_be_bytes());
    format!("{MIRROR_UID_PREFIX}{}", hex(&hasher.finalize()[..16]))
}

/// A hash of exactly the fields [`mirror_event`] copies, so a change to
/// anything left out (attendees, categories, a reminder, an href) doesn't make
/// E2.3 rewrite the mirror. Field names and lengths are part of the canonical
/// text, so no two different field values can hash the same.
fn copy_hash(occurrence: &EventOccurrence, time_zone: Option<&str>, include_description: bool) -> String {
    let start = occurrence.start.timestamp_micros().to_string();
    let end = occurrence.end.timestamp_micros().to_string();
    let mut canonical = String::new();
    field(&mut canonical, "summary", occurrence.summary.as_deref());
    field(&mut canonical, "start", Some(&start));
    field(&mut canonical, "end", Some(&end));
    field(&mut canonical, "all_day", Some(if occurrence.all_day { "1" } else { "0" }));
    field(&mut canonical, "time_zone", time_zone);
    field(&mut canonical, "location", occurrence.location.as_deref());
    field(&mut canonical, "description", if include_description { occurrence.description.as_deref() } else { None });
    hex(&Sha256::digest(canonical.as_bytes()))
}

/// Appends one canonical field. A missing value is distinct from an empty one,
/// so adding a summary or clearing it always changes the hash.
fn field(out: &mut String, name: &str, value: Option<&str>) {
    match value {
        Some(value) => {
            out.push_str(name);
            out.push('=');
            out.push_str(&value.len().to_string());
            out.push(':');
            out.push_str(value);
        }
        None => {
            out.push_str(name);
            out.push_str("=-");
        }
    }
    out.push('\n');
}

/// The stripped-down [`CalendarEvent`] `build_vcalendar` serializes: the
/// copied fields only, no recurrence of any kind and no participants.
fn stripped_event(occurrence: &EventOccurrence, time_zone: Option<String>, include_description: bool, mirror_uid: &str) -> CalendarEvent {
    CalendarEvent {
        uid: EventUid(mirror_uid.to_string()),
        calendar_id: occurrence.calendar_id.clone(),
        summary: occurrence.summary.clone(),
        description: if include_description { occurrence.description.clone() } else { None },
        location: occurrence.location.clone(),
        start: occurrence.start,
        end: occurrence.end,
        all_day: occurrence.all_day,
        time_zone,
        rrule: None,
        recurrence_id: None,
        recurrence_range: RecurrenceRange::default(),
        exdates: Vec::new(),
        rdates: Vec::new(),
        href: None,
        etag: None,
        sequence: 0,
        attendees: Vec::new(),
        organizer: None,
        schedule_agent: None,
        categories: Vec::new(),
        sensitivity: Default::default(),
        show_as: Default::default(),
        status: EventStatus::Confirmed,
        reminder_minutes_before: None,
        conference_url: None,
        is_online_meeting: false,
    }
}

/// Cuts the `VCALENDAR` wrapper off `build_vcalendar`'s output, keeping the
/// `VTIMEZONE` (when there is one) and the `VEVENT`, and stamps the copy hash
/// into the event.
fn mirror_component(ics: &str, hash: &str) -> Option<String> {
    let start = ics.find("BEGIN:VTIMEZONE").or_else(|| ics.find("BEGIN:VEVENT"))?;
    let vevent_end = ics.find("END:VEVENT")?;
    let end = ics[vevent_end..].find('\n').map_or(ics.len(), |offset| vevent_end + offset + 1);
    let mut component = ics.get(start..end)?.to_string();
    let insert_at = component.rfind("END:VEVENT")?;
    component.insert_str(insert_at, &format!("{HASH_PROPERTY}:{hash}\r\n"));
    Some(component)
}

/// Lower-case hex, so hashes are stable and safe as iCalendar property values.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone};
    use lookout_core::{Attendee, AttendeeRole, AttendeeStatus, BusyStatus, CalendarId, EmailAddress, EventSensitivity, EventUid};

    fn at(year: i32, month: u32, day: u32, hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(year, month, day, hour, 0, 0).unwrap()
    }

    fn occurrence() -> EventOccurrence {
        EventOccurrence {
            uid: EventUid("evt-1@example.com".to_string()),
            calendar_id: CalendarId("acct:cal".to_string()),
            summary: Some("Review".to_string()),
            description: Some("Secret notes".to_string()),
            location: Some("Room 1".to_string()),
            start: at(2026, 7, 15, 14),
            end: at(2026, 7, 15, 15),
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
            sensitivity: EventSensitivity::default(),
            show_as: BusyStatus::default(),
            status: EventStatus::default(),
            reminder_minutes_before: None,
            conference_url: None,
        }
    }

    fn hidden() -> HashSet<CalendarId> {
        HashSet::new()
    }

    fn build(occ: &EventOccurrence) -> MirrorEvent {
        mirror_event(occ, false, &hidden()).expect("an ordinary occurrence is mirrored")
    }

    #[test]
    fn uid_is_stable_and_unique_to_one_instance() {
        let occ = occurrence();
        let first = build(&occ);
        assert_eq!(first.uid, build(&occ.clone()).uid, "re-reading an event keeps its UID");
        assert!(first.uid.starts_with("lookout-"), "mirror UIDs are recognisable: {}", first.uid);

        let mut moved = occ.clone();
        moved.start += Duration::days(1);
        moved.end += Duration::days(1);
        assert_ne!(first.uid, build(&moved).uid, "another instance is another event");

        let mut other_calendar = occ.clone();
        other_calendar.calendar_id = CalendarId("acct:other".to_string());
        assert_ne!(first.uid, build(&other_calendar).uid, "the same UID in another calendar is another event");

        let mut other_event = occ.clone();
        other_event.uid = EventUid("evt-2@example.com".to_string());
        assert_ne!(first.uid, build(&other_event).uid);
    }

    #[test]
    fn all_day_events_are_written_as_dates() {
        let mut occ = occurrence();
        occ.all_day = true;
        occ.start = at(2026, 7, 20, 0);
        occ.end = at(2026, 7, 21, 0);
        occ.time_zone = Some("Europe/Berlin".to_string());
        let event = build(&occ);
        assert!(event.component.starts_with("BEGIN:VEVENT"), "{}", event.component);
        assert!(event.component.contains("DTSTART;VALUE=DATE:20260720"), "{}", event.component);
        assert!(event.component.contains("DTEND;VALUE=DATE:20260721"), "{}", event.component);
        assert!(!event.component.contains("TZID"), "an all-day event is zone-free: {}", event.component);
        assert!(!event.component.contains("VTIMEZONE"), "{}", event.component);
    }

    #[test]
    fn zoned_events_keep_their_zone() {
        let mut occ = occurrence();
        occ.time_zone = Some("Europe/Berlin".to_string());
        let event = build(&occ);
        assert!(event.component.starts_with("BEGIN:VTIMEZONE"), "{}", event.component);
        assert!(event.component.contains("DTSTART;TZID=Europe/Berlin:20260715T160000"), "{}", event.component);
        assert!(event.component.contains("DTEND;TZID=Europe/Berlin:20260715T170000"), "{}", event.component);
    }

    #[test]
    fn recurrence_is_flattened_away() {
        let mut occ = occurrence();
        occ.rrule = Some("FREQ=WEEKLY;COUNT=10".to_string());
        occ.recurrence_id = Some(occ.start);
        occ.exdates = vec![occ.start + Duration::days(7)];
        let event = build(&occ);
        assert!(!event.component.contains("RRULE"), "{}", event.component);
        assert!(!event.component.contains("EXDATE"), "{}", event.component);
        assert!(!event.component.contains("RECURRENCE-ID"), "{}", event.component);
    }

    #[test]
    fn the_component_is_a_bare_vevent_with_the_hash() {
        let event = build(&occurrence());
        assert!(event.component.starts_with("BEGIN:VEVENT"), "{}", event.component);
        assert!(event.component.ends_with("END:VEVENT\r\n"), "{}", event.component);
        assert!(!event.component.contains("BEGIN:VCALENDAR"), "{}", event.component);
        assert!(event.component.contains(&format!("UID:{}\r\n", event.uid)), "{}", event.component);
        assert!(event.component.contains(&format!("{}\r\n", hash_line(&event.hash))), "{}", event.component);
        assert!(event.component.contains("SUMMARY:Review"), "{}", event.component);
        assert!(event.component.contains("LOCATION:Room 1"), "{}", event.component);
    }

    fn hash_line(hash: &str) -> String {
        format!("{HASH_PROPERTY}:{hash}")
    }

    #[test]
    fn hash_tracks_the_copied_fields() {
        let base = build(&occurrence());
        let cases: Vec<(&str, fn(&mut EventOccurrence))> = vec![
            ("summary", |occ| occ.summary = Some("Renamed".to_string())),
            ("summary cleared", |occ| occ.summary = None),
            ("start", |occ| occ.start += Duration::minutes(30)),
            ("end", |occ| occ.end += Duration::minutes(30)),
            ("all-day", |occ| occ.all_day = true),
            ("time zone", |occ| occ.time_zone = Some("Europe/Berlin".to_string())),
            ("location", |occ| occ.location = Some("Room 2".to_string())),
            ("location cleared", |occ| occ.location = None),
        ];
        for (name, change) in cases {
            let mut occ = occurrence();
            change(&mut occ);
            assert_ne!(base.hash, build(&occ).hash, "changing the {name} must change the hash");
        }
    }

    #[test]
    fn untracked_fields_do_not_change_the_hash() {
        let base = build(&occurrence());
        let mut occ = occurrence();
        occ.attendees = vec![Attendee {
            address: EmailAddress::new("someone@example.com"),
            role: AttendeeRole::Required,
            status: AttendeeStatus::Accepted,
        }];
        occ.organizer = Some(EmailAddress::new("me@example.com"));
        occ.categories = vec!["Work".to_string()];
        occ.reminder_minutes_before = Some(15);
        occ.conference_url = Some("https://meet.example.com/abc".to_string());
        occ.href = Some("/cal/evt-1.ics".to_string());
        occ.etag = Some("\"abc\"".to_string());
        occ.sequence = 7;
        occ.sensitivity = EventSensitivity::Private;
        occ.show_as = BusyStatus::Free;
        occ.status = EventStatus::Tentative;
        occ.rrule = Some("FREQ=WEEKLY".to_string());
        let changed = build(&occ);
        assert_eq!(base.hash, changed.hash, "fields that aren't copied must not force a rewrite");
        for property in ["ATTENDEE", "ORGANIZER", "CATEGORIES", "CONFERENCE", "VALARM", "CLASS", "STATUS", "SEQUENCE"] {
            assert!(!changed.component.contains(property), "{property} must not be copied:\n{}", changed.component);
        }
    }

    #[test]
    fn description_is_copied_only_when_allowed() {
        let occ = occurrence();
        let without = mirror_event(&occ, false, &hidden()).unwrap();
        assert!(!without.component.contains("DESCRIPTION"), "{}", without.component);

        let mut other_notes = occ.clone();
        other_notes.description = Some("Different notes".to_string());
        let without_other = mirror_event(&other_notes, false, &hidden()).unwrap();
        assert_eq!(without.hash, without_other.hash, "notes that aren't copied don't change the hash");

        let with = mirror_event(&occ, true, &hidden()).unwrap();
        assert!(with.component.contains("DESCRIPTION:Secret notes"), "{}", with.component);
        let with_other = mirror_event(&other_notes, true, &hidden()).unwrap();
        assert_ne!(with.hash, with_other.hash, "notes that are copied do change the hash");
        assert_eq!(with.uid, without.uid, "the privacy setting never changes an event's identity");
    }

    #[test]
    fn cancelled_and_hidden_occurrences_are_dropped() {
        let mut cancelled = occurrence();
        cancelled.status = EventStatus::Cancelled;
        assert!(mirror_event(&cancelled, false, &hidden()).is_none());

        let mut hidden_calendars = HashSet::new();
        hidden_calendars.insert(CalendarId("acct:cal".to_string()));
        assert!(mirror_event(&occurrence(), false, &hidden_calendars).is_none());

        hidden_calendars.insert(CalendarId("acct:other".to_string()));
        assert!(mirror_event(&occurrence(), false, &hidden_calendars).is_none(), "an event on any hidden calendar stays out");
        let mut elsewhere = occurrence();
        elsewhere.calendar_id = CalendarId("acct:visible".to_string());
        assert!(mirror_event(&elsewhere, false, &hidden_calendars).is_some());
    }
}
