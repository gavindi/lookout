//! Completion semantics for recurring tasks (T-06 / item 4.10).
//!
//! Lookout keeps one VTODO per recurring series (the CalDAV model), so
//! completing an occurrence rolls the task forward *in place*: the series
//! anchor (`start`) stays put, the next due date replaces the old one, and the
//! completion fields reset. A series whose `COUNT`/`UNTIL` is exhausted, or a
//! provider that rolls recurring tasks itself, keeps the ordinary "completed"
//! state instead.
//!
//! The function is pure and shared by the Tasks view's checkbox and the task
//! editor's `% complete` field, so both routes behave identically.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use lookout_core::{CalendarTask, TaskStatus};

/// Microsoft To Do rolls a recurring task on completion server-side; applying
/// the same transition client-side as well would leave a duplicate next
/// occurrence behind. CalDAV and local lists need the client roll.
fn provider_rolls_forward(calendar_id: &lookout_core::CalendarId) -> bool {
    !calendar_id.0.starts_with("todo:")
}

fn is_completed(task: &CalendarTask) -> bool {
    task.status == TaskStatus::Completed || task.percent_complete == Some(100)
}

/// Rolls `task` to its next occurrence when the save is a *new* completion of
/// a recurring task. Returns whether a roll happened.
pub fn roll_forward_if_completed(previous: Option<&CalendarTask>, task: &mut CalendarTask) -> bool {
    if !is_completed(task) || previous.is_some_and(is_completed) {
        return false;
    }
    if task.rrule.is_none() || !provider_rolls_forward(&task.calendar_id) {
        return false;
    }
    let Some((start, due)) = lookout_dav::next_task_occurrence(task) else {
        return false;
    };
    task.start = Some(start);
    task.due = Some(due);
    task.status = TaskStatus::NeedsAction;
    task.completed = None;
    task.percent_complete = None;
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use lookout_core::{CalendarId, TaskPriority, TaskUid};

    fn task(calendar_id: &str, rrule: Option<&str>, completed: bool) -> CalendarTask {
        CalendarTask {
            uid: TaskUid("t-1".to_string()),
            calendar_id: CalendarId(calendar_id.to_string()),
            summary: Some("Recurring".to_string()),
            description: None,
            due: Some("2026-07-15T09:00:00Z".parse().unwrap()),
            start: Some("2026-07-15T09:00:00Z".parse().unwrap()),
            rrule: rrule.map(str::to_string),
            created: None,
            completed: completed.then(|| "2026-07-15T10:00:00Z".parse().unwrap()),
            status: if completed { TaskStatus::Completed } else { TaskStatus::NeedsAction },
            priority: TaskPriority(0),
            percent_complete: completed.then_some(100),
            reminder_minutes_before: None,
            categories: Vec::new(),
            href: None,
            etag: None,
        }
    }

    #[test]
    fn completing_a_recurring_task_rolls_it_to_the_next_occurrence() {
        let previous = task("cal", Some("FREQ=DAILY"), false);
        let mut updated = previous.clone();
        updated.status = TaskStatus::Completed;
        updated.completed = Some("2026-07-15T10:00:00Z".parse().unwrap());
        updated.percent_complete = Some(100);
        assert!(roll_forward_if_completed(Some(&previous), &mut updated));
        assert_eq!(updated.due, Some("2026-07-16T09:00:00Z".parse().unwrap()));
        assert_eq!(updated.status, TaskStatus::NeedsAction);
        assert_eq!(updated.completed, None);
        assert_eq!(updated.percent_complete, None);
        assert_eq!(updated.rrule.as_deref(), Some("FREQ=DAILY"));
    }

    #[test]
    fn only_a_transition_into_completion_rolls() {
        let completed = task("cal", Some("FREQ=DAILY"), true);
        let mut already = completed.clone();
        assert!(!roll_forward_if_completed(Some(&completed), &mut already));

        let previous = task("cal", Some("FREQ=DAILY"), false);
        let mut unchecked = previous.clone();
        assert!(!roll_forward_if_completed(Some(&previous), &mut unchecked));

        let mut one_off = task("cal", None, false);
        one_off.status = TaskStatus::Completed;
        assert!(!roll_forward_if_completed(None, &mut one_off));
    }

    #[test]
    fn graph_to_do_and_exhausted_series_keep_the_completed_state() {
        let previous = task("todo:list-1", Some("FREQ=DAILY"), false);
        let mut updated = previous.clone();
        updated.status = TaskStatus::Completed;
        assert!(!roll_forward_if_completed(Some(&previous), &mut updated));

        let previous = task("cal", Some("FREQ=DAILY;COUNT=1"), false);
        let mut updated = previous.clone();
        updated.status = TaskStatus::Completed;
        assert!(!roll_forward_if_completed(Some(&previous), &mut updated));
        assert_eq!(updated.status, TaskStatus::Completed);
    }
}
