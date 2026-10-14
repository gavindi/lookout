//! Free/busy availability data, shared by the Scheduling Assistant (CAL-19)
//! and the Schedule view (CAL-03).
//!
//! These types are the neutral form a server's free/busy answer is normalised
//! into: RFC 6638 `VFREEBUSY` over a CalDAV schedule-outbox, and (later) the
//! Microsoft Graph `getSchedule` response both map onto
//! [`AttendeeAvailability`]. Keeping the model here means feature code never
//! depends on a specific backend's wire shape.
//!
//! Like the rest of `lookout-core` this is I/O-free: it carries instants and
//! statuses only, never zone math or network calls.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use chrono::{DateTime, Utc};

use crate::email::EmailAddress;
use crate::interop::cdo::BusyStatus;

/// One occupied span on an attendee's calendar, as reported by a free/busy
/// lookup. `start`/`end` are UTC instants (`end` exclusive); `status` is the
/// Outlook five-state show-as the backend reported (a `Free` interval is
/// normally omitted, but a backend is allowed to return one for a
/// "working elsewhere" day).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FreeBusyInterval {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    #[serde(default)]
    pub status: BusyStatus,
}

impl FreeBusyInterval {
    /// Whether this interval overlaps the half-open `[start, end)` span.
    pub fn overlaps(&self, start: DateTime<Utc>, end: DateTime<Utc>) -> bool {
        self.start < end && start < self.end
    }
}

/// RFC 6638 §3.2.9 `SCHEDULE-STATUS`: a dotted code the scheduling server
/// attaches to an attendee's free/busy answer (and to a delivered iTIP
/// message) saying how far it got. Only the coarse first digit is interpreted
/// here; the full string is kept verbatim so nothing is lost on a round-trip.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ScheduleStatus(pub String);

impl ScheduleStatus {
    pub fn new(code: impl Into<String>) -> Self {
        ScheduleStatus(code.into())
    }

    /// Whether the lookup succeeded: the RFC 6638 delivery family (`1.x`, e.g.
    /// `1.2 Delivered`) or the iTIP success family (`2.x`, the free/busy
    /// `request-status` form, e.g. `2.0;Success`).
    pub fn is_success(&self) -> bool {
        self.0.starts_with("1.") || self.0.starts_with("2.")
    }

    /// A short human-readable label for the common families, else `None` (the
    /// caller can fall back to the raw code).
    pub fn label(&self) -> Option<&'static str> {
        match self.0.split('.').next() {
            Some("1") => Some("Pending"),
            Some("2") => Some("Available"),
            Some("3") => Some("Unknown user"),
            Some("5") => Some("Service unavailable"),
            _ => None,
        }
    }
}

/// One attendee's availability: the intervals a backend reported plus how the
/// lookup went. `unknown` is the honest "no data" state (an attendee address a
/// server couldn't resolve, or an account with no free/busy backend at all) -
/// the UI shows it distinctly rather than pretending the person is free.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AttendeeAvailability {
    pub address: EmailAddress,
    #[serde(default)]
    pub intervals: Vec<FreeBusyInterval>,
    #[serde(default)]
    pub schedule_status: Option<ScheduleStatus>,
    #[serde(default)]
    pub unknown: bool,
}

impl AttendeeAvailability {
    /// A free/busy answer for an attendee with the given intervals.
    pub fn new(address: EmailAddress, intervals: Vec<FreeBusyInterval>) -> Self {
        AttendeeAvailability {
            address,
            intervals,
            schedule_status: Some(ScheduleStatus::new("1.0")),
            unknown: false,
        }
    }

    /// The "we couldn't find out" form - a placeholder row.
    pub fn unknown(address: EmailAddress) -> Self {
        AttendeeAvailability {
            address,
            intervals: Vec::new(),
            schedule_status: None,
            unknown: true,
        }
    }

    /// The first interval overlapping the given span, if any - a backend's busy
    /// block that clashes with a proposed meeting time.
    pub fn conflict(&self, start: DateTime<Utc>, end: DateTime<Utc>) -> Option<&FreeBusyInterval> {
        self.intervals.iter().find(|interval| interval.overlaps(start, end))
    }

    /// Whether this attendee is known to be busy anywhere in the span.
    pub fn is_busy(&self, start: DateTime<Utc>, end: DateTime<Utc>) -> bool {
        self.conflict(start, end).is_some()
    }
}

/// Whether every attendee in the slice is free over the span. An empty slice
/// is trivially free; `unknown` attendees are treated as not-free so a
/// suggestion never assumes data the server didn't give.
pub fn all_free(availabilities: &[AttendeeAvailability], start: DateTime<Utc>, end: DateTime<Utc>) -> bool {
    availabilities.iter().all(|a| !a.unknown && !a.is_busy(start, end))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn utc(y: i32, m: u32, d: u32, h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, 0, 0).unwrap()
    }

    fn interval(start_hour: u32, end_hour: u32) -> FreeBusyInterval {
        FreeBusyInterval {
            start: utc(2026, 10, 15, start_hour),
            end: utc(2026, 10, 15, end_hour),
            status: BusyStatus::Busy,
        }
    }

    #[test]
    fn overlap_is_half_open() {
        let i = interval(9, 10);
        assert!(i.overlaps(utc(2026, 10, 15, 9), utc(2026, 10, 15, 11)), "starts inside the block");
        assert!(i.overlaps(utc(2026, 10, 15, 9), utc(2026, 10, 15, 10)));
        assert!(!i.overlaps(utc(2026, 10, 15, 10), utc(2026, 10, 15, 11)), "touching the end does not overlap");
        assert!(!i.overlaps(utc(2026, 10, 15, 8), utc(2026, 10, 15, 9)), "touching the start does not overlap");
        assert!(!i.overlaps(utc(2026, 10, 15, 7), utc(2026, 10, 15, 8)));
    }

    #[test]
    fn attendee_reports_conflicts() {
        let attendee = AttendeeAvailability::new(EmailAddress::new("a@example.com"), vec![interval(9, 10)]);
        assert!(attendee.is_busy(utc(2026, 10, 15, 9), utc(2026, 10, 15, 11)));
        assert!(!attendee.is_busy(utc(2026, 10, 15, 11), utc(2026, 10, 15, 12)));
    }

    #[test]
    fn unknown_attendees_block_suggestions() {
        let free = AttendeeAvailability::new(EmailAddress::new("free@example.com"), vec![]);
        assert!(all_free(std::slice::from_ref(&free), utc(2026, 10, 15, 9), utc(2026, 10, 15, 10)));
        let unknown = AttendeeAvailability::unknown(EmailAddress::new("who@example.com"));
        assert!(!all_free(&[free, unknown], utc(2026, 10, 15, 9), utc(2026, 10, 15, 10)));
    }

    #[test]
    fn schedule_status_families() {
        assert!(ScheduleStatus::new("1.2").is_success());
        assert!(ScheduleStatus::new("2.0").is_success());
        assert_eq!(ScheduleStatus::new("2.0").label(), Some("Available"));
        assert_eq!(ScheduleStatus::new("1.0").label(), Some("Pending"));
        assert_eq!(ScheduleStatus::new("3.7").label(), Some("Unknown user"));
        assert_eq!(ScheduleStatus::new("").label(), None);
    }
}
