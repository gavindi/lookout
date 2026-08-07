use chrono::{DateTime, Utc};
use lookout_core::{CalendarEvent, EventOccurrence};
use rrule::{RRule, RRuleSet, Unvalidated};

/// Defensive cap on how many occurrences a single event can expand into,
/// even bounded to a date window - guards against a pathological RRULE that
/// somehow still yields an enormous number of occurrences within the window
/// (e.g. a `FREQ=SECONDLY` rule), though the window bound should already
/// make that moot in practice.
const MAX_OCCURRENCES: u16 = 400;

/// Expands `event` into concrete occurrences overlapping `[window_start,
/// window_end)`. Non-recurring events pass through unchanged, subject to the
/// same overlap check. Any RRULE that fails to parse or validate falls back
/// to treating the event as a single non-recurring occurrence (logged as a
/// warning) rather than dropping it silently.
pub fn expand_occurrences(event: &CalendarEvent, window_start: DateTime<Utc>, window_end: DateTime<Utc>) -> Vec<EventOccurrence> {
    let Some(rrule_str) = &event.rrule else {
        return single_occurrence_if_overlapping(event, window_start, window_end);
    };

    match build_rrule_set(event, rrule_str) {
        Ok(set) => {
            let duration = event.end - event.start;
            let tz_start = window_start.with_timezone(&rrule::Tz::UTC);
            let tz_end = window_end.with_timezone(&rrule::Tz::UTC);
            let result = set.after(tz_start).before(tz_end).all(MAX_OCCURRENCES);
            if result.limited {
                tracing::warn!(uid = %event.uid, "RRULE expansion hit the {MAX_OCCURRENCES}-occurrence cap within the requested window");
            }
            result
                .dates
                .into_iter()
                .map(|start| {
                    let start_utc = start.with_timezone(&Utc);
                    EventOccurrence {
                        uid: event.uid.clone(),
                        calendar_id: event.calendar_id.clone(),
                        summary: event.summary.clone(),
                        description: event.description.clone(),
                        location: event.location.clone(),
                        start: start_utc,
                        end: start_utc + duration,
                        all_day: event.all_day,
                        rrule: event.rrule.clone(),
                        master_start: Some(event.start),
                        master_end: Some(event.end),
                        href: event.href.clone(),
                        etag: event.etag.clone(),
                        attendees: event.attendees.clone(),
                        organizer: event.organizer.clone(),
                        categories: event.categories.clone(),
                        sensitivity: event.sensitivity,
                        transparency: event.transparency,
                        reminder_minutes_before: event.reminder_minutes_before,
                        conference_url: event.conference_url.clone(),
                    }
                })
                .collect()
        }
        Err(e) => {
            tracing::warn!(uid = %event.uid, "malformed RRULE {rrule_str:?}: {e}; treating as a single occurrence");
            single_occurrence_if_overlapping(event, window_start, window_end)
        }
    }
}

fn build_rrule_set(event: &CalendarEvent, rrule_str: &str) -> Result<RRuleSet, String> {
    let dtstart = event.start.with_timezone(&rrule::Tz::UTC);
    let unvalidated: RRule<Unvalidated> = rrule_str.parse().map_err(|e| format!("{e}"))?;
    let validated = unvalidated.validate(dtstart).map_err(|e| format!("{e}"))?;
    Ok(RRuleSet::new(dtstart).rrule(validated))
}

fn single_occurrence_if_overlapping(event: &CalendarEvent, window_start: DateTime<Utc>, window_end: DateTime<Utc>) -> Vec<EventOccurrence> {
    if event.start < window_end && event.end > window_start {
        vec![EventOccurrence {
            uid: event.uid.clone(),
            calendar_id: event.calendar_id.clone(),
            summary: event.summary.clone(),
            description: event.description.clone(),
            location: event.location.clone(),
            start: event.start,
            end: event.end,
            all_day: event.all_day,
            rrule: event.rrule.clone(),
            master_start: Some(event.start),
            master_end: Some(event.end),
            href: event.href.clone(),
            etag: event.etag.clone(),
            attendees: event.attendees.clone(),
            organizer: event.organizer.clone(),
            categories: event.categories.clone(),
            sensitivity: event.sensitivity,
            transparency: event.transparency,
            reminder_minutes_before: event.reminder_minutes_before,
            conference_url: event.conference_url.clone(),
        }]
    } else {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lookout_core::{CalendarId, EventUid};

    fn base_event(start: &str, end: &str, rrule: Option<&str>) -> CalendarEvent {
        CalendarEvent {
            uid: EventUid("evt-1@example.com".to_string()),
            calendar_id: CalendarId("acct:cal".to_string()),
            summary: Some("Test event".to_string()),
            description: None,
            location: None,
            start: start.parse().unwrap(),
            end: end.parse().unwrap(),
            all_day: false,
            rrule: rrule.map(str::to_string),
            href: None,
            etag: None,
            attendees: Vec::new(),
            organizer: None,
            categories: Vec::new(),
            sensitivity: lookout_core::EventSensitivity::default(),
            transparency: lookout_core::EventTransparency::default(),
            reminder_minutes_before: None,
            conference_url: None,
        }
    }

    fn window(start: &str, end: &str) -> (DateTime<Utc>, DateTime<Utc>) {
        (start.parse().unwrap(), end.parse().unwrap())
    }

    #[test]
    fn non_recurring_event_inside_window_yields_one_occurrence() {
        let event = base_event("2026-07-15T14:00:00Z", "2026-07-15T15:00:00Z", None);
        let (start, end) = window("2026-07-01T00:00:00Z", "2026-08-01T00:00:00Z");
        let occurrences = expand_occurrences(&event, start, end);
        assert_eq!(occurrences.len(), 1);
        assert_eq!(occurrences[0].start, event.start);
    }

    #[test]
    fn non_recurring_event_outside_window_yields_zero_occurrences() {
        let event = base_event("2026-06-15T14:00:00Z", "2026-06-15T15:00:00Z", None);
        let (start, end) = window("2026-07-01T00:00:00Z", "2026-08-01T00:00:00Z");
        assert!(expand_occurrences(&event, start, end).is_empty());
    }

    #[test]
    fn weekly_rrule_with_count_yields_exactly_that_many_occurrences_when_window_covers_all_of_them() {
        let event = base_event("2026-01-06T09:00:00Z", "2026-01-06T09:30:00Z", Some("FREQ=WEEKLY;COUNT=10"));
        let (start, end) = window("2020-01-01T00:00:00Z", "2030-01-01T00:00:00Z");
        let occurrences = expand_occurrences(&event, start, end);
        assert_eq!(occurrences.len(), 10);
        // Each occurrence keeps the master's 30-minute duration.
        for occ in &occurrences {
            assert_eq!(occ.end - occ.start, chrono::Duration::minutes(30));
        }
    }

    #[test]
    fn unbounded_weekly_rrule_is_correctly_clipped_to_the_window() {
        let event = base_event("2020-01-01T09:00:00Z", "2020-01-01T10:00:00Z", Some("FREQ=WEEKLY"));
        let (start, end) = window("2026-07-01T00:00:00Z", "2026-08-01T00:00:00Z");
        let occurrences = expand_occurrences(&event, start, end);
        // Exactly the July 2026 Wednesdays (2020-01-01 is a Wednesday).
        assert_eq!(occurrences.len(), 5);
        for occ in &occurrences {
            assert!(occ.start >= start && occ.start < end);
        }
    }

    #[test]
    fn malformed_rrule_falls_back_to_single_occurrence_instead_of_panicking() {
        let event = base_event("2026-07-15T14:00:00Z", "2026-07-15T15:00:00Z", Some("FREQ=BOGUSFREQ;COUNT=abc"));
        let (start, end) = window("2026-07-01T00:00:00Z", "2026-08-01T00:00:00Z");
        let occurrences = expand_occurrences(&event, start, end);
        assert_eq!(occurrences.len(), 1);
        assert_eq!(occurrences[0].start, event.start);
    }

    #[test]
    fn occurrences_carry_the_master_anchor_for_whole_series_edits() {
        // A recurring master anchored weeks before the requested window, so no
        // occurrence's own start equals the master's DTSTART - the editor must
        // still be able to recover the series anchor from any occurrence.
        let event = base_event("2026-06-01T09:00:00Z", "2026-06-01T09:30:00Z", Some("FREQ=WEEKLY"));
        let (start, end) = window("2026-07-01T00:00:00Z", "2026-08-01T00:00:00Z");
        let occurrences = expand_occurrences(&event, start, end);
        assert!(occurrences.len() > 1);
        for occ in &occurrences {
            assert_eq!(occ.master_start, Some(event.start));
            assert_eq!(occ.master_end, Some(event.end));
            assert_ne!(occ.start, event.start, "the occurrence is an expansion, not the master");
        }
    }
}
