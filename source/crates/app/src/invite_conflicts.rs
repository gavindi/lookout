//! Conflict detection for incoming invitations (OUTLOOK_PARITY.md item 3.18 /
//! CAL-35): does the time a message invites the user to clash with anything
//! they already have?
//!
//! Pure over the caller's in-memory occurrences, so it is unit-tested without
//! a calendar session or a window. Only blocking (non-`Free`) events count,
//! matching the Scheduling Assistant's busy filter; the invited event itself
//! is excluded by its `UID` so an already-accepted invitation doesn't report
//! a conflict with itself.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use chrono::{DateTime, Utc};
use lookout_core::{BusyStatus, EventOccurrence, FreeBusyInterval};

/// `occurrences` that block `[start, end)` - excluding `invite_uid` and
/// anything marked `Free` (transparent), in the order given.
pub fn conflicting_occurrences<'a>(occurrences: &[&'a EventOccurrence], start: DateTime<Utc>, end: DateTime<Utc>, invite_uid: &str) -> Vec<&'a EventOccurrence> {
    occurrences
        .iter()
        .copied()
        .filter(|occurrence| occurrence.show_as != BusyStatus::Free)
        .filter(|occurrence| occurrence.uid.0.as_str() != invite_uid)
        .filter(|occurrence| occurrence.start < end && start < occurrence.end)
        .collect()
}

/// The blocking occurrences as free/busy intervals (for the invite card's mini
/// day timeline), excluding `invite_uid`. Not clipped to a day - the timeline
/// widget clips to whatever day it draws.
pub fn busy_intervals(occurrences: &[&EventOccurrence], invite_uid: &str) -> Vec<FreeBusyInterval> {
    occurrences
        .iter()
        .copied()
        .filter(|occurrence| occurrence.show_as != BusyStatus::Free)
        .filter(|occurrence| occurrence.uid.0.as_str() != invite_uid)
        .map(|occurrence| FreeBusyInterval {
            start: occurrence.start,
            end: occurrence.end,
            status: occurrence.show_as,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use lookout_core::{CalendarId, EventUid};

    fn occurrence(uid: &str, start_hour: u32, end_hour: u32, show_as: BusyStatus) -> EventOccurrence {
        let at = |hour: u32| Utc.with_ymd_and_hms(2026, 10, 15, hour, 0, 0).unwrap();
        EventOccurrence {
            uid: EventUid(uid.to_string()),
            calendar_id: CalendarId("cal".to_string()),
            summary: Some(uid.to_string()),
            description: None,
            location: None,
            start: at(start_hour),
            end: at(end_hour),
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
            show_as,
            status: Default::default(),
            reminder_minutes_before: None,
            conference_url: None,
        }
    }

    fn at(hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 15, hour, 0, 0).unwrap()
    }

    #[test]
    fn overlapping_busy_events_conflict() {
        let busy = occurrence("busy", 10, 11, BusyStatus::Busy);
        let occurrences = vec![&busy];
        let conflicts = conflicting_occurrences(&occurrences, at(10), at(11), "invite");
        assert_eq!(conflicts.len(), 1);
    }

    #[test]
    fn back_to_back_events_do_not_conflict() {
        let busy = occurrence("busy", 9, 10, BusyStatus::Busy);
        let occurrences = vec![&busy];
        // The proposed span starts exactly when the busy one ends.
        assert!(conflicting_occurrences(&occurrences, at(10), at(11), "invite").is_empty());
        // ...and ends exactly when it starts.
        assert!(conflicting_occurrences(&occurrences, at(8), at(9), "invite").is_empty());
    }

    #[test]
    fn free_events_and_the_invite_itself_are_ignored() {
        let free = occurrence("free", 10, 11, BusyStatus::Free);
        let invite = occurrence("invite", 10, 11, BusyStatus::Busy);
        let occurrences = vec![&free, &invite];
        assert!(conflicting_occurrences(&occurrences, at(10), at(11), "invite").is_empty());
    }

    #[test]
    fn tentative_and_oof_block_but_free_does_not() {
        let tentative = occurrence("t", 10, 11, BusyStatus::Tentative);
        let oof = occurrence("o", 11, 12, BusyStatus::Oof);
        let occurrences = vec![&tentative, &oof];
        assert_eq!(conflicting_occurrences(&occurrences, at(10), at(12), "invite").len(), 2);
        assert_eq!(busy_intervals(&occurrences, "invite").len(), 2);
    }

    #[test]
    fn busy_intervals_carry_the_show_as_status() {
        let tentative = occurrence("t", 10, 11, BusyStatus::Tentative);
        let intervals = busy_intervals(&[&tentative], "invite");
        assert_eq!(intervals[0].status, BusyStatus::Tentative);
    }
}
