//! The Tasks module: a grouped to-do list backed by CalDAV `VTODO`
//! resources, following the same data-in/widget-state-out convention as the
//! rest of `calendar_view`: the caller owns the task list and pushes it via
//! [`TasksView::set_tasks`]; the view owns only widget state. Grouping and
//! ordering live in pure, unit-tested functions so the layout logic is
//! testable independent of a running GTK main loop.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use chrono::{Duration, Local, NaiveDate, NaiveDateTime, Weekday};
use gtk::prelude::*;
use lookout_core::{CalendarId, CalendarTask, EmailSummary, TaskStatus};

use crate::calendar_colors::CalendarColorMap;
use crate::categories::{CategoryDef, CategorySet};

/// How tasks are ordered within their group (T-09). Set from the
/// `tasks-sort-key` GSettings string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TaskSortKey {
    /// Soonest due first, undated last (the default).
    #[default]
    Due,
    /// Most important first (RFC 5545 priority; unset sorts last).
    Importance,
    /// Title A→Z.
    Title,
    /// Newest created first.
    Created,
}

impl TaskSortKey {
    /// Parses a stored `tasks-sort-key` value; unknown falls back to `Due`.
    pub fn from_setting(value: &str) -> Self {
        match value {
            "importance" => TaskSortKey::Importance,
            "title" => TaskSortKey::Title,
            "created" => TaskSortKey::Created,
            _ => TaskSortKey::Due,
        }
    }
}

/// How tasks are sectioned (T-09). Set from the `tasks-group-by` string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TaskGroupBy {
    /// Due-date buckets (Overdue/Today/This week/Later/Completed), Outlook's
    /// default and the app's original behaviour.
    #[default]
    Due,
    /// One section per task list (calendar).
    List,
    /// One section per category; a task with several appears under each.
    Category,
    /// A single ungrouped list.
    None,
}

impl TaskGroupBy {
    /// Parses a stored `tasks-group-by` value; unknown falls back to `Due`.
    pub fn from_setting(value: &str) -> Self {
        match value {
            "list" => TaskGroupBy::List,
            "category" => TaskGroupBy::Category,
            "none" => TaskGroupBy::None,
            _ => TaskGroupBy::Due,
        }
    }
}

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

/// Sorts the tasks *within* one bucket by due date then priority then summary
/// - the original ordering, kept for the dashboard's due-bucketed list.
pub fn sort_bucket(tasks: &mut [CalendarTask]) {
    sort_tasks(tasks, TaskSortKey::Due);
}

/// A task's due date in local time, undated sorting last.
fn due_key(task: &CalendarTask) -> NaiveDate {
    task.due.map(|d| d.with_timezone(&Local).date_naive()).unwrap_or(NaiveDate::MAX)
}

/// Sorts tasks within one group by the chosen key (T-09). Every key falls
/// back to a stable tie-break so equal tasks keep fetch order.
pub fn sort_tasks(tasks: &mut [CalendarTask], key: TaskSortKey) {
    tasks.sort_by(|a, b| {
        let by_due = due_key(a).cmp(&due_key(b));
        let by_priority = priority_rank(a.priority.0).cmp(&priority_rank(b.priority.0));
        let by_title = a
            .summary
            .clone()
            .unwrap_or_default()
            .to_lowercase()
            .cmp(&b.summary.clone().unwrap_or_default().to_lowercase());
        match key {
            TaskSortKey::Due => by_due.then(by_priority).then(by_title),
            TaskSortKey::Importance => by_priority.then(by_due).then(by_title),
            TaskSortKey::Title => by_title.then(by_due).then(by_priority),
            // Newest first; `Option`'s `None < Some` reversed puts undated
            // (created-less) tasks last.
            TaskSortKey::Created => b.created.cmp(&a.created).then(by_title).then(by_due),
        }
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

/// Arranges tasks into render sections per the chosen grouping (T-09), each
/// section's tasks ordered by `sort`. Returns `(label, tasks)` pairs in render
/// order; an empty label means the group is ungrouped (`TaskGroupBy::None`)
/// and renders without a header. `list_names` maps a task's list (calendar)
/// id to its display name for the `List` grouping.
pub fn arrange_tasks(
    tasks: &[CalendarTask],
    sort: TaskSortKey,
    group: TaskGroupBy,
    now: NaiveDateTime,
    list_names: &HashMap<CalendarId, String>,
) -> Vec<(String, Vec<CalendarTask>)> {
    if group == TaskGroupBy::Due {
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
        let mut groups = Vec::new();
        for (bucket, mut bucket_tasks) in buckets {
            if bucket_tasks.is_empty() {
                continue;
            }
            sort_tasks(&mut bucket_tasks, sort);
            groups.push((bucket.label().to_string(), bucket_tasks));
        }
        return groups;
    }

    let mut map: HashMap<String, Vec<CalendarTask>> = HashMap::new();
    for task in tasks {
        match group {
            TaskGroupBy::List => {
                let label = list_names.get(&task.calendar_id).cloned().unwrap_or_else(|| task.calendar_id.0.clone());
                map.entry(label).or_default().push(task.clone());
            }
            TaskGroupBy::Category => {
                if task.categories.is_empty() {
                    map.entry("No category".to_string()).or_default().push(task.clone());
                } else {
                    let mut seen = std::collections::HashSet::new();
                    for name in &task.categories {
                        if seen.insert(name.to_lowercase()) {
                            map.entry(name.clone()).or_default().push(task.clone());
                        }
                    }
                }
            }
            TaskGroupBy::None => map.entry(String::new()).or_default().push(task.clone()),
            TaskGroupBy::Due => unreachable!("handled above"),
        }
    }

    let mut keys: Vec<String> = map.keys().cloned().collect();
    if group == TaskGroupBy::Category {
        // "No category" is the catch-all and sorts last.
        keys.sort_by(|a, b| (a == "No category").cmp(&(b == "No category")).then_with(|| a.to_lowercase().cmp(&b.to_lowercase())));
    } else {
        keys.sort_by_key(|key| key.to_lowercase());
    }
    let mut groups = Vec::new();
    for key in keys {
        let mut group_tasks = map.remove(&key).unwrap_or_default();
        sort_tasks(&mut group_tasks, sort);
        groups.push((key, group_tasks));
    }
    groups
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

/// Which smart list / source the Tasks navigation pane has selected (item 4.8
/// / T-01). `All` is Outlook's "Tasks" smart list; `MyDay` the per-day pin set
/// (item 4.9 / T-02); `Flagged` the follow-up-flagged mail section (item 1.16);
/// `List` one source (a CalDAV calendar, Google list, To Do list or local
/// list).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum TasksFilter {
    /// Every task from every source - Outlook's "Tasks".
    #[default]
    All,
    /// The My Day pin set plus its due-today/overdue suggestions.
    MyDay,
    /// Open follow-up-flagged mail.
    Flagged,
    /// One task list / calendar.
    List(CalendarId),
}

/// One row in the Tasks navigation pane. Colour dots are for source lists;
/// smart lists have none. `list_id` is `Some` for a user list (which offers
/// Rename/Delete), `None` for a smart list.
pub struct TasksNavRow {
    pub label: String,
    pub filter: TasksFilter,
    pub color: Option<String>,
    pub list_id: Option<CalendarId>,
}

/// What a Tasks navigation row asked for.
pub enum TasksNavAction {
    Select(TasksFilter),
    /// The trailing "+ New list" row.
    NewList,
    RenameList(CalendarId),
    DeleteList(CalendarId),
}

/// A Tasks navigation action callback. Stored on the view (like the toggle
/// handler) so the caller owns the routing.
pub type TasksNavHandler = Rc<dyn Fn(TasksNavAction) + 'static>;

/// The Add/Remove-from-My-Day action (item 4.9 / T-02): the caller flips the
/// pin set, persists it, and repaints.
pub type MyDayToggleHandler = Rc<dyn Fn(crate::my_day::MyDayKey) + 'static>;

/// The Tasks view's widget state. `root` is the paned navigation-pane + task
/// list; callers read nothing back from it - `set_tasks` repaints from scratch
/// on every push. The toggle/activate callbacks are stored on the view (via
/// [`set_handlers`]) so `set_tasks` callers don't need to know about the
/// window or the session routing - the widget owns the handlers, the caller
/// owns the data.
pub struct TasksView {
    pub root: gtk::Paned,
    /// The navigation pane's row list (item 4.8 / T-01), rebuilt by
    /// [`set_nav`] from the caller's rows.
    sidebar: gtk::ListBox,
    list: gtk::Box,
    /// The "Flagged email" section above the task buckets (item 1.16),
    /// repainted on its own by [`set_flagged_mail`].
    flagged: gtk::Box,
    toggle: RefCell<Option<ToggleHandler>>,
    activate: RefCell<Option<ActivateHandler>>,
    flag_complete: RefCell<Option<FlagCompleteHandler>>,
    flag_open: RefCell<Option<FlagOpenHandler>>,
    /// The message shown when the list is empty - the caller sets it to
    /// reflect what task sources exist (see `window::refresh_tasks_view`),
    /// defaulting to a plain "no tasks yet" hint.
    empty_message: RefCell<String>,
    /// The current sort key (T-09), set from the toolbar/settings.
    sort: RefCell<TaskSortKey>,
    /// The current grouping (T-09).
    group: RefCell<TaskGroupBy>,
    /// Whether completed tasks are shown.
    show_completed: RefCell<bool>,
    /// Which navigation row is selected (item 4.8 / T-01); drives which tasks
    /// `set_tasks` renders and whether the flagged-mail section is shown.
    filter: RefCell<TasksFilter>,
    /// The navigation-pane action callback (item 4.8 / T-01), set once by the
    /// caller via [`set_nav`]. Shared with the sidebar's `row-selected`
    /// connection, which is wired once at build time.
    nav_action: Rc<RefCell<Option<TasksNavHandler>>>,
    /// The filter each navigation row maps to, parallel to the sidebar's rows -
    /// the lookup `row-selected` uses to name the selection.
    nav_filters: Rc<RefCell<Vec<TasksFilter>>>,
    /// Mutes `row-selected` while `set_nav` marks the selected row
    /// programmatically, so re-selecting can't recurse through the handler.
    nav_suppress: Rc<std::cell::Cell<bool>>,
    /// The current My Day pin set (item 4.9 / T-02), mirrored from the caller so
    /// a task row's context action can label itself Add or Remove.
    my_day_pins: Rc<RefCell<Vec<crate::my_day::MyDayKey>>>,
    /// The Add/Remove-from-My-Day action (item 4.9 / T-02), set once by the
    /// caller via [`set_my_day_handler`].
    my_day_toggle: Rc<RefCell<Option<MyDayToggleHandler>>>,
    /// The live shared category master list (2.1), read to colour the task
    /// rows' category chips. Held as the same `Rc` the rest of the app
    /// mutates, so a category colour change needs no repaint bookkeeping.
    categories: Rc<RefCell<CategorySet>>,
}

/// One task row's checkbox flip: the caller receives the pre-toggle task (for
/// roll-forward and Undo) and the updated task (with status/completed/percent
/// rewritten), then sends it to its account session.
pub type ToggleHandler = Rc<dyn Fn(CalendarTask, CalendarTask, bool) + 'static>;
/// A clicked row: the caller should open the task editor for it.
pub(crate) type ActivateHandler = Rc<dyn Fn(CalendarTask) + 'static>;
/// A flagged-email row's checkbox: the caller marks the message's flag
/// complete.
pub(crate) type FlagCompleteHandler = Rc<dyn Fn(EmailSummary) + 'static>;
/// A clicked flagged-email row: the caller opens the message.
pub(crate) type FlagOpenHandler = Rc<dyn Fn(EmailSummary) + 'static>;

/// Builds a fresh Tasks view: a navigation pane (item 4.8 / T-01) beside a
/// scrolled task list, with an empty placeholder for the no-tasks state.
/// `categories` is the shared category master list (2.1), kept live for the
/// rows' category chips.
pub fn build_tasks_view(categories: Rc<RefCell<CategorySet>>) -> TasksView {
    install_tasks_css();
    let list = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .margin_top(8)
        .margin_bottom(8)
        .margin_start(8)
        .margin_end(8)
        .build();
    let flagged = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .margin_top(8)
        .margin_start(8)
        .margin_end(8)
        .visible(false)
        .build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&flagged);
    content.append(&list);
    let list_scroller = gtk::ScrolledWindow::builder()
        .child(&content)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .build();

    // The navigation pane: smart lists and per-source rows, rebuilt by
    // `set_nav` as sources change.
    let sidebar = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::Single)
        .css_classes(["navigation-sidebar"])
        .build();
    let sidebar_scroller = gtk::ScrolledWindow::builder()
        .child(&sidebar)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .build();
    // The outer Tasks card provides the frame, so the sidebar is a plain
    // fixed-width column (no nested card styling).
    let sidebar_card = gtk::Box::new(gtk::Orientation::Vertical, 0);
    sidebar_card.set_width_request(220);
    sidebar_card.append(&sidebar_scroller);

    let root = gtk::Paned::builder()
        .orientation(gtk::Orientation::Horizontal)
        .start_child(&sidebar_card)
        .end_child(&list_scroller)
        .resize_start_child(false)
        .resize_end_child(true)
        .shrink_start_child(false)
        .shrink_end_child(false)
        .position(220)
        .build();
    root.add_css_class("seamless-paned");

    let nav_action: Rc<RefCell<Option<TasksNavHandler>>> = Rc::new(RefCell::new(None));
    let nav_filters: Rc<RefCell<Vec<TasksFilter>>> = Rc::new(RefCell::new(Vec::new()));
    let nav_suppress: Rc<std::cell::Cell<bool>> = Rc::new(std::cell::Cell::new(false));
    let my_day_pins: Rc<RefCell<Vec<crate::my_day::MyDayKey>>> = Rc::new(RefCell::new(Vec::new()));
    let my_day_toggle: Rc<RefCell<Option<MyDayToggleHandler>>> = Rc::new(RefCell::new(None));
    // Selection is reported through the stored handler; the sidebar itself
    // holds no state, so rebuilding its rows never stacks handlers. The last
    // row is the "+ New list" entry, which maps to `NewList` rather than a
    // filter. `nav_suppress` mutes the programmatic selection `set_nav`
    // performs, so re-selecting the current row can't recurse.
    {
        let nav_action = nav_action.clone();
        let nav_filters = nav_filters.clone();
        let nav_suppress = nav_suppress.clone();
        sidebar.connect_row_selected(move |_list, row| {
            if nav_suppress.get() {
                return;
            }
            let Some(row) = row else { return };
            let index = row.index();
            if index < 0 {
                return;
            }
            let Some(handler) = nav_action.borrow().clone() else { return };
            let filters = nav_filters.borrow();
            let action = if (index as usize) < filters.len() {
                TasksNavAction::Select(filters[index as usize].clone())
            } else {
                TasksNavAction::NewList
            };
            drop(filters);
            handler(action);
        });
    }

    TasksView {
        root,
        sidebar,
        list,
        flagged,
        toggle: RefCell::new(None),
        activate: RefCell::new(None),
        flag_complete: RefCell::new(None),
        flag_open: RefCell::new(None),
        empty_message: RefCell::new("No tasks yet - use New task to create one.".to_string()),
        sort: RefCell::new(TaskSortKey::default()),
        group: RefCell::new(TaskGroupBy::default()),
        show_completed: RefCell::new(true),
        filter: RefCell::new(TasksFilter::default()),
        nav_action,
        nav_filters,
        nav_suppress,
        my_day_pins,
        my_day_toggle,
        categories,
    }
}

/// Stores the sort/group/show-completed options the next `set_tasks` uses
/// (T-09). Called at startup from the persisted settings and on every
/// toolbar/settings change.
pub fn set_view_options(view: &TasksView, sort: TaskSortKey, group: TaskGroupBy, show_completed: bool) {
    *view.sort.borrow_mut() = sort;
    *view.group.borrow_mut() = group;
    *view.show_completed.borrow_mut() = show_completed;
}

/// Stores the row-action callbacks. Called once at startup; every later
/// `set_tasks` repaint reuses them.
pub fn set_handlers(view: &TasksView, toggle: ToggleHandler, activate: ActivateHandler) {
    *view.toggle.borrow_mut() = Some(toggle);
    *view.activate.borrow_mut() = Some(activate);
}

/// Stores the navigation-pane action callback (item 4.8 / T-01). Called once
/// at startup, like [`set_handlers`].
pub fn set_nav_handler(view: &TasksView, handler: TasksNavHandler) {
    *view.nav_action.borrow_mut() = Some(handler);
}

/// Stores the Add/Remove-from-My-Day action (item 4.9 / T-02). Called once by
/// the caller.
pub fn set_my_day_handler(view: &TasksView, handler: MyDayToggleHandler) {
    *view.my_day_toggle.borrow_mut() = Some(handler);
}

/// Stores the current My Day pin set (item 4.9 / T-02) so a task row's context
/// action can label itself Add or Remove. Called before every repaint from the
/// caller's in-memory mirror of the database.
pub fn set_my_day_pins(view: &TasksView, pins: &[crate::my_day::MyDayKey]) {
    *view.my_day_pins.borrow_mut() = pins.to_vec();
}

/// Rebuilds the navigation pane from `rows`, marks `selected` (without
/// emitting a selection action), and stores the row→filter map `row-selected`
/// reads. Source rows get a colour dot; user lists get a trailing ⋯ menu with
/// Rename/Delete. A trailing "+ New list" row is always appended.
pub fn set_nav(view: &TasksView, rows: &[TasksNavRow], selected: &TasksFilter) {
    *view.filter.borrow_mut() = selected.clone();
    while let Some(child) = view.sidebar.first_child() {
        view.sidebar.remove(&child);
    }
    let mut filters: Vec<TasksFilter> = Vec::with_capacity(rows.len() + 1);
    let handler = view.nav_action.borrow().clone().unwrap_or_else(|| Rc::new(|_| {}));
    let mut selected_index: i32 = -1;
    for (index, row) in rows.iter().enumerate() {
        let row_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .margin_start(6)
            .margin_end(6)
            .margin_top(4)
            .margin_bottom(4)
            .build();
        if let Some(color) = &row.color {
            let color = color.clone();
            let dot = gtk::DrawingArea::builder().width_request(10).height_request(10).valign(gtk::Align::Center).build();
            dot.set_draw_func(move |_, cr, width, height| {
                let (r, g, b) = crate::calendar_colors::rgb(&color);
                let radius = width.min(height) as f64 / 2.0 - 1.0;
                cr.arc(width as f64 / 2.0, height as f64 / 2.0, radius.max(1.0), 0.0, 2.0 * std::f64::consts::PI);
                cr.set_source_rgba(r, g, b, 1.0);
                let _ = cr.fill();
            });
            row_box.append(&dot);
        }
        let label = gtk::Label::builder()
            .label(&row.label)
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        if row.list_id.is_none() {
            // My Day / Flagged email / Tasks are app chrome; list names are content.
            crate::privacy::mark_ui(&label);
        }
        row_box.append(&label);
        if let Some(list_id) = &row.list_id {
            let menu = gtk::MenuButton::builder()
                .icon_name("view-more-symbolic")
                .css_classes(["flat"])
                .tooltip_text("More options")
                .build();
            let popover = gtk::Popover::new();
            let menu_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).build();
            let rename = gtk::Button::builder().label("Rename list").css_classes(["flat"]).halign(gtk::Align::Start).build();
            let delete = gtk::Button::builder().label("Delete list").css_classes(["flat"]).halign(gtk::Align::Start).build();
            menu_box.append(&rename);
            menu_box.append(&delete);
            popover.set_child(Some(&menu_box));
            menu.set_popover(Some(&popover));
            {
                let handler = handler.clone();
                let list_id = list_id.clone();
                let popover = popover.clone();
                rename.connect_clicked(move |_| {
                    popover.popdown();
                    handler(TasksNavAction::RenameList(list_id.clone()));
                });
            }
            {
                let handler = handler.clone();
                let list_id = list_id.clone();
                let popover = popover.clone();
                delete.connect_clicked(move |_| {
                    popover.popdown();
                    handler(TasksNavAction::DeleteList(list_id.clone()));
                });
            }
            row_box.append(&menu);
        }
        let list_row = gtk::ListBoxRow::builder().child(&row_box).activatable(true).build();
        view.sidebar.append(&list_row);
        if *selected == row.filter {
            selected_index = index as i32;
        }
        filters.push(row.filter.clone());
    }
    // "+ New list" is a plain row (no colour dot, no menu).
    {
        let new_row = gtk::ListBoxRow::builder().activatable(true).build();
        let label = gtk::Label::builder()
            .label("+ New list")
            .xalign(0.0)
            .margin_start(6)
            .margin_top(4)
            .margin_bottom(4)
            .css_classes(["dim-label"])
            .build();
        crate::privacy::mark_ui(&label);
        new_row.set_child(Some(&label));
        view.sidebar.append(&new_row);
    }
    *view.nav_filters.borrow_mut() = filters;
    // Mark the selected row without re-emitting a selection action.
    if selected_index >= 0 {
        if let Some(row) = view.sidebar.row_at_index(selected_index) {
            view.nav_suppress.set(true);
            view.sidebar.select_row(Some(&row));
            view.nav_suppress.set(false);
        }
    }
}

/// Stores the flagged-email row callbacks (item 1.16). Called once at
/// startup, like [`set_handlers`].
pub(crate) fn set_flag_handlers(view: &TasksView, complete: FlagCompleteHandler, open: FlagOpenHandler) {
    *view.flag_complete.borrow_mut() = Some(complete);
    *view.flag_open.borrow_mut() = Some(open);
}

/// The messages the "Flagged email" section lists: open follow-up flags only
/// (completing one drops it, as Outlook's To Do list does), most urgent
/// first - by `FlagBucket`, then due date, then newest message.
pub fn flagged_mail_rows(messages: &[EmailSummary], today: NaiveDate, week_start: Weekday) -> Vec<EmailSummary> {
    let mut rows: Vec<(crate::follow_up::FlagBucket, EmailSummary)> = messages
        .iter()
        .filter(|message| message.is_flagged_for_follow_up())
        .map(|message| (crate::follow_up::bucket(message.follow_up().as_ref(), today, week_start), message.clone()))
        .collect();
    rows.sort_by(|(a_bucket, a), (b_bucket, b)| {
        let due = |m: &EmailSummary| m.follow_up().and_then(|flag| flag.due);
        a_bucket.cmp(b_bucket).then_with(|| due(a).cmp(&due(b))).then_with(|| b.date.cmp(&a.date))
    });
    rows.into_iter().map(|(_, message)| message).collect()
}

/// Repaints the "Flagged email" section from `messages` (every account's
/// flagged mail). Shown only while the Flagged email navigation row is
/// selected (item 4.8 / T-01); hidden and emptied otherwise.
pub fn set_flagged_mail(view: &TasksView, messages: &[EmailSummary]) {
    while let Some(child) = view.flagged.first_child() {
        view.flagged.remove(&child);
    }
    if *view.filter.borrow() != TasksFilter::Flagged {
        view.flagged.set_visible(false);
        return;
    }
    let zone = crate::calendar_tz::display_zone();
    let today = zone.today();
    let rows = flagged_mail_rows(messages, today, crate::message_list::current_week_start());
    view.flagged.set_visible(!rows.is_empty());
    if rows.is_empty() {
        return;
    }
    let flagged_heading = gtk::Label::builder()
        .label("Flagged email")
        .css_classes(["dim-label", "caption-heading"])
        .xalign(0.0)
        .margin_bottom(2)
        .build();
    crate::privacy::mark_ui(&flagged_heading);
    view.flagged.append(&flagged_heading);
    let complete = view.flag_complete.borrow().clone().unwrap_or_else(|| Rc::new(|_| {}));
    let open = view.flag_open.borrow().clone().unwrap_or_else(|| Rc::new(|_| {}));
    for message in rows {
        view.flagged.append(&flagged_mail_row(&message, today, Some(complete.clone()), open.clone(), &[]));
    }
}

/// Opens a message through the `app.open-message` action - the click
/// handler for flagged-email rows outside the Tasks view (the dashboard and
/// the Mail overview pane), which hold no window handles of their own.
pub(crate) fn open_message_through_app() -> FlagOpenHandler {
    Rc::new(|message: EmailSummary| {
        if let Some(app) = gtk::gio::Application::default() {
            let target = format!("{}:{}", message.uid.0, message.mailbox.0);
            app.activate_action("open-message", Some(&target.to_variant()));
        }
    })
}

/// One flagged-email row: a completion checkbox (when `complete` is given),
/// a mail icon, the subject over the sender, and the due label ("Due Tue",
/// "Overdue"). `summary_classes` are extra classes for the subject, as in
/// [`task_row`]. `pub(crate)` for the dashboard and the Mail overview pane.
pub(crate) fn flagged_mail_row(message: &EmailSummary, today: NaiveDate, complete: Option<FlagCompleteHandler>, open: FlagOpenHandler, summary_classes: &[&str]) -> gtk::Button {
    let check = complete.as_ref().map(|_| {
        let check = gtk::CheckButton::new();
        check.set_tooltip_text(Some("Mark complete"));
        check
    });
    let icon = gtk::Image::builder().icon_name("mail-unread-symbolic").pixel_size(12).valign(gtk::Align::Center).build();
    let subject = gtk::Label::builder()
        .label(message.subject.as_deref().filter(|s| !s.trim().is_empty()).unwrap_or("(no subject)"))
        .css_classes(summary_classes)
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    let sender = gtk::Label::builder()
        .label(message.from.first().map(|a| a.display_label().to_string()).unwrap_or_default())
        .css_classes(["dim-label", "caption"])
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    // Flagged-mail rows show the sender's name: blurred by privacy mode.
    crate::privacy::mark(&sender);
    let text = gtk::Box::builder().orientation(gtk::Orientation::Vertical).hexpand(true).build();
    text.append(&subject);
    text.append(&sender);
    let due_date = message.follow_up().as_ref().and_then(crate::follow_up::due_date);
    let due = gtk::Label::builder()
        .label(due_date.map(|d| crate::follow_up::due_label(d, today)).unwrap_or_default())
        .css_classes(["dim-label", "caption"])
        .build();
    if due_date.is_some_and(|d| d < today) {
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
    row_box.append(&icon);
    row_box.append(&text);
    row_box.append(&due);
    let button = gtk::Button::builder().child(&row_box).css_classes(["flat"]).hexpand(true).build();
    // Subject/sender/due are content: blurred by privacy mode.
    crate::privacy::mark_content(&button);
    if let (Some(check), Some(complete)) = (&check, complete) {
        let message = message.clone();
        check.connect_toggled(move |check| {
            if check.is_active() {
                complete(message.clone());
            }
        });
    }
    let message = message.clone();
    button.connect_clicked(move |_| open(message.clone()));
    button
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

/// Repaints the whole list from `tasks`, arranged by the view's current
/// sort/group options (T-09), with a colour dot per calendar and a chip per
/// category. `list_names` maps a list (calendar) id to its display name for
/// the "group by list" mode. Rebuilds rows from scratch on every call - the
/// same repaint-from-scratch convention as the calendar checklist.
pub fn set_tasks(view: &TasksView, tasks: &[CalendarTask], colors: &CalendarColorMap, list_names: &HashMap<CalendarId, String>) {
    let toggle = view.toggle.borrow().clone().unwrap_or_else(|| Rc::new(|_t, _u, _c| {}));
    let activate = view.activate.borrow().clone().unwrap_or_else(|| Rc::new(|_t| {}));
    let sort = *view.sort.borrow();
    let group = *view.group.borrow();
    let show_completed = *view.show_completed.borrow();
    let selected = view.filter.borrow().clone();
    let categories: Vec<CategoryDef> = view.categories.borrow().categories.clone();

    while let Some(child) = view.list.first_child() {
        view.list.remove(&child);
    }

    // My Day and Flagged email are rendered by their own setters; the main
    // task list renders `All` and a single `List`.
    match selected {
        TasksFilter::MyDay | TasksFilter::Flagged => return,
        TasksFilter::All | TasksFilter::List(_) => {}
    }
    let scoped: Vec<CalendarTask> = match &selected {
        TasksFilter::List(calendar_id) => tasks.iter().filter(|task| task.calendar_id == *calendar_id).cloned().collect(),
        _ => tasks.to_vec(),
    };
    let visible: Vec<CalendarTask> = if show_completed {
        scoped
    } else {
        scoped.into_iter().filter(|task| !task_completed(task)).collect()
    };

    if visible.is_empty() {
        let message = view.empty_message.borrow().clone();
        let placeholder = gtk::Label::builder()
            .label(&message)
            .css_classes(["dim-label", "caption"])
            .xalign(0.0)
            .wrap(true)
            .margin_top(16)
            .build();
        crate::privacy::mark_ui(&placeholder);
        view.list.append(&placeholder);
        return;
    }

    let now = Local::now().naive_local();
    for (label, group_tasks) in arrange_tasks(&visible, sort, group, now, list_names) {
        // An empty label is the ungrouped mode - no header.
        if !label.is_empty() {
            let header = gtk::Label::builder()
                .label(&label)
                .css_classes(["dim-label", "caption-heading"])
                .xalign(0.0)
                .hexpand(true)
                .margin_top(8)
                .margin_bottom(2)
                .build();
            view.list.append(&header);
        }
        for task in group_tasks {
            let row = task_row(&task, colors, toggle.clone(), activate.clone(), &[], true, &categories);
            attach_my_day_menu(&row, &task, &view.my_day_pins, &view.my_day_toggle);
            view.list.append(&row);
        }
    }
}

/// Repaints the My Day smart list (item 4.9 / T-02): the pinned tasks, then
/// the caller-computed due-today/overdue suggestions. Renders only while the
/// My Day row is selected - the other filters' setters own their own lists.
pub fn set_my_day(view: &TasksView, pinned: &[CalendarTask], suggestions: &[CalendarTask], colors: &CalendarColorMap) {
    if *view.filter.borrow() != TasksFilter::MyDay {
        return;
    }
    let toggle = view.toggle.borrow().clone().unwrap_or_else(|| Rc::new(|_t, _u, _c| {}));
    let activate = view.activate.borrow().clone().unwrap_or_else(|| Rc::new(|_t| {}));
    let show_completed = *view.show_completed.borrow();
    let categories: Vec<CategoryDef> = view.categories.borrow().categories.clone();

    while let Some(child) = view.list.first_child() {
        view.list.remove(&child);
    }

    let pinned: Vec<CalendarTask> = if show_completed {
        pinned.to_vec()
    } else {
        pinned.iter().filter(|task| !task_completed(task)).cloned().collect()
    };

    if pinned.is_empty() && suggestions.is_empty() {
        let empty = gtk::Label::builder()
            .label("Nothing for today - pin a task from any list, or add one due today.")
            .css_classes(["dim-label", "caption"])
            .xalign(0.0)
            .wrap(true)
            .margin_top(16)
            .build();
        crate::privacy::mark_ui(&empty);
        view.list.append(&empty);
        return;
    }

    if !pinned.is_empty() {
        view.list.append(&section_header("My Day"));
        for task in &pinned {
            let row = task_row(task, colors, toggle.clone(), activate.clone(), &[], true, &categories);
            attach_my_day_menu(&row, task, &view.my_day_pins, &view.my_day_toggle);
            view.list.append(&row);
        }
    }
    if !suggestions.is_empty() {
        view.list.append(&section_header("Suggestions"));
        for task in suggestions {
            let row = task_row(task, colors, toggle.clone(), activate.clone(), &[], true, &categories);
            attach_my_day_menu(&row, task, &view.my_day_pins, &view.my_day_toggle);
            view.list.append(&row);
        }
    }
}

/// A caption heading for a My Day section.
fn section_header(label: &str) -> gtk::Label {
    let label = gtk::Label::builder()
        .label(label)
        .css_classes(["dim-label", "caption-heading"])
        .xalign(0.0)
        .hexpand(true)
        .margin_top(8)
        .margin_bottom(2)
        .build();
    crate::privacy::mark_ui(&label);
    label
}

/// Attaches the right-click "Add to My Day" / "Remove from My Day" action to a
/// task row (item 4.9 / T-02). A no-op until the caller installs a toggle
/// handler via [`set_my_day_state`] - so the dashboard's and Mail overview's
/// shared rows, which never do, are unaffected.
fn attach_my_day_menu(row: &gtk::Button, task: &CalendarTask, pins: &Rc<RefCell<Vec<crate::my_day::MyDayKey>>>, toggle: &Rc<RefCell<Option<MyDayToggleHandler>>>) {
    if toggle.borrow().is_none() {
        return;
    }
    let gesture = gtk::GestureClick::new();
    gesture.set_button(3);
    let row_for_gesture = row.clone();
    let task = task.clone();
    let pins = pins.clone();
    let toggle = toggle.clone();
    gesture.connect_pressed(move |_gesture, _n_press, x, y| {
        let Some(handler) = toggle.borrow().clone() else { return };
        let key = crate::my_day::key_for(&task);
        let pinned = crate::my_day::is_pinned(&pins.borrow(), &key);
        let popover = gtk::Popover::new();
        popover.set_parent(&row_for_gesture);
        popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        popover.set_has_arrow(false);
        let menu = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).build();
        let item = gtk::Button::with_label(if pinned { "Remove from My Day" } else { "Add to My Day" });
        item.add_css_class("flat");
        item.set_halign(gtk::Align::Start);
        {
            let popover = popover.clone();
            let key = key.clone();
            item.connect_clicked(move |_| {
                popover.popdown();
                handler(key.clone());
            });
        }
        menu.append(&item);
        popover.set_child(Some(&menu));
        // Unparent on close so repeatedly opening the menu doesn't parent a
        // fresh popover to the row forever.
        popover.connect_closed(|popover| popover.unparent());
        popover.popup();
    });
    row.add_controller(gesture);
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
/// reuses the same row. `categories` is the shared master list used to colour
/// the row's category chips (T-08); pass an empty slice where chips aren't
/// wanted (the dashboard and Mail overview rows).
pub(crate) fn task_row(
    task: &CalendarTask,
    colors: &CalendarColorMap,
    toggle: ToggleHandler,
    activate: ActivateHandler,
    summary_classes: &[&str],
    show_check: bool,
    categories: &[CategoryDef],
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
        let (r, g, b) = crate::calendar_colors::rgb(&color);
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
    if task.rrule.is_some() {
        let repeat = gtk::Image::from_icon_name("media-playlist-repeat-symbolic");
        repeat.set_tooltip_text(Some("Repeating task"));
        repeat.add_css_class("dim-label");
        row_box.append(&repeat);
    }
    if task.reminder_minutes_before.is_some() {
        let bell = gtk::Image::from_icon_name("alarm-symbolic");
        bell.set_tooltip_text(Some("Reminder set"));
        bell.add_css_class("dim-label");
        row_box.append(&bell);
    }
    if let Some(chips) = category_chips(&task, categories) {
        row_box.append(&chips);
    }
    row_box.append(&due);

    if completed {
        summary.add_css_class("task-summary-done");
    }

    let button = gtk::Button::builder().child(&row_box).css_classes(["flat"]).hexpand(true).build();
    // Task text is content: blurred by privacy mode.
    crate::privacy::mark_content(&button);
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
            toggle(task.clone(), updated, !task_completed(&task));
        });
    }
    button.connect_clicked(move |_| activate(task.clone()));
    button
}

fn task_completed(task: &CalendarTask) -> bool {
    task.status == TaskStatus::Completed || task.percent_complete == Some(100)
}

/// The row's category chips (T-08): a small colour dot (the master list's
/// preset colour, grey when unmatched) plus the name, up to three so a
/// heavily-tagged task can't crowd out the due date. `None` when the task has
/// no categories or the caller passed no master list.
fn category_chips(task: &CalendarTask, categories: &[CategoryDef]) -> Option<gtk::Box> {
    if task.categories.is_empty() {
        return None;
    }
    let boxed = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).valign(gtk::Align::Center).build();
    for (shown, name) in task.categories.iter().enumerate() {
        if shown >= 3 {
            break;
        }
        let color = categories
            .iter()
            .find(|def| def.name.eq_ignore_ascii_case(name))
            .map(|def| def.color())
            .unwrap_or("#808080")
            .to_string();
        let dot = gtk::DrawingArea::builder().width_request(8).height_request(8).valign(gtk::Align::Center).build();
        dot.set_draw_func(move |_, cr, width, height| {
            let (r, g, b) = crate::calendar_colors::rgb(&color);
            let radius = width.min(height) as f64 / 2.0 - 0.5;
            cr.arc(width as f64 / 2.0, height as f64 / 2.0, radius.max(1.0), 0.0, 2.0 * std::f64::consts::PI);
            cr.set_source_rgba(r, g, b, 1.0);
            let _ = cr.fill();
        });
        let label = gtk::Label::builder().label(name).css_classes(["dim-label", "caption"]).build();
        let chip = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(3).build();
        chip.append(&dot);
        chip.append(&label);
        chip.set_tooltip_text(Some(name));
        boxed.append(&chip);
    }
    Some(boxed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use lookout_core::TaskPriority;

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
            rrule: None,
            created: None,
            completed: None,
            status,
            priority: TaskPriority(priority),
            percent_complete: percent,
            reminder_minutes_before: None,
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

    fn titles(tasks: &[CalendarTask]) -> Vec<&str> {
        tasks.iter().map(|t| t.summary.as_deref().unwrap()).collect()
    }

    #[test]
    fn sort_tasks_orders_by_each_key() {
        let at = |day: u32| NaiveDateTime::new(NaiveDate::from_ymd_opt(2026, 8, day).unwrap(), chrono::NaiveTime::from_hms_opt(9, 0, 0).unwrap());
        let mk = |summary: &str, due: Option<NaiveDateTime>, priority: u8, created_day: u32| {
            let mut t = task(summary, due, TaskStatus::NeedsAction, None, priority);
            t.created = Some(chrono::Utc.with_ymd_and_hms(2026, 8, created_day, 9, 0, 0).unwrap());
            t
        };
        let base = vec![mk("zzz", Some(at(11)), 9, 3), mk("aaa", Some(at(13)), 1, 1), mk("mmm", None, 5, 2)];

        let mut due = base.clone();
        sort_tasks(&mut due, TaskSortKey::Due);
        assert_eq!(titles(&due), ["zzz", "aaa", "mmm"], "soonest first, undated last");

        let mut importance = base.clone();
        sort_tasks(&mut importance, TaskSortKey::Importance);
        assert_eq!(titles(&importance), ["aaa", "mmm", "zzz"], "priority 1 first, 9 last");

        let mut title = base.clone();
        sort_tasks(&mut title, TaskSortKey::Title);
        assert_eq!(titles(&title), ["aaa", "mmm", "zzz"], "alphabetical");

        let mut created = base.clone();
        sort_tasks(&mut created, TaskSortKey::Created);
        assert_eq!(titles(&created), ["zzz", "mmm", "aaa"], "newest created first");
    }

    #[test]
    fn arrange_tasks_groups_by_list_category_and_none() {
        let now = NaiveDateTime::new(NaiveDate::from_ymd_opt(2026, 8, 10).unwrap(), chrono::NaiveTime::from_hms_opt(12, 0, 0).unwrap());
        let list_a = CalendarId("list-a".to_string());
        let list_b = CalendarId("list-b".to_string());
        let mut one = task("one", None, TaskStatus::NeedsAction, None, 5);
        one.calendar_id = list_a.clone();
        one.categories = vec!["Work".to_string()];
        let mut two = task("two", None, TaskStatus::NeedsAction, None, 5);
        two.calendar_id = list_b.clone();
        two.categories = vec!["Home".to_string(), "Work".to_string()];
        let three = task("three", None, TaskStatus::NeedsAction, None, 5);
        let tasks = vec![one, two, three];
        let names: HashMap<CalendarId, String> = [(list_a, "Alpha".to_string()), (list_b, "Beta".to_string())].into_iter().collect();

        let by_list = arrange_tasks(&tasks, TaskSortKey::Title, TaskGroupBy::List, now, &names);
        let labels: Vec<&str> = by_list.iter().map(|(label, _)| label.as_str()).collect();
        assert_eq!(labels, vec!["Alpha", "Beta", "test:cal"], "named lists sort; the unmapped one falls back to its id");

        let by_category = arrange_tasks(&tasks, TaskSortKey::Title, TaskGroupBy::Category, now, &names);
        let labels: Vec<&str> = by_category.iter().map(|(label, _)| label.as_str()).collect();
        assert_eq!(labels, vec!["Home", "Work", "No category"], "\"No category\" is last");
        // "two" carries both Work and Home, so it appears under each.
        let work = by_category.iter().find(|(label, _)| label == "Work").unwrap();
        assert_eq!(titles(&work.1), ["one", "two"]);

        let flat = arrange_tasks(&tasks, TaskSortKey::Title, TaskGroupBy::None, now, &names);
        assert_eq!(flat.len(), 1);
        assert!(flat[0].0.is_empty(), "ungrouped has no header label");
        assert_eq!(titles(&flat[0].1), ["one", "three", "two"]);
    }

    #[test]
    fn arrange_tasks_due_keeps_bucket_order_and_applies_sort() {
        let now = NaiveDateTime::new(NaiveDate::from_ymd_opt(2026, 8, 10).unwrap(), chrono::NaiveTime::from_hms_opt(12, 0, 0).unwrap());
        let at = |day: u32| NaiveDateTime::new(NaiveDate::from_ymd_opt(2026, 8, day).unwrap(), chrono::NaiveTime::from_hms_opt(9, 0, 0).unwrap());
        let tasks = vec![
            task("later", Some(at(25)), TaskStatus::NeedsAction, None, 5),
            task("overdue", Some(at(2)), TaskStatus::NeedsAction, None, 5),
            task("today", Some(at(10)), TaskStatus::NeedsAction, None, 5),
        ];
        let groups = arrange_tasks(&tasks, TaskSortKey::Due, TaskGroupBy::Due, now, &HashMap::new());
        let labels: Vec<&str> = groups.iter().map(|(label, _)| label.as_str()).collect();
        assert_eq!(labels, vec!["Overdue", "Today", "Later"]);
    }

    fn flagged_message(uid: u32, flag: lookout_core::FollowUpFlag) -> EmailSummary {
        EmailSummary {
            inference_classification: Default::default(),
            uid: lookout_core::Uid(uid),
            mailbox: lookout_core::MailboxId("a:INBOX".into()),
            message_id: None,
            in_reply_to: None,
            references: Vec::new(),
            thread_key: lookout_core::ThreadKey(String::new()),
            subject: Some(format!("m{uid}")),
            from: Vec::new(),
            to: Vec::new(),
            cc: Vec::new(),
            date: chrono::Utc.with_ymd_and_hms(2026, 10, 1, 9, 0, 0).unwrap(),
            flags: Default::default(),
            keywords: lookout_core::follow_up_keywords(&flag).into_iter().collect(),
            size: 0,
            has_attachment: false,
            has_calendar: false,
            preview: None,
            mentions: false,
            importance: Default::default(),
            sensitivity: Default::default(),
            list_id: None,
            structure: None,
        }
    }

    #[test]
    fn flagged_mail_lists_open_flags_most_urgent_first() {
        let today = NaiveDate::from_ymd_opt(2026, 10, 13).unwrap();
        let day = |d: u32| Some(NaiveDate::from_ymd_opt(2026, 10, d).unwrap());
        let messages = vec![
            flagged_message(1, crate::follow_up::flagged(None)),
            flagged_message(2, crate::follow_up::completed()),
            flagged_message(3, crate::follow_up::flagged(day(20))),
            flagged_message(4, crate::follow_up::flagged(day(10))),
            flagged_message(5, crate::follow_up::flagged(day(13))),
        ];
        let order: Vec<u32> = flagged_mail_rows(&messages, today, Weekday::Mon).iter().map(|m| m.uid.0).collect();
        assert_eq!(order, vec![4, 5, 3, 1], "overdue, today, next week, undated; the completed one is left out");
    }
}
