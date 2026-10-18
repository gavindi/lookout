//! Calendar event search (CAL-22 / item 3.14): query the per-account
//! `CalendarCache` indexes, merge/dedupe the hits and drive the Calendar
//! module's results page. The title-bar wiring lives in `window.rs`; this
//! module owns the search state transitions and the pure result shaping.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use adw::prelude::*;
use chrono::{Datelike, Months, NaiveDate};
use gtk::glib;
use lookout_core::calendar_protocol::CalendarCommand;
use lookout_core::{AccountId, CalendarId};
use lookout_dav::subscription::SubscriptionCommand;
use lookout_dav::CachedEventHit;

use crate::calendar_view::{self, CalendarMain, CalendarSearchRow};
use crate::worker::Worker;

use super::*;

/// Splits a query into search terms, honouring double-quoted phrases. Terms
/// are lowercased here (the cache stores `search_blob` lowercased) and empty
/// terms are dropped, so `""` never matches everything.
pub(crate) fn calendar_search_terms(query: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for character in query.chars() {
        match character {
            '"' => {
                if !current.trim().is_empty() {
                    terms.push(current.trim().to_lowercase());
                }
                current.clear();
                quoted = !quoted;
            }
            character if character.is_whitespace() && !quoted => {
                if !current.trim().is_empty() {
                    terms.push(current.trim().to_lowercase());
                }
                current.clear();
            }
            character => current.push(character),
        }
    }
    if !current.trim().is_empty() {
        terms.push(current.trim().to_lowercase());
    }
    terms
}

/// Merges every account's raw cache hits into one row per recurring series.
/// A series matched in many cached months (a weekly stand-up over a year)
/// becomes one row, dated at the matching occurrence nearest `now` (future
/// preferred on a tie). Rows are sorted newest first, matching the Mail list's
/// date ordering.
pub(crate) fn merge_calendar_search_hits(
    hits: Vec<CachedEventHit>,
    calendar_names: &HashMap<CalendarId, String>,
    checked_only: Option<&HashSet<CalendarId>>,
    now: chrono::DateTime<chrono::Utc>,
) -> Vec<CalendarSearchRow> {
    let mut best: HashMap<(String, String), ((i64, bool), CachedEventHit)> = HashMap::new();
    for hit in hits {
        if let Some(checked) = checked_only {
            if !checked.contains(&hit.occurrence.calendar_id) {
                continue;
            }
        }
        let delta = hit.occurrence.start.signed_duration_since(now).num_seconds();
        // Distance first; a future occurrence wins only an exact tie.
        let rank = (delta.abs(), delta < 0);
        let key = (hit.occurrence.calendar_id.0.clone(), hit.occurrence.uid.0.clone());
        match best.get(&key) {
            Some((existing_rank, _)) if *existing_rank <= rank => {}
            _ => {
                best.insert(key, (rank, hit));
            }
        }
    }

    let mut rows: Vec<CalendarSearchRow> = best
        .into_values()
        .map(|(_, hit)| {
            let calendar_name = calendar_names
                .get(&hit.occurrence.calendar_id)
                .cloned()
                .unwrap_or_else(|| hit.occurrence.calendar_id.0.clone());
            CalendarSearchRow {
                occurrence: hit.occurrence,
                calendar_name,
                snippet: hit.snippet,
            }
        })
        .collect();
    rows.sort_by(|a, b| b.occurrence.start.cmp(&a.occurrence.start).then_with(|| a.occurrence.uid.0.cmp(&b.occurrence.uid.0)));
    rows.truncate(CALENDAR_SEARCH_RESULT_LIMIT);
    rows
}

/// The per-account cache ids a search covers: every connected calendar
/// account plus every webcal subscription (whose session caches under
/// `webcal-<subscription id>`). Birthdays are synthesized from contacts and
/// are intentionally not in the event index - People search covers them.
fn calendar_search_sources(calendar_state: &Rc<RefCell<CalendarUiState>>) -> Vec<AccountId> {
    let st = calendar_state.borrow();
    let mut sources: Vec<AccountId> = st.accounts.keys().cloned().collect();
    sources.extend(st.webcal_handles.keys().map(|id| AccountId(format!("webcal-{id}"))));
    sources
}

fn calendar_names(calendar_state: &Rc<RefCell<CalendarUiState>>) -> HashMap<CalendarId, String> {
    let st = calendar_state.borrow();
    let mut names = HashMap::new();
    for handle in st.accounts.values() {
        for calendar in &handle.calendars {
            names.insert(calendar.id.clone(), calendar.display_name.clone());
        }
    }
    for handle in st.webcal_handles.values() {
        names.insert(handle.calendar_id.clone(), handle.display_name.clone());
    }
    names
}

/// One source's `FetchMonth` channel - CalDAV/Graph accounts and the single
/// webcal session use different command enums.
enum BackfillSource {
    Account(async_channel::Sender<CalendarCommand>),
    Webcal(async_channel::Sender<SubscriptionCommand>),
}

/// A backfill target: the cache id to check for missing months (accounts
/// only - webcal's single session has no per-subscription cache check) and the
/// command channel that fetches them.
struct BackfillTarget {
    account_id: Option<AccountId>,
    source: BackfillSource,
}

/// The bounded month window the one-shot search backfill fetches: the past
/// year through the next year. Sending the missing months through the
/// existing `FetchMonth` command populates the same cache the search reads;
/// the `OccurrencesUpdated` display guard keeps these background fetches from
/// replacing the displayed month.
fn search_backfill_months() -> Vec<NaiveDate> {
    let current = crate::calendar_tz::display_zone()
        .today()
        .with_day(1)
        .unwrap_or_else(|| crate::calendar_tz::display_zone().today());
    let mut months = Vec::with_capacity(25);
    for offset in (1..=12).rev() {
        months.push(current - Months::new(offset));
    }
    for offset in 0..=12 {
        months.push(current + Months::new(offset));
    }
    months
}

/// Fetches the search window once per session, one month per source per tick,
/// so a cold start can find events that were never on screen. The cache is the
/// only consumer; the user's current view is untouched thanks to the
/// displayed-month guard in the calendar event loop.
pub(crate) fn backfill_calendar_search(calendar_state: &Rc<RefCell<CalendarUiState>>, worker: &Rc<Worker>, calendar_main: &Rc<CalendarMain>) {
    let targets: Vec<BackfillTarget> = {
        let mut st = calendar_state.borrow_mut();
        if st.calendar_search_backfilled {
            return;
        }
        let mut targets: Vec<BackfillTarget> = st
            .accounts
            .iter()
            .map(|(account_id, handle)| BackfillTarget {
                account_id: Some(account_id.clone()),
                source: BackfillSource::Account(handle.cmd_tx.clone()),
            })
            .collect();
        if let Some(cmd_tx) = st.webcal_cmd_tx.clone() {
            targets.push(BackfillTarget {
                account_id: None,
                source: BackfillSource::Webcal(cmd_tx),
            });
        }
        if targets.is_empty() {
            return;
        }
        st.calendar_search_backfilled = true;
        targets
    };

    let months = Rc::new(search_backfill_months());
    let calendar_main = calendar_main.clone();
    let worker = worker.clone();
    glib::spawn_future_local(async move {
        let mut pending: Vec<(BackfillSource, Vec<NaiveDate>)> = Vec::new();
        for target in targets {
            let missing = if let Some(account_id) = target.account_id {
                spawn_calendar_cache_missing_months(&worker, account_id, (*months).clone()).recv().await.unwrap_or_default()
            } else {
                (*months).clone()
            };
            if !missing.is_empty() {
                // `pop` below drains from the end; reverse so the oldest
                // missing month is fetched first.
                let mut missing = missing;
                missing.reverse();
                pending.push((target.source, missing));
            }
        }
        if pending.is_empty() {
            return;
        }
        calendar_view::set_search_backfilling(&calendar_main, true);
        let pending = Rc::new(RefCell::new(pending));
        let timer_main = calendar_main.clone();
        glib::timeout_add_local(std::time::Duration::from_millis(1200), move || {
            let mut pending = pending.borrow_mut();
            for (source, months) in pending.iter_mut() {
                let Some(month) = months.pop() else { continue };
                match source {
                    BackfillSource::Account(cmd_tx) => {
                        let _ = cmd_tx.send_blocking(CalendarCommand::FetchMonth(month));
                    }
                    BackfillSource::Webcal(cmd_tx) => {
                        let _ = cmd_tx.send_blocking(SubscriptionCommand::FetchMonth(month));
                    }
                }
            }
            pending.retain(|(_, months)| !months.is_empty());
            if pending.is_empty() {
                calendar_view::set_search_backfilling(&timer_main, false);
                return glib::ControlFlow::Break;
            }
            glib::ControlFlow::Continue
        });
    });
}

/// Starts a new calendar search (or silently repaints the existing one). The
/// previous calendar view is remembered only on the first search, so the
/// background refresh after a month sync doesn't overwrite it.
pub(crate) fn start_calendar_search(calendar_state: &Rc<RefCell<CalendarUiState>>, worker: &Rc<Worker>, calendar_main: &Rc<CalendarMain>, query: &str) {
    run_calendar_search(calendar_state, worker, calendar_main, query, true);
    backfill_calendar_search(calendar_state, worker, calendar_main);
}

/// Re-runs the active search after a calendar sync, an event edit or a
/// calendar-list change. A no-op when no search is showing.
pub(crate) fn rerun_calendar_search(calendar_state: &Rc<RefCell<CalendarUiState>>, worker: &Rc<Worker>, calendar_main: &Rc<CalendarMain>) {
    let (active, query) = {
        let st = calendar_state.borrow();
        (st.calendar_search_active, st.calendar_search_query.clone())
    };
    if active && !query.trim().is_empty() {
        run_calendar_search(calendar_state, worker, calendar_main, &query, false);
    }
}

fn run_calendar_search(calendar_state: &Rc<RefCell<CalendarUiState>>, worker: &Rc<Worker>, calendar_main: &Rc<CalendarMain>, query: &str, first: bool) {
    let query = query.trim().to_string();
    if query.is_empty() {
        return;
    }
    let terms = calendar_search_terms(&query);
    if terms.is_empty() {
        return;
    }

    let names = calendar_names(calendar_state);
    let (generation, existing, checked_only, checked) = {
        let mut st = calendar_state.borrow_mut();
        if first && !st.calendar_search_active {
            let view = calendar_view::active_view(calendar_main);
            st.calendar_search_previous_view = if view == "search" { "month" } else { view };
        }
        let existing = if first { Vec::new() } else { st.calendar_search_results.clone() };
        st.calendar_search_active = true;
        st.calendar_search_query = query.clone();
        st.calendar_search_generation = st.calendar_search_generation.wrapping_add(1);
        let generation = st.calendar_search_generation;
        let checked_only = st.calendar_search_checked_only;
        let checked = st.checked_calendar_ids.clone();
        (generation, existing, checked_only, checked)
    };

    let sources = calendar_search_sources(calendar_state);
    {
        let mut st = calendar_state.borrow_mut();
        st.calendar_search_pending = sources.len();
        st.calendar_search_results = existing.clone();
    }
    calendar_view::set_search_results(calendar_main, &query, &existing, true);

    if sources.is_empty() {
        calendar_state.borrow_mut().calendar_search_pending = 0;
        let rows = merge_calendar_search_hits(Vec::new(), &names, None, chrono::Utc::now());
        calendar_state.borrow_mut().calendar_search_results = rows.clone();
        calendar_view::set_search_results(calendar_main, &query, &rows, false);
        return;
    }

    let receivers: Vec<async_channel::Receiver<Vec<CachedEventHit>>> = sources.iter().map(|source| spawn_calendar_cache_search(worker, source.clone(), terms.clone())).collect();

    let calendar_state = calendar_state.clone();
    let calendar_main = calendar_main.clone();
    glib::spawn_future_local(async move {
        let mut hits = Vec::new();
        for receiver in receivers {
            if let Ok(reply) = receiver.recv().await {
                hits.extend(reply);
            }
        }
        {
            let st = calendar_state.borrow();
            if st.calendar_search_generation != generation || st.calendar_search_query != query {
                return;
            }
        }
        let checked_filter = checked_only.then_some(&checked);
        let rows = merge_calendar_search_hits(hits, &names, checked_filter, chrono::Utc::now());
        {
            let mut st = calendar_state.borrow_mut();
            st.calendar_search_pending = 0;
            st.calendar_search_results = rows.clone();
        }
        calendar_view::set_search_results(&calendar_main, &query, &rows, false);
    });
}

/// The calendar state, reached through `UiState` for call sites (the nav-rail
/// handlers, the title-bar closure) that are wired before `calendar_state`
/// exists as a local.
pub(crate) fn active_calendar_state(state: &Rc<RefCell<UiState>>) -> Option<Rc<RefCell<CalendarUiState>>> {
    state.borrow().calendar_state.clone()
}

/// Exits calendar search when one is active - the module-switch helper.
/// A no-op when Calendar isn't searching, so every nav-rail handler can call
/// it unconditionally.
pub(crate) fn exit_calendar_search_if_active(state: &Rc<RefCell<UiState>>, calendar_main: &Rc<CalendarMain>, search_entry: &gtk::SearchEntry) {
    if let Some(calendar_state) = active_calendar_state(state) {
        let active = calendar_state.borrow().calendar_search_active;
        if active {
            exit_calendar_search(&calendar_state, calendar_main, search_entry);
        }
    }
}

/// Leaves calendar search: clears the rows, restores the header and returns
/// to the view that was showing before the search. Safe to call twice (the
/// title-bar entry's own empty-query handler calls it again).
pub(crate) fn exit_calendar_search(calendar_state: &Rc<RefCell<CalendarUiState>>, calendar_main: &Rc<CalendarMain>, search_entry: &gtk::SearchEntry) {
    let (was_active, previous_view) = {
        let mut st = calendar_state.borrow_mut();
        let was_active = st.calendar_search_active;
        let previous = st.calendar_search_previous_view;
        st.calendar_search_active = false;
        st.calendar_search_query.clear();
        st.calendar_search_results.clear();
        st.calendar_search_pending = 0;
        // Bumping the generation invalidates any cache query still in flight.
        st.calendar_search_generation = st.calendar_search_generation.wrapping_add(1);
        (was_active, previous)
    };
    calendar_view::clear_search_results(calendar_main);
    if was_active {
        calendar_view::set_view(calendar_main, previous_view);
    }
    if !search_entry.text().is_empty() {
        search_entry.set_text("");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use lookout_core::{CalendarId, EventOccurrence, EventUid};

    fn hit(uid: &str, start: chrono::DateTime<Utc>) -> CachedEventHit {
        let occurrence = EventOccurrence {
            uid: EventUid(uid.to_string()),
            calendar_id: CalendarId("cal-1".to_string()),
            summary: Some(format!("Event {uid}")),
            description: None,
            location: None,
            start,
            end: start + chrono::Duration::hours(1),
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
            sensitivity: lookout_core::EventSensitivity::default(),
            show_as: lookout_core::BusyStatus::default(),
            status: lookout_core::EventStatus::default(),
            reminder_minutes_before: None,
            conference_url: None,
        };
        CachedEventHit {
            occurrence,
            matched: lookout_dav::SearchField::Summary,
            snippet: None,
        }
    }

    #[test]
    fn terms_honour_quoted_phrases_and_drop_empty_tokens() {
        assert_eq!(calendar_search_terms("  budget   review "), vec!["budget", "review"]);
        assert_eq!(calendar_search_terms("\"quarterly budget\" planning"), vec!["quarterly budget", "planning"]);
        assert!(calendar_search_terms("   ").is_empty());
        assert!(calendar_search_terms("\"\"").is_empty());
    }

    #[test]
    fn merge_collapses_a_series_to_the_occurrence_nearest_now() {
        let now = Utc.with_ymd_and_hms(2026, 8, 15, 12, 0, 0).unwrap();
        let hits = vec![
            hit("weekly", Utc.with_ymd_and_hms(2026, 8, 1, 9, 0, 0).unwrap()),
            hit("weekly", Utc.with_ymd_and_hms(2026, 8, 22, 9, 0, 0).unwrap()),
            hit("weekly", Utc.with_ymd_and_hms(2026, 8, 9, 9, 0, 0).unwrap()),
        ];
        let names = HashMap::from([(CalendarId("cal-1".to_string()), "Work".to_string())]);
        let rows = merge_calendar_search_hits(hits, &names, None, now);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].occurrence.start, Utc.with_ymd_and_hms(2026, 8, 9, 9, 0, 0).unwrap());
        assert_eq!(rows[0].calendar_name, "Work");
    }

    #[test]
    fn merge_sorts_newest_first_and_filters_to_checked_calendars() {
        let now = Utc.with_ymd_and_hms(2026, 8, 15, 12, 0, 0).unwrap();
        let mut older = hit("older", Utc.with_ymd_and_hms(2026, 8, 1, 9, 0, 0).unwrap());
        let newer = hit("newer", Utc.with_ymd_and_hms(2026, 8, 20, 9, 0, 0).unwrap());
        older.occurrence.calendar_id = CalendarId("cal-hidden".to_string());
        let names = HashMap::new();
        let checked = HashSet::from([CalendarId("cal-1".to_string())]);
        let rows = merge_calendar_search_hits(vec![older, newer], &names, Some(&checked), now);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].occurrence.uid.0, "newer");
    }
}
