//! VTIMEZONE generation for per-event time zones (CAL-11).
//!
//! `icalendar` 0.17 has no VTIMEZONE component (an upstream TODO), so this
//! module builds the component's text from `chrono-tz`'s own transition data;
//! `ical.rs` then parses it into the crate's generic `Other` component, which
//! `Calendar::push` serializes with its nested STANDARD/DAYLIGHT observances.
//!
//! The observances are explicit transitions rather than Outlook's compact
//! yearly RRULEs: every offset change in the two calendar years either side
//! of the event is emitted with its `DTSTART`, `TZOFFSETFROM`/`TZOFFSETTO`
//! and - when the zone names its offsets - `TZNAME`. That is exactly right
//! around the event, needs no rule reconstruction, and handles irregular
//! zones (Morocco, Lord Howe) the same way as regular ones.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use chrono::{DateTime, Datelike, Duration, Offset, TimeZone, Utc};
use chrono_tz::Tz;

/// Builds the `BEGIN:VTIMEZONE...END:VTIMEZONE` block for `tz`, covering
/// every transition from 1 Jan two years before the event to 1 Jan three
/// years after it. A zone with no transition in that span (UTC,
/// Asia/Kolkata) gets one fixed STANDARD observance, so the `TZID` always
/// resolves in a client that insists on a matching VTIMEZONE.
pub fn build(tz: Tz, start: DateTime<Utc>) -> String {
    let anchor = start.with_timezone(&tz).year();
    let scan_from = Utc.with_ymd_and_hms(anchor - 2, 1, 1, 0, 0, 0).unwrap();
    let scan_to = Utc.with_ymd_and_hms(anchor + 3, 1, 1, 0, 0, 0).unwrap();

    let mut text = String::from("BEGIN:VTIMEZONE\r\n");
    text.push_str(&format!("TZID:{}\r\n", tz.name()));
    let transitions = transitions_between(tz, scan_from, scan_to);
    if transitions.is_empty() {
        let offset = offset_at(tz, start);
        text.push_str("BEGIN:STANDARD\r\n");
        text.push_str("DTSTART:19700101T000000\r\n");
        text.push_str(&format!("TZOFFSETFROM:{}\r\n", offset_hhmm(offset)));
        text.push_str(&format!("TZOFFSETTO:{}\r\n", offset_hhmm(offset)));
        text.push_str("END:STANDARD\r\n");
    } else {
        for (at, from, to) in transitions {
            let kind = if to > from { "DAYLIGHT" } else { "STANDARD" };
            // RFC 5545 §3.6.5: the observance's DTSTART is the onset in the
            // local time in effect *before* the change.
            let onset = (at + Duration::seconds(i64::from(from))).naive_utc();
            text.push_str(&format!("BEGIN:{kind}\r\n"));
            let name = at.with_timezone(&tz).format("%Z").to_string();
            if name.chars().any(|c| c.is_ascii_alphabetic()) {
                text.push_str(&format!("TZNAME:{name}\r\n"));
            }
            text.push_str(&format!("DTSTART:{}\r\n", onset.format("%Y%m%dT%H%M%S")));
            text.push_str(&format!("TZOFFSETFROM:{}\r\n", offset_hhmm(from)));
            text.push_str(&format!("TZOFFSETTO:{}\r\n", offset_hhmm(to)));
            text.push_str(&format!("END:{kind}\r\n"));
        }
    }
    text.push_str("END:VTIMEZONE\r\n");
    text
}

/// Every UTC offset change in `[from, to)`, as `(instant, from, to)` in
/// seconds east of UTC, found by hourly sampling and then a minute-resolution
/// bisection (an hourly sample can't see a :30 transition such as Lord
/// Howe's).
fn transitions_between(tz: Tz, from: DateTime<Utc>, to: DateTime<Utc>) -> Vec<(DateTime<Utc>, i32, i32)> {
    let mut transitions = Vec::new();
    let mut previous = offset_at(tz, from);
    let mut hour = from + Duration::hours(1);
    while hour < to {
        let offset = offset_at(tz, hour);
        if offset != previous {
            transitions.push((transition_between(tz, hour - Duration::hours(1), hour, previous), previous, offset));
            previous = offset;
        }
        hour += Duration::hours(1);
    }
    transitions
}

/// The exact instant a transition known to lie in `(before, after]` happens,
/// to the minute.
fn transition_between(tz: Tz, before: DateTime<Utc>, after: DateTime<Utc>, before_offset: i32) -> DateTime<Utc> {
    let (mut lo, mut hi) = (before, after);
    while hi - lo > Duration::minutes(1) {
        let mid = lo + (hi - lo) / 2;
        if offset_at(tz, mid) == before_offset {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    hi
}

fn offset_at(tz: Tz, at: DateTime<Utc>) -> i32 {
    at.with_timezone(&tz).offset().fix().local_minus_utc()
}

/// `+HHMM`/`-HHMM` (RFC 5545 UTC-OFFSET, minute resolution - every zone in
/// the tz database is a whole number of minutes from UTC).
fn offset_hhmm(seconds: i32) -> String {
    let sign = if seconds < 0 { '-' } else { '+' };
    let abs = seconds.unsigned_abs();
    format!("{sign}{:02}{:02}", abs / 3600, (abs % 3600) / 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn berlin_emits_the_exact_2026_transitions() {
        let text = build(chrono_tz::Europe::Berlin, "2026-07-10T10:00:00Z".parse().unwrap());
        assert!(text.starts_with("BEGIN:VTIMEZONE\r\nTZID:Europe/Berlin\r\n"), "{text}");
        // Spring forward: last Sunday of March, 02:00 CET becomes 03:00 CEST.
        assert!(
            text.contains("BEGIN:DAYLIGHT\r\nTZNAME:CEST\r\nDTSTART:20260329T020000\r\nTZOFFSETFROM:+0100\r\nTZOFFSETTO:+0200\r\nEND:DAYLIGHT\r\n"),
            "{text}"
        );
        // Fall back: last Sunday of October, 03:00 CEST becomes 02:00 CET.
        assert!(
            text.contains("BEGIN:STANDARD\r\nTZNAME:CET\r\nDTSTART:20261025T030000\r\nTZOFFSETFROM:+0200\r\nTZOFFSETTO:+0100\r\nEND:STANDARD\r\n"),
            "{text}"
        );
        assert!(text.ends_with("END:VTIMEZONE\r\n"), "{text}");
    }

    #[test]
    fn half_hour_transitions_are_found_to_the_minute() {
        let lo = "2026-03-01T00:00:00Z".parse().unwrap();
        let hi = "2026-11-01T00:00:00Z".parse().unwrap();
        let changes = transitions_between(chrono_tz::Australia::Lord_Howe, lo, hi);
        assert_eq!(changes.len(), 2, "{changes:?}");
        // DST ends 5 Apr 02:00 +11 (04 Apr 15:00 UTC)...
        assert_eq!(changes[0].0.to_rfc3339(), "2026-04-04T15:00:00+00:00");
        assert_eq!((changes[0].1, changes[0].2), (11 * 3600, 10 * 3600 + 1800));
        // ...and starts 4 Oct 02:00 +10:30 (03 Oct 15:30 UTC).
        assert_eq!(changes[1].0.to_rfc3339(), "2026-10-03T15:30:00+00:00");
        assert_eq!((changes[1].1, changes[1].2), (10 * 3600 + 1800, 11 * 3600));
        // The onset is local-before time: both transitions are 02:00 local.
        let text = build(chrono_tz::Australia::Lord_Howe, "2026-07-10T10:00:00Z".parse().unwrap());
        assert!(text.contains("DTSTART:20260405T020000\r\nTZOFFSETFROM:+1100\r\nTZOFFSETTO:+1030"), "{text}");
        assert!(text.contains("DTSTART:20261004T020000\r\nTZOFFSETFROM:+1030\r\nTZOFFSETTO:+1100"), "{text}");
    }

    #[test]
    fn a_zone_without_transitions_gets_a_fixed_observance() {
        let text = build(chrono_tz::Asia::Kolkata, "2026-07-10T10:00:00Z".parse().unwrap());
        assert!(
            text.contains("BEGIN:STANDARD\r\nDTSTART:19700101T000000\r\nTZOFFSETFROM:+0530\r\nTZOFFSETTO:+0530\r\nEND:STANDARD\r\n"),
            "{text}"
        );
    }
}
