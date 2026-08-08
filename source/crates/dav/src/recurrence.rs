use chrono::{DateTime, Utc};
use lookout_core::{CalendarEvent, EventOccurrence, RecurrenceRange};
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
///
/// `EXDATE`s are applied (the excluded instances never expand) and `RDATE`s
/// are folded in, so a master carrying either renders exactly its instance
/// set. Per-occurrence overrides are *not* merged here - call
/// [`expand_master_with_overrides`] for a master whose VEVENTs include
/// `RECURRENCE-ID` siblings.
pub fn expand_occurrences(event: &CalendarEvent, window_start: DateTime<Utc>, window_end: DateTime<Utc>) -> Vec<EventOccurrence> {
    let Some(rrule_str) = &event.rrule else {
        return single_occurrence_if_overlapping(event, window_start, window_end);
    };

    match build_rrule_set(event, rrule_str) {
        Ok(set) => expand_set(set, event, window_start, window_end),
        Err(e) => {
            tracing::warn!(uid = %event.uid, "malformed RRULE {rrule_str:?}: {e}; treating as a single occurrence");
            single_occurrence_if_overlapping(event, window_start, window_end)
        }
    }
}

/// Expands a recurring master together with its per-occurrence overrides -
/// the VEVENTs sharing its UID that carry `RECURRENCE-ID` - into a single
/// non-duplicated occurrence list. Each "this instance" override replaces
/// exactly the master instance it names (which no longer expands); a
/// `RANGE=THISANDFUTURE` override replaces that instance and every later
/// one, expanding its own series from its anchor. Every occurrence keeps the
/// master's anchor (`master_start`/`master_end`) so whole-series edits stay
/// possible from any occurrence, and override-derived occurrences carry
/// their `recurrence_id` so the UI can tell them apart.
pub fn expand_master_with_overrides<'a, I>(master: &CalendarEvent, overrides: I, window_start: DateTime<Utc>, window_end: DateTime<Utc>) -> Vec<EventOccurrence>
where
    I: IntoIterator<Item = &'a CalendarEvent>,
{
    let overrides: Vec<&CalendarEvent> = overrides.into_iter().collect();
    let mut occurrences = expand_occurrences(master, window_start, window_end);
    if overrides.is_empty() {
        return occurrences;
    }
    // The instances the overrides cover: every this-scoped override names its
    // own instance; a THISANDFUTURE override covers its anchor and everything
    // after. Master instances in any covered range are dropped - the
    // overrides render in their place below.
    let this_and_future_anchors: Vec<DateTime<Utc>> = overrides
        .iter()
        .filter(|o| o.recurrence_range == RecurrenceRange::ThisAndFuture)
        .filter_map(|o| o.recurrence_id)
        .collect();
    occurrences.retain(|occ| !(overrides.iter().any(|o| o.recurrence_id == Some(occ.start)) || this_and_future_anchors.iter().any(|anchor| occ.start >= *anchor)));

    let mut override_occurrences: Vec<EventOccurrence> = Vec::new();
    for override_event in overrides {
        let mut expanded = expand_occurrences(override_event, window_start, window_end);
        if expanded.is_empty() {
            // A no-RRULE override that doesn't expand above is just its one
            // named instance; re-run the overlap check directly rather than
            // silently dropping it.
            expanded = single_occurrence_if_overlapping(override_event, window_start, window_end);
        }
        for mut occ in expanded {
            // The occurrence stays anchored to the series' master (not the
            // override's own start) so "edit all" / "edit this-and-following"
            // scope decisions have the real series anchor to work from.
            occ.master_start = Some(master.start);
            occ.master_end = Some(master.end);
            occ.master_href = master.href.clone();
            occ.master_etag = master.etag.clone();
            // The master's EXDATEs (which govern the whole series) ride along
            // too - the override's own exclusions are empty in practice, and
            // the master's must survive a whole-series save from this
            // occurrence.
            occ.exdates = master.exdates.clone();
            occ.recurrence_id = override_event.recurrence_id;
            // A this-scoped override keeps the master's RRULE on its
            // occurrence (it only moves one instance); a THISANDFUTURE
            // override's occurrences carry the override's own rule, since
            // that's what governs the series from its anchor on.
            if override_event.recurrence_range != RecurrenceRange::ThisAndFuture {
                occ.rrule = master.rrule.clone();
            }
            override_occurrences.push(occ);
        }
    }
    override_occurrences.sort_by_key(|occ| occ.start);
    occurrences.extend(override_occurrences);
    occurrences
}

fn expand_set(set: RRuleSet, event: &CalendarEvent, window_start: DateTime<Utc>, window_end: DateTime<Utc>) -> Vec<EventOccurrence> {
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
                recurrence_id: None,
                exdates: event.exdates.clone(),
                master_start: Some(event.start),
                master_end: Some(event.end),
                href: event.href.clone(),
                etag: event.etag.clone(),
                master_href: None,
                master_etag: None,
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

fn build_rrule_set(event: &CalendarEvent, rrule_str: &str) -> Result<RRuleSet, String> {
    let dtstart = event.start.with_timezone(&rrule::Tz::UTC);
    let unvalidated: RRule<Unvalidated> = rrule_str.parse().map_err(|e| format!("{e}"))?;
    let validated = unvalidated.validate(dtstart).map_err(|e| format!("{e}"))?;
    let mut set = RRuleSet::new(dtstart).rrule(validated);
    for exdate in &event.exdates {
        set = set.exdate(exdate.with_timezone(&rrule::Tz::UTC));
    }
    for rdate in &event.rdates {
        set = set.rdate(rdate.with_timezone(&rrule::Tz::UTC));
    }
    Ok(set)
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
            recurrence_id: None,
            exdates: event.exdates.clone(),
            master_start: Some(event.start),
            master_end: Some(event.end),
            href: event.href.clone(),
            etag: event.etag.clone(),
            master_href: None,
            master_etag: None,
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

    pub fn base_event(start: &str, end: &str, rrule: Option<&str>) -> CalendarEvent {
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
            recurrence_id: None,
            recurrence_range: lookout_core::RecurrenceRange::default(),
            exdates: Vec::new(),
            rdates: Vec::new(),
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
        (start.parse::<DateTime<Utc>>().unwrap(), end.parse().unwrap())
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

    #[test]
    fn exdates_are_excluded_from_expansion_and_rdates_are_folded_in() {
        let mut event = base_event("2026-07-01T09:00:00Z", "2026-07-01T09:30:00Z", Some("FREQ=WEEKLY"));
        event.exdates = vec!["2026-07-08T09:00:00Z".parse::<DateTime<Utc>>().unwrap()];
        event.rdates = vec!["2026-07-21T15:00:00Z".parse::<DateTime<Utc>>().unwrap()];
        let (start, end) = window("2026-07-01T00:00:00Z", "2026-08-01T00:00:00Z");
        let occurrences = expand_occurrences(&event, start, end);
        let starts: Vec<DateTime<Utc>> = occurrences.iter().map(|occ| occ.start).collect();
        // Wednesdays in July 2026: 1, 8, 15, 22, 29. The 8th is exdated; the
        // extra RDATE lands on the 21st (a Tuesday - a genuinely extra
        // instance, not a duplicate of a regular expansion).
        assert!(!starts.contains(&"2026-07-08T09:00:00Z".parse::<DateTime<Utc>>().unwrap()));
        assert!(starts.contains(&"2026-07-21T15:00:00Z".parse::<DateTime<Utc>>().unwrap()));
        assert_eq!(occurrences.len(), 5);
    }

    #[test]
    fn this_scoped_override_replaces_exactly_one_instance() {
        let master = base_event("2026-07-01T09:00:00Z", "2026-07-01T09:30:00Z", Some("FREQ=WEEKLY"));
        let mut override_event = base_event("2026-07-15T11:00:00Z", "2026-07-15T12:00:00Z", None);
        override_event.uid = master.uid.clone();
        override_event.recurrence_id = Some("2026-07-15T09:00:00Z".parse::<DateTime<Utc>>().unwrap());
        let (start, end) = window("2026-07-01T00:00:00Z", "2026-08-01T00:00:00Z");
        let occurrences = expand_master_with_overrides(&master, &[override_event], start, end);
        let mut occurrences = occurrences;
        occurrences.sort_by_key(|occ| occ.start);
        // 1st, 8th, 15th (overridden, now at 11:00), 22nd, 29th.
        assert_eq!(occurrences.len(), 5);
        assert!(!occurrences.iter().any(|occ| occ.start == "2026-07-15T09:00:00Z".parse::<DateTime<Utc>>().unwrap()));
        let moved = occurrences.iter().find(|occ| occ.recurrence_id.is_some()).unwrap();
        assert_eq!(moved.start, "2026-07-15T11:00:00Z".parse::<DateTime<Utc>>().unwrap());
        assert_eq!(moved.end - moved.start, chrono::Duration::hours(1));
        // The override occurrence stays anchored to the series master and
        // keeps the master's RRULE so a whole-series edit can be offered.
        assert_eq!(moved.master_start, Some(master.start));
        assert_eq!(moved.rrule.as_deref(), Some("FREQ=WEEKLY"));
    }

    #[test]
    fn this_and_future_override_replaces_its_anchor_and_every_later_instance() {
        let master = base_event("2026-07-01T09:00:00Z", "2026-07-01T09:30:00Z", Some("FREQ=WEEKLY"));
        // A real THISANDFUTURE override carries its own RRULE (that's what
        // "and future" means: the override's recurrence set replaces the
        // rest of the series from its anchor on).
        let mut override_event = base_event("2026-07-15T10:00:00Z", "2026-07-15T10:45:00Z", Some("FREQ=WEEKLY"));
        override_event.uid = master.uid.clone();
        override_event.recurrence_id = Some("2026-07-15T09:00:00Z".parse().unwrap());
        override_event.recurrence_range = RecurrenceRange::ThisAndFuture;
        let (start, end) = window("2026-07-01T00:00:00Z", "2026-08-01T00:00:00Z");
        let occurrences = expand_master_with_overrides(&master, &[override_event], start, end);
        let mut occurrences = occurrences;
        occurrences.sort_by_key(|occ| occ.start);
        assert_eq!(occurrences.len(), 5);
        // Instances from the 15th on render at the override's time.
        for occ in &occurrences[2..] {
            assert_eq!(occ.start, occ.end - chrono::Duration::minutes(45), "all later instances carry the override's duration");
            assert_eq!(occ.recurrence_id, Some("2026-07-15T09:00:00Z".parse().unwrap()));
        }
        assert_eq!(occurrences[0].start, "2026-07-01T09:00:00Z".parse::<DateTime<Utc>>().unwrap());
        assert_eq!(occurrences[1].start, "2026-07-08T09:00:00Z".parse::<DateTime<Utc>>().unwrap());
    }
}
