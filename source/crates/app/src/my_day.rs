//! My Day (OUTLOOK_PARITY.md item 4.9 / T-02): the user's per-day pinned
//! tasks, plus the suggestions the smart list offers - open tasks that are due
//! today or already overdue.
//!
//! The pin set itself lives in the UI-state database (`ui_state_db`'s
//! `my_day_tasks` table), keyed by `(day, calendar_id, uid)` so a pinned task
//! survives a restart and re-resolves against whatever the task sources last
//! reported. Everything in this module is pure (task slices in, task slices
//! out) so the selection logic is unit-testable without GTK, following the
//! `tasks_view` convention.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::collections::HashSet;

use chrono::{Local, NaiveDate};
use lookout_core::{CalendarTask, TaskStatus};

/// A My Day pin: the identity of one task across sources - its synthetic
/// calendar (list) id and its uid.
pub type MyDayKey = (String, String);

/// The pin key of one task.
pub fn key_for(task: &CalendarTask) -> MyDayKey {
    (task.calendar_id.0.clone(), task.uid.0.clone())
}

/// Whether a task is done (status or 100%) - the same rule `tasks_view` uses.
fn is_completed(task: &CalendarTask) -> bool {
    task.status == TaskStatus::Completed || task.percent_complete == Some(100)
}

/// A task's due date in local time, if any.
fn due_date(task: &CalendarTask) -> Option<NaiveDate> {
    task.due.map(|d| d.with_timezone(&Local).date_naive())
}

/// Whether `key` is pinned.
pub fn is_pinned(pins: &[MyDayKey], key: &MyDayKey) -> bool {
    pins.iter().any(|k| k == key)
}

/// Toggles `key` in the pin set in place, preserving insertion order.
pub fn toggle(pins: &mut Vec<MyDayKey>, key: MyDayKey) {
    if let Some(index) = pins.iter().position(|k| k == &key) {
        pins.remove(index);
    } else {
        pins.push(key);
    }
}

/// The pinned tasks, in pin order, resolved against `tasks`. A pin whose task
/// no longer exists (deleted, or its source disconnected) is silently dropped
/// from the result - the stored pin is left alone so a temporary source
/// outage can't lose it.
pub fn pinned_tasks(tasks: &[CalendarTask], pins: &[MyDayKey]) -> Vec<CalendarTask> {
    pins.iter()
        .filter_map(|(calendar_id, uid)| tasks.iter().find(|t| t.calendar_id.0 == *calendar_id && t.uid.0 == *uid).cloned())
        .collect()
}

/// The My Day suggestions: open tasks due today or earlier that aren't already
/// pinned, most urgent first (soonest due date, then RFC 5545 priority, then
/// title) - Outlook's "due today / overdue" prompt.
pub fn suggestions(tasks: &[CalendarTask], pins: &[MyDayKey], today: NaiveDate) -> Vec<CalendarTask> {
    let pinned: HashSet<MyDayKey> = pins.iter().cloned().collect();
    let mut out: Vec<CalendarTask> = tasks
        .iter()
        .filter(|task| !is_completed(task))
        .filter(|task| !pinned.contains(&key_for(task)))
        .filter(|task| due_date(task).is_some_and(|due| due <= today))
        .cloned()
        .collect();
    out.sort_by(|a, b| {
        let by_due = due_date(a).cmp(&due_date(b));
        let by_priority = priority_rank(a.priority.0).cmp(&priority_rank(b.priority.0));
        let by_title = a
            .summary
            .clone()
            .unwrap_or_default()
            .to_lowercase()
            .cmp(&b.summary.clone().unwrap_or_default().to_lowercase());
        by_due.then(by_priority).then(by_title)
    });
    out
}

/// Priority 0 (unset) and 9 (lowest) both sort last; 1..=8 sort by value -
/// the same ranking `tasks_view` uses.
fn priority_rank(raw: u8) -> u8 {
    if raw == 0 {
        10
    } else {
        raw
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveDateTime, NaiveTime, TimeZone};
    use lookout_core::{CalendarId, TaskPriority, TaskUid};

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn task(cal: &str, uid: &str, due: Option<NaiveDate>, priority: u8, completed: bool) -> CalendarTask {
        CalendarTask {
            uid: TaskUid(uid.to_string()),
            calendar_id: CalendarId(cal.to_string()),
            summary: Some(uid.to_string()),
            description: None,
            due: due.map(|d| {
                Local
                    .from_local_datetime(&NaiveDateTime::new(d, NaiveTime::from_hms_opt(9, 0, 0).unwrap()))
                    .unwrap()
                    .with_timezone(&chrono::Utc)
            }),
            start: None,
            rrule: None,
            created: None,
            completed: None,
            status: if completed { TaskStatus::Completed } else { TaskStatus::NeedsAction },
            priority: TaskPriority(priority),
            percent_complete: if completed { Some(100) } else { None },
            reminder_minutes_before: None,
            categories: Vec::new(),
            href: None,
            etag: None,
        }
    }

    #[test]
    fn toggle_adds_and_removes_preserving_order() {
        let mut pins: Vec<MyDayKey> = Vec::new();
        toggle(&mut pins, ("a".into(), "1".into()));
        toggle(&mut pins, ("b".into(), "2".into()));
        assert_eq!(pins, vec![("a".into(), "1".into()), ("b".into(), "2".into())]);
        assert!(is_pinned(&pins, &("a".into(), "1".into())));
        toggle(&mut pins, ("a".into(), "1".into()));
        assert_eq!(pins, vec![("b".into(), "2".into())]);
        assert!(!is_pinned(&pins, &("a".into(), "1".into())));
    }

    #[test]
    fn pinned_tasks_resolve_in_pin_order_and_drop_missing() {
        let tasks = vec![task("a", "1", None, 0, false), task("b", "2", None, 0, false)];
        let pins = vec![("b".into(), "2".into()), ("zz".into(), "gone".into()), ("a".into(), "1".into())];
        let resolved = pinned_tasks(&tasks, &pins);
        let ids: Vec<&str> = resolved.iter().map(|t| t.uid.0.as_str()).collect();
        assert_eq!(ids, vec!["2", "1"], "pin order kept, the missing pin dropped");
    }

    #[test]
    fn suggestions_are_due_today_or_overdue_open_and_unpinned() {
        let today = day(2026, 10, 16);
        let tasks = vec![
            task("a", "overdue", Some(day(2026, 10, 14)), 5, false),
            task("a", "today", Some(day(2026, 10, 16)), 5, false),
            task("a", "future", Some(day(2026, 10, 20)), 5, false),
            task("a", "undated", None, 5, false),
            task("a", "done-today", Some(day(2026, 10, 16)), 5, true),
            task("a", "pinned-today", Some(day(2026, 10, 16)), 5, false),
        ];
        let pins = vec![("a".into(), "pinned-today".into())];
        let suggested = suggestions(&tasks, &pins, today);
        let ids: Vec<&str> = suggested.iter().map(|t| t.uid.0.as_str()).collect();
        assert_eq!(ids, vec!["overdue", "today"], "soonest due first; future/undated/completed/pinned excluded");
    }

    #[test]
    fn suggestions_break_due_ties_by_priority_then_title() {
        let today = day(2026, 10, 16);
        let tasks = vec![
            task("a", "zzz", Some(today), 1, false),
            task("a", "aaa", Some(today), 1, false),
            task("a", "low", Some(today), 9, false),
            task("a", "unset", Some(today), 0, false),
        ];
        let suggested = suggestions(&tasks, &[], today);
        let ids: Vec<&str> = suggested.iter().map(|t| t.uid.0.as_str()).collect();
        assert_eq!(ids, vec!["aaa", "zzz", "low", "unset"], "priority 1 first, 0/9 last, ties by title");
    }
}
