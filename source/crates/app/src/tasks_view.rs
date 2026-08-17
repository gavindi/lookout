//! The Tasks module: a grouped to-do list backed by CalDAV `VTODO`
//! resources, following the same data-in/widget-state-out convention as the
//! rest of `calendar_view`: the caller owns the task list and pushes it via
//! [`TasksView::set_tasks`]; the view owns only widget state. Grouping and
//! ordering live in pure, unit-tested functions so the layout logic is
//! testable independent of a running GTK main loop.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::RefCell;
use std::rc::Rc;

use chrono::{Duration, Local, NaiveDate, NaiveDateTime};
use gtk::prelude::*;
use lookout_core::{CalendarTask, TaskStatus};

use crate::calendar_colors::CalendarColorMap;

/// The sections the list renders, in render order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskBucket {
    Overdue,
    Today,
    ThisWeek,
    Later,
    Completed,
}

impl TaskBucket {
    pub fn label(self) -> &'static str {
        match self {
            TaskBucket::Overdue => "Overdue",
            TaskBucket::Today => "Today",
            TaskBucket::ThisWeek => "This week",
            TaskBucket::Later => "Later",
            TaskBucket::Completed => "Completed",
        }
    }
}

/// Classifies one task into its list section. Completed is a status
/// decision; everything else is a due-date window relative to `today`
/// (local time, so "today" means the user's today). A task with no due date
/// lands in "Later" - it isn't overdue, and it has no date to place.
pub fn bucket_for(task: &CalendarTask, today: NaiveDate) -> TaskBucket {
    if task.status == TaskStatus::Completed || task.percent_complete == Some(100) {
        return TaskBucket::Completed;
    }
    let due = task.due.map(|d| d.with_timezone(&Local).date_naive());
    match due {
        Some(due) if due < today => TaskBucket::Overdue,
        Some(due) if due == today => TaskBucket::Today,
        Some(due) if due < today + Duration::days(7) => TaskBucket::ThisWeek,
        _ => TaskBucket::Later,
    }
}

/// Sorts the tasks *within* one bucket: due first (undated last, via
/// `NaiveDate::MAX` - `Option`'s own ordering would put them first), then by
/// RFC 5545 priority (1 is the most urgent, 0 = unset sorts last), then by
/// summary. Stable so equal keys keep fetch order.
pub fn sort_bucket(tasks: &mut [CalendarTask]) {
    tasks.sort_by_key(|t| {
        (
            t.due.map(|d| d.with_timezone(&Local).date_naive()).unwrap_or(NaiveDate::MAX),
            priority_rank(t.priority.0),
            t.summary.clone().unwrap_or_default(),
        )
    });
}

/// Priority 0 (unset) and 9 (lowest) both sort last; 1..=8 sort by value.
fn priority_rank(raw: u8) -> u8 {
    if raw == 0 {
        10
    } else {
        raw
    }
}

/// Groups the full task list into render sections, in
/// [`TaskBucket`] render order, with each bucket's tasks sorted.
pub fn group_tasks(tasks: &[CalendarTask], now: NaiveDateTime) -> Vec<(TaskBucket, Vec<CalendarTask>)> {
    let today = now.date();
    let mut buckets: Vec<(TaskBucket, Vec<CalendarTask>)> = vec![
        (TaskBucket::Overdue, Vec::new()),
        (TaskBucket::Today, Vec::new()),
        (TaskBucket::ThisWeek, Vec::new()),
        (TaskBucket::Later, Vec::new()),
        (TaskBucket::Completed, Vec::new()),
    ];
    for task in tasks {
        let bucket = bucket_for(task, today);
        buckets.iter_mut().find(|(b, _)| *b == bucket).expect("every bucket is pre-seeded").1.push(task.clone());
    }
    for (_, bucket_tasks) in &mut buckets {
        sort_bucket(bucket_tasks);
    }
    buckets.retain(|(_, tasks)| !tasks.is_empty());
    buckets
}

/// The Tasks view's widget state. `root` is the scrolled list; callers read
/// nothing back from it - `set_tasks` repaints from scratch on every push.
/// The toggle/activate callbacks are stored on the view (via
/// [`TasksView::set_handlers`]) so `set_tasks` callers don't need to know
/// about the window or the session routing - the widget owns the handlers,
/// the caller owns the data.
pub struct TasksView {
    pub root: gtk::ScrolledWindow,
    list: gtk::Box,
    toggle: RefCell<Option<ToggleHandler>>,
    activate: RefCell<Option<ActivateHandler>>,
    /// The message shown when the list is empty - the caller sets it to
    /// reflect what task sources exist (see `window::refresh_tasks_view`),
    /// defaulting to a plain "no tasks yet" hint.
    empty_message: RefCell<String>,
}

/// One task row's checkbox flip: the caller should send the task (with
/// status/completed/percent updated) to its account session.
pub type ToggleHandler = Rc<dyn Fn(CalendarTask, bool) + 'static>;
/// A clicked row: the caller should open the task editor for it.
pub(crate) type ActivateHandler = Rc<dyn Fn(CalendarTask) + 'static>;

/// Builds a fresh Tasks view: a scrolled list inside a card, with an empty
/// placeholder for the no-tasks state.
pub fn build_tasks_view() -> TasksView {
    install_tasks_css();
    let list = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .margin_top(8)
        .margin_bottom(8)
        .margin_start(8)
        .margin_end(8)
        .build();
    let root = gtk::ScrolledWindow::builder().child(&list).hscrollbar_policy(gtk::PolicyType::Never).vexpand(true).build();
    TasksView {
        root,
        list,
        toggle: RefCell::new(None),
        activate: RefCell::new(None),
        empty_message: RefCell::new("No tasks yet - use New task to create one.".to_string()),
    }
}

/// Stores the row-action callbacks. Called once at startup; every later
/// `set_tasks` repaint reuses them.
pub fn set_handlers(view: &TasksView, toggle: ToggleHandler, activate: ActivateHandler) {
    *view.toggle.borrow_mut() = Some(toggle);
    *view.activate.borrow_mut() = Some(activate);
}

/// Sets the message the empty state shows when there are no tasks at all -
/// lets the caller distinguish "you have no tasks" from "you have nowhere to
/// put tasks". Has no effect while tasks exist.
pub fn set_empty_message(view: &TasksView, message: &str) {
    *view.empty_message.borrow_mut() = message.to_string();
}

/// The tasks list's handful of display-level CSS rules, installed once per
/// process (multiple installs are harmless, but a `Once` keeps it to one).
fn install_tasks_css() {
    use std::sync::Once;
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let provider = gtk::CssProvider::new();
        provider.load_from_string(
            ".task-summary-done {
                text-decoration: line-through;
                color: alpha(currentColor, 0.6);
            }
            .task-due-overdue {
                color: @lookout-task-overdue;
            }",
        );
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::style_context_add_provider_for_display(&display, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
        }
    });
}

/// Repaints the whole list from `tasks`, grouped by due-date bucket, with a
/// colour dot per calendar. Rebuilds rows from scratch on every call - the
/// same repaint-from-scratch convention as the calendar checklist.
pub fn set_tasks(view: &TasksView, tasks: &[CalendarTask], colors: &CalendarColorMap) {
    let toggle = view.toggle.borrow().clone().unwrap_or_else(|| Rc::new(|_t, _c| {}));
    let activate = view.activate.borrow().clone().unwrap_or_else(|| Rc::new(|_t| {}));

    while let Some(child) = view.list.first_child() {
        view.list.remove(&child);
    }

    if tasks.is_empty() {
        let message = view.empty_message.borrow().clone();
        let placeholder = gtk::Label::builder()
            .label(&message)
            .css_classes(["dim-label", "caption"])
            .xalign(0.0)
            .wrap(true)
            .margin_top(16)
            .build();
        view.list.append(&placeholder);
        return;
    }

    let now = Local::now().naive_local();
    for (bucket, bucket_tasks) in group_tasks(tasks, now) {
        let header = gtk::Label::builder()
            .label(bucket.label())
            .css_classes(["dim-label", "caption-heading"])
            .xalign(0.0)
            .hexpand(true)
            .margin_top(8)
            .margin_bottom(2)
            .build();
        view.list.append(&header);
        for task in bucket_tasks {
            view.list.append(&task_row(&task, colors, toggle.clone(), activate.clone(), &[], true));
        }
    }
}

/// One task row: a completion checkbox, a calendar colour dot, the summary,
/// and a right-aligned due-date caption. Completed rows render dimmed and
/// struck through. Clicking the row body (not the checkbox) activates the
/// row's edit callback. `summary_classes` are extra CSS classes for the
/// summary label - the Mail-screen overview pane passes `["caption"]` so its
/// rows match the event list's text size, the Tasks view and the dashboard
/// pass none. `show_check` omits the completion checkbox - the Mail-screen
/// overview pane hides it, leaving click-to-edit as the row's only action.
/// `pub(crate)` for the Lookout dashboard, whose outstanding-tasks section
/// reuses the same row.
pub(crate) fn task_row(
    task: &CalendarTask,
    colors: &CalendarColorMap,
    toggle: ToggleHandler,
    activate: ActivateHandler,
    summary_classes: &[&str],
    show_check: bool,
) -> gtk::Button {
    let task = task.clone();
    let completed = task.status == TaskStatus::Completed || task.percent_complete == Some(100);

    let check = show_check.then(|| {
        let check = gtk::CheckButton::new();
        check.set_active(completed);
        check
    });

    // The calendar colour dot is cairo-painted (the same trick the "My
    // calendars" checklist uses for its indicators) because a stock GTK
    // widget's `.check`/`.label` nodes ignore display-level colour overrides.
    let color = colors
        .get(&task.calendar_id)
        .map(String::as_str)
        .unwrap_or(crate::calendar_colors::DEFAULT_CHECK_COLOR)
        .to_string();
    let dot = gtk::DrawingArea::builder().width_request(10).height_request(10).valign(gtk::Align::Center).build();
    dot.set_draw_func(move |_, cr, width, height| {
        let (r, g, b) = parse_css_color(&color);
        let radius = width.min(height) as f64 / 2.0 - 1.0;
        cr.arc(width as f64 / 2.0, height as f64 / 2.0, radius, 0.0, 2.0 * std::f64::consts::PI);
        cr.set_source_rgba(r, g, b, 1.0);
        let _ = cr.fill();
    });
    dot.set_tooltip_text(Some(&task.calendar_id.0));

    let summary = gtk::Label::builder()
        .label(task.summary.clone().unwrap_or_else(|| "(untitled)".to_string()))
        .css_classes(summary_classes)
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();

    let due_text = task
        .due
        .map(|d| {
            let local = d.with_timezone(&Local);
            if completed {
                format!("{}", local.format("%a %d %b"))
            } else if bucket_for(&task, Local::now().date_naive()) == TaskBucket::Overdue {
                format!("Overdue · {}", local.format("%a %d %b"))
            } else {
                format!("{}", local.format("%a %d %b"))
            }
        })
        .unwrap_or_default();
    let due = gtk::Label::builder().label(&due_text).css_classes(["dim-label", "caption"]).build();
    if !completed && bucket_for(&task, Local::now().date_naive()) == TaskBucket::Overdue {
        due.add_css_class("task-due-overdue");
    }

    let row_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .margin_start(4)
        .margin_end(4)
        .build();
    if let Some(check) = &check {
        row_box.append(check);
    }
    row_box.append(&dot);
    row_box.append(&summary);
    row_box.append(&due);

    if completed {
        summary.add_css_class("task-summary-done");
    }

    let button = gtk::Button::builder().child(&row_box).css_classes(["flat"]).hexpand(true).build();
    if let Some(check) = &check {
        let task = task.clone();
        check.connect_toggled(move |_| {
            let mut updated = task.clone();
            if updated.status == TaskStatus::Completed || updated.percent_complete == Some(100) {
                updated.status = TaskStatus::NeedsAction;
                updated.completed = None;
                updated.percent_complete = None;
            } else {
                updated.status = TaskStatus::Completed;
                updated.completed = Some(chrono::Utc::now());
                updated.percent_complete = Some(100);
            }
            toggle(updated, !task_completed(&task));
        });
    }
    button.connect_clicked(move |_| activate(task.clone()));
    button
}

fn task_completed(task: &CalendarTask) -> bool {
    task.status == TaskStatus::Completed || task.percent_complete == Some(100)
}

/// Parses a `#rgb`/`#rrggbb` hex colour to an `(r, g, b)` tuple in 0..=1 for
/// Cairo, falling back to a neutral grey on anything unparseable.
/// `pub(crate)` for the Lookout dashboard's event rows.
pub(crate) fn parse_css_color(color: &str) -> (f64, f64, f64) {
    let body = color.trim_start_matches('#');
    let expanded = if body.len() == 3 && body.chars().all(|c| c.is_ascii_hexdigit()) {
        body.chars().flat_map(|c| [c, c]).collect::<String>()
    } else {
        body.to_string()
    };
    let rgba = gtk::gdk::RGBA::parse(format!("#{expanded}")).unwrap_or_else(|_| gtk::gdk::RGBA::new(0.6, 0.6, 0.6, 1.0));
    (f64::from(rgba.red()), f64::from(rgba.green()), f64::from(rgba.blue()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use lookout_core::{CalendarId, TaskPriority};

    fn cal() -> CalendarId {
        CalendarId("test:cal".to_string())
    }

    fn task(uid: &str, due: Option<NaiveDateTime>, status: TaskStatus, percent: Option<u8>, priority: u8) -> CalendarTask {
        CalendarTask {
            uid: lookout_core::TaskUid(uid.to_string()),
            calendar_id: cal(),
            summary: Some(uid.to_string()),
            description: None,
            due: due.map(|d| Local.from_local_datetime(&d).unwrap().with_timezone(&chrono::Utc)),
            start: None,
            completed: None,
            status,
            priority: TaskPriority(priority),
            percent_complete: percent,
            categories: Vec::new(),
            href: None,
            etag: None,
        }
    }

    #[test]
    fn buckets_classify_by_due_window_and_completion() {
        let today = NaiveDate::from_ymd_opt(2026, 8, 10).unwrap();
        let at = |day: u32, hour: u32| NaiveDateTime::new(NaiveDate::from_ymd_opt(2026, 8, day).unwrap(), chrono::NaiveTime::from_hms_opt(hour, 0, 0).unwrap());

        assert_eq!(bucket_for(&task("overdue", Some(at(9, 12)), TaskStatus::NeedsAction, None, 5), today), TaskBucket::Overdue);
        assert_eq!(bucket_for(&task("today", Some(at(10, 9)), TaskStatus::NeedsAction, None, 5), today), TaskBucket::Today);
        assert_eq!(bucket_for(&task("week", Some(at(14, 9)), TaskStatus::NeedsAction, None, 5), today), TaskBucket::ThisWeek);
        assert_eq!(bucket_for(&task("later", Some(at(20, 9)), TaskStatus::NeedsAction, None, 5), today), TaskBucket::Later);
        // Undated tasks are never overdue.
        assert_eq!(bucket_for(&task("undated", None, TaskStatus::NeedsAction, None, 5), today), TaskBucket::Later);
        // Completed is a status decision, whatever the due date says.
        assert_eq!(bucket_for(&task("done", Some(at(1, 9)), TaskStatus::Completed, Some(100), 5), today), TaskBucket::Completed);
        // Percent-complete 100 without STATUS counts as done (common server pattern).
        assert_eq!(
            bucket_for(&task("done-pct", Some(at(1, 9)), TaskStatus::NeedsAction, Some(100), 5), today),
            TaskBucket::Completed
        );
    }

    #[test]
    fn group_tasks_emits_buckets_in_render_order_only_when_non_empty() {
        let now = NaiveDateTime::new(NaiveDate::from_ymd_opt(2026, 8, 10).unwrap(), chrono::NaiveTime::from_hms_opt(12, 0, 0).unwrap());
        let at = |day: u32| NaiveDateTime::new(NaiveDate::from_ymd_opt(2026, 8, day).unwrap(), chrono::NaiveTime::from_hms_opt(9, 0, 0).unwrap());

        let groups = group_tasks(
            &[
                task("later", Some(at(25)), TaskStatus::NeedsAction, None, 5),
                task("done", Some(at(1)), TaskStatus::Completed, Some(100), 5),
                task("overdue", Some(at(2)), TaskStatus::NeedsAction, None, 5),
                task("today", Some(at(10)), TaskStatus::NeedsAction, None, 5),
            ],
            now,
        );
        let labels: Vec<&str> = groups.iter().map(|(b, _)| b.label()).collect();
        assert_eq!(labels, vec!["Overdue", "Today", "Later", "Completed"]);
    }

    #[test]
    fn sort_bucket_orders_by_due_then_priority_then_summary() {
        let at = |day: u32| NaiveDateTime::new(NaiveDate::from_ymd_opt(2026, 8, day).unwrap(), chrono::NaiveTime::from_hms_opt(9, 0, 0).unwrap());

        let mut tasks = vec![
            task("b-later", Some(at(12)), TaskStatus::NeedsAction, None, 1),
            task("a-undated", None, TaskStatus::NeedsAction, None, 1),
            task("c-prio9", Some(at(11)), TaskStatus::NeedsAction, None, 9),
            task("c-prio0", Some(at(11)), TaskStatus::NeedsAction, None, 0),
            task("c-prio1", Some(at(11)), TaskStatus::NeedsAction, None, 1),
        ];
        sort_bucket(&mut tasks);
        let order: Vec<&str> = tasks.iter().map(|t| t.summary.as_deref().unwrap()).collect();
        assert_eq!(order, vec!["c-prio1", "c-prio9", "c-prio0", "b-later", "a-undated"]);
    }
}
