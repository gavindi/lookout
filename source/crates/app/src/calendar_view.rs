use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use chrono::{Datelike, NaiveDate, Timelike};
use gtk::prelude::*;
use lookout_core::{CalendarId, CalendarInfo, EventOccurrence};
use lookout_dav::session::ConnectionState as CalendarConnectionState;

/// Sunday-first week (matches Outlook's/the US default convention, per the
/// reference screenshot this view is matched against) rather than
/// locale-detected - an explicit simplification for this pass.
const WEEKDAY_LABELS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const WEEK_DAYS: [chrono::Weekday; 7] = [
    chrono::Weekday::Sun,
    chrono::Weekday::Mon,
    chrono::Weekday::Tue,
    chrono::Weekday::Wed,
    chrono::Weekday::Thu,
    chrono::Weekday::Fri,
    chrono::Weekday::Sat,
];
const WORK_WEEK_DAYS: [chrono::Weekday; 5] = [chrono::Weekday::Mon, chrono::Weekday::Tue, chrono::Weekday::Wed, chrono::Weekday::Thu, chrono::Weekday::Fri];
const MAX_VISIBLE_EVENTS_PER_DAY: usize = 3;
const MAX_VISIBLE_EVENTS_PER_HOUR: usize = 3;
const HOURS_PER_DAY: usize = 24;
/// Width of the hour-gutter column in the Day/Week grids, wide enough for
/// "00:00" without pushing the event columns off-balance.
const HOUR_GUTTER_WIDTH: i32 = 56;

struct DayCell {
    container: gtk::Box,
    date_label: gtk::Label,
    events_box: gtk::Box,
}

/// Flat hairline grids (`.calendar-day-cell`/`.calendar-hour-cell`) instead of
/// libadwaita's rounded `.card` panels, and a bordered highlight for today
/// (`.calendar-today-cell`) - matches the Outlook reference this view is
/// styled against. Registered once (from [`build_main`]) on the default
/// display, same pattern as `window.rs`'s `install_paned_css()`.
fn install_calendar_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(
        ".calendar-day-cell {
            border: 1px solid alpha(currentColor, 0.08);
        }
        .calendar-today-cell {
            border: 2px solid @accent_bg_color;
        }
        .calendar-hour-cell {
            border-bottom: 1px solid alpha(currentColor, 0.06);
        }
        .calendar-main-background {
            background-color: #2e2e32;
            border-radius: 12px;
        }",
    );
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(&display, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    }
}

/// A single 6-week, Sunday-first month grid. Deliberately dumb and read-only:
/// no drag/resize/creation, no per-event click handling, and no header row -
/// the header (label + Today/prev/next) is owned by [`CalendarMain`], which
/// shares it across every view. Kept as plain data-in/widget-state-out
/// functions (`set_month`/`set_month_occurrences`), mirroring
/// `folder_tree.rs`'s `build_multi_account_tree_model` precedent, so the
/// date-bucketing logic stays testable independent of a running GTK main loop.
pub struct MonthGrid {
    pub root: gtk::Widget,
    day_cells: Vec<DayCell>,
    anchor_month: Rc<RefCell<NaiveDate>>,
}

fn build_month_grid() -> MonthGrid {
    let grid = gtk::Grid::builder()
        .row_homogeneous(true)
        .column_homogeneous(true)
        .row_spacing(1)
        .column_spacing(1)
        .vexpand(true)
        .hexpand(true)
        .build();

    for (col, label) in WEEKDAY_LABELS.iter().enumerate() {
        let weekday_label = gtk::Label::builder().label(*label).css_classes(["dim-label", "caption-heading"]).build();
        grid.attach(&weekday_label, col as i32, 0, 1, 1);
    }

    let mut day_cells = Vec::with_capacity(42);
    for row in 0..6 {
        for col in 0..7 {
            let date_label = gtk::Label::builder().xalign(0.0).margin_start(4).margin_top(2).css_classes(["caption"]).build();
            let events_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(1).vexpand(true).build();
            let container = gtk::Box::builder().orientation(gtk::Orientation::Vertical).css_classes(["calendar-day-cell"]).build();
            container.append(&date_label);
            container.append(&events_box);
            grid.attach(&container, col, row + 1, 1, 1);
            day_cells.push(DayCell {
                container,
                date_label,
                events_box,
            });
        }
    }

    let root_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).vexpand(true).hexpand(true).build();
    root_box.append(&grid);

    MonthGrid {
        root: root_box.upcast(),
        day_cells,
        anchor_month: Rc::new(RefCell::new(first_of_month(chrono::Utc::now().date_naive()))),
    }
}

/// Rebuilds the grid's date labels/highlighting for the month containing
/// `month` and clears every cell's event list (a subsequent
/// `set_month_occurrences` call repopulates them).
pub fn set_month(mg: &MonthGrid, month: NaiveDate) {
    let month = first_of_month(month);
    *mg.anchor_month.borrow_mut() = month;

    let today = chrono::Utc::now().date_naive();
    let grid_start = first_grid_day(month);

    for (i, cell) in mg.day_cells.iter().enumerate() {
        let date = grid_start + chrono::Duration::days(i as i64);
        cell.date_label.set_label(&date.day().to_string());
        clear_children(&cell.events_box);

        if date.month() == month.month() {
            cell.container.remove_css_class("dim-label");
        } else {
            cell.container.add_css_class("dim-label");
        }
        if date == today {
            cell.date_label.add_css_class("accent");
            cell.date_label.add_css_class("heading");
            cell.container.add_css_class("calendar-today-cell");
        } else {
            cell.date_label.remove_css_class("accent");
            cell.date_label.remove_css_class("heading");
            cell.container.remove_css_class("calendar-today-cell");
        }
    }
}

/// Buckets `occurrences` by local calendar date and fills each visible day
/// cell's event list, capped at [`MAX_VISIBLE_EVENTS_PER_DAY`] with a
/// "+N more" label for the rest (no popover - kept simple for this pass).
/// Occurrences for dates outside the grid's currently-displayed 6-week span
/// are silently ignored.
pub fn set_month_occurrences(mg: &MonthGrid, occurrences: &[EventOccurrence]) {
    let grid_start = first_grid_day(*mg.anchor_month.borrow());

    for cell in &mg.day_cells {
        clear_children(&cell.events_box);
    }

    let mut by_date: HashMap<NaiveDate, Vec<&EventOccurrence>> = HashMap::new();
    for occ in occurrences {
        let date = occ.start.with_timezone(&chrono::Local).date_naive();
        by_date.entry(date).or_default().push(occ);
    }

    for (i, cell) in mg.day_cells.iter().enumerate() {
        let date = grid_start + chrono::Duration::days(i as i64);
        let Some(day_occurrences) = by_date.get(&date) else { continue };
        for occ in &day_occurrences[..day_occurrences.len().min(MAX_VISIBLE_EVENTS_PER_DAY)] {
            cell.events_box.append(&event_label(occ));
        }
        if day_occurrences.len() > MAX_VISIBLE_EVENTS_PER_DAY {
            cell.events_box.append(&more_label(day_occurrences.len() - MAX_VISIBLE_EVENTS_PER_DAY));
        }
    }
}

/// One day column of a [`WeekGrid`]: a header showing the weekday/date, an
/// all-day row, and one events cell per hour of the day.
struct WeekDayColumn {
    header: gtk::Label,
    all_day: gtk::Box,
    hours: Vec<gtk::Box>,
}

/// A read-only time-grid view of a single week: a "All day" row plus 24 hour
/// rows per day column, with a shared hour-ruler gutter on the left. Two
/// instances are built - one Sunday-first ("Week") and one Monday-first with
/// five columns ("Work week") - sharing this one widget.
pub struct WeekGrid {
    pub root: gtk::Widget,
    columns: Vec<WeekDayColumn>,
    anchor: Rc<RefCell<NaiveDate>>,
    weekdays: Vec<chrono::Weekday>,
}

fn build_week_grid(weekdays: &[chrono::Weekday]) -> WeekGrid {
    let grid = gtk::Grid::builder()
        .column_homogeneous(false)
        .row_homogeneous(true)
        .row_spacing(1)
        .column_spacing(1)
        .vexpand(true)
        .hexpand(true)
        .build();

    grid.attach(&gtk::Label::builder().width_request(HOUR_GUTTER_WIDTH).build(), 0, 0, 1, 1);

    let mut columns = Vec::with_capacity(weekdays.len());
    for (i, _) in weekdays.iter().enumerate() {
        let col = i as i32 + 1;
        let header = gtk::Label::builder().css_classes(["dim-label", "caption-heading"]).build();
        grid.attach(&header, col, 0, 1, 1);

        let all_day = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(1).hexpand(true).build();
        grid.attach(&all_day, col, 1, 1, 1);

        let mut hours = Vec::with_capacity(HOURS_PER_DAY);
        for h in 0..HOURS_PER_DAY {
            let cell = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .css_classes(["calendar-hour-cell"])
                .hexpand(true)
                .spacing(1)
                .build();
            grid.attach(&cell, col, h as i32 + 2, 1, 1);
            hours.push(cell);
        }
        columns.push(WeekDayColumn { header, all_day, hours });
    }

    let all_day_gutter = gtk::Label::builder()
        .label("All day")
        .css_classes(["dim-label", "caption"])
        .halign(gtk::Align::End)
        .width_request(HOUR_GUTTER_WIDTH)
        .margin_end(4)
        .build();
    grid.attach(&all_day_gutter, 0, 1, 1, 1);
    for h in 0..HOURS_PER_DAY {
        let hour_label = gtk::Label::builder()
            .label(format!("{h:02}:00"))
            .css_classes(["dim-label", "caption"])
            .halign(gtk::Align::End)
            .width_request(HOUR_GUTTER_WIDTH)
            .margin_end(4)
            .build();
        grid.attach(&hour_label, 0, h as i32 + 2, 1, 1);
    }

    let root_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).vexpand(true).hexpand(true).build();
    root_box.append(&grid);

    WeekGrid {
        root: root_box.upcast(),
        columns,
        anchor: Rc::new(RefCell::new(chrono::Utc::now().date_naive())),
        weekdays: weekdays.to_vec(),
    }
}

/// Relabels the column headers for the week containing `anchor` (resolved to
/// the week whose first day matches this grid's `weekdays[0]`) and clears
/// every column's event cells for a subsequent `set_week_occurrences`.
pub fn set_week(w: &WeekGrid, anchor: NaiveDate) {
    *w.anchor.borrow_mut() = anchor;
    let start = week_start(anchor, w.weekdays[0]);
    let today = chrono::Utc::now().date_naive();

    for (i, col) in w.columns.iter().enumerate() {
        let date = start + chrono::Duration::days(i as i64);
        let weekday = WEEKDAY_LABELS[date.weekday().num_days_from_sunday() as usize];
        col.header.set_label(&format!("{weekday} {}", date.day()));
        clear_children(&col.all_day);
        for cell in &col.hours {
            clear_children(cell);
        }
        if date == today {
            col.header.add_css_class("accent");
        } else {
            col.header.remove_css_class("accent");
        }
    }
}

/// Buckets `occurrences` by local calendar date/hour into the displayed week's
/// columns: all-day events into the "All day" row, timed events into the hour
/// cell matching their local start hour (capped per hour with a "+N more").
/// Occurrences outside the displayed week are ignored.
pub fn set_week_occurrences(w: &WeekGrid, occurrences: &[EventOccurrence]) {
    let anchor = *w.anchor.borrow();
    let start = week_start(anchor, w.weekdays[0]);
    let end = start + chrono::Duration::days(w.columns.len() as i64);

    let mut by_date: HashMap<NaiveDate, Vec<&EventOccurrence>> = HashMap::new();
    for occ in occurrences {
        let date = occ.start.with_timezone(&chrono::Local).date_naive();
        if date >= start && date < end {
            by_date.entry(date).or_default().push(occ);
        }
    }

    for (i, col) in w.columns.iter().enumerate() {
        let date = start + chrono::Duration::days(i as i64);
        let Some(list) = by_date.get(&date) else { continue };
        for occ in list {
            if occ.all_day {
                append_event(&col.all_day, occ, MAX_VISIBLE_EVENTS_PER_HOUR);
            } else {
                let hour = occ.start.with_timezone(&chrono::Local).hour() as usize;
                if hour < col.hours.len() {
                    append_event(&col.hours[hour], occ, MAX_VISIBLE_EVENTS_PER_HOUR);
                }
            }
        }
    }
}

/// A single-day time grid: an all-day row plus one cell per hour, against the
/// same hour-ruler gutter the week grid uses. Header/date comes from
/// [`CalendarMain`]'s shared header.
pub struct DayView {
    pub root: gtk::Widget,
    anchor: Rc<RefCell<NaiveDate>>,
    all_day: gtk::Box,
    hours: Vec<gtk::Box>,
}

fn build_day_view() -> DayView {
    let grid = gtk::Grid::builder()
        .column_homogeneous(false)
        .row_homogeneous(true)
        .row_spacing(1)
        .column_spacing(1)
        .vexpand(true)
        .hexpand(true)
        .build();

    let all_day = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(1).hexpand(true).build();
    let all_day_gutter = gtk::Label::builder()
        .label("All day")
        .css_classes(["dim-label", "caption"])
        .halign(gtk::Align::End)
        .width_request(HOUR_GUTTER_WIDTH)
        .margin_end(4)
        .build();
    grid.attach(&all_day_gutter, 0, 0, 1, 1);
    grid.attach(&all_day, 1, 0, 1, 1);

    let mut hours = Vec::with_capacity(HOURS_PER_DAY);
    for h in 0..HOURS_PER_DAY {
        let hour_label = gtk::Label::builder()
            .label(format!("{h:02}:00"))
            .css_classes(["dim-label", "caption"])
            .halign(gtk::Align::End)
            .width_request(HOUR_GUTTER_WIDTH)
            .margin_end(4)
            .build();
        let cell = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .css_classes(["calendar-hour-cell"])
            .hexpand(true)
            .spacing(1)
            .build();
        grid.attach(&hour_label, 0, h as i32 + 1, 1, 1);
        grid.attach(&cell, 1, h as i32 + 1, 1, 1);
        hours.push(cell);
    }

    let root_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).vexpand(true).hexpand(true).build();
    root_box.append(&grid);

    DayView {
        root: root_box.upcast(),
        anchor: Rc::new(RefCell::new(chrono::Utc::now().date_naive())),
        all_day,
        hours,
    }
}

/// Re-points the day view at `day` and clears its cells for a subsequent
/// `set_day_occurrences`.
pub fn set_day(d: &DayView, day: NaiveDate) {
    *d.anchor.borrow_mut() = day;
    clear_children(&d.all_day);
    for cell in &d.hours {
        clear_children(cell);
    }
}

/// Fills the day view with every occurrence whose local date matches the
/// displayed day: all-day events on top, timed events in their local start
/// hour's cell (capped per hour).
pub fn set_day_occurrences(d: &DayView, occurrences: &[EventOccurrence]) {
    let day = *d.anchor.borrow();
    for occ in occurrences {
        let date = occ.start.with_timezone(&chrono::Local).date_naive();
        if date != day {
            continue;
        }
        if occ.all_day {
            append_event(&d.all_day, occ, MAX_VISIBLE_EVENTS_PER_HOUR);
        } else {
            let hour = occ.start.with_timezone(&chrono::Local).hour() as usize;
            if hour < d.hours.len() {
                append_event(&d.hours[hour], occ, MAX_VISIBLE_EVENTS_PER_HOUR);
            }
        }
    }
}

/// A chronological (agenda-style) list of the displayed day's month, from the
/// anchor date forward to the end of that month, sorted by start time - the
/// same shape as the Mail-screen overview pane's day list, but scoped to the
/// whole remaining month instead of a single day. Used both as its own view
/// and as the right-hand pane of the Split view.
pub struct AgendaView {
    pub root: gtk::Widget,
    events_box: gtk::Box,
    anchor: Rc<RefCell<NaiveDate>>,
}

fn build_agenda_view() -> AgendaView {
    let events_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).build();
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .vexpand(true)
        .hexpand(true)
        .child(&events_box)
        .build();

    let root_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).vexpand(true).hexpand(true).build();
    root_box.append(&scroller);

    AgendaView {
        root: root_box.upcast(),
        events_box,
        anchor: Rc::new(RefCell::new(chrono::Utc::now().date_naive())),
    }
}

/// Rebuilds the agenda's rows for `anchor` (its own month forward). One row
/// per occurrence: a date column plus "HH:MM – HH:MM summary" (or "All day
/// summary") in the summary column.
pub fn set_agenda(a: &AgendaView, anchor: NaiveDate, occurrences: &[EventOccurrence]) {
    *a.anchor.borrow_mut() = anchor;
    clear_children(&a.events_box);

    let month_end = last_of_month(anchor);
    let mut upcoming: Vec<&EventOccurrence> = occurrences
        .iter()
        .filter(|occ| {
            let date = occ.start.with_timezone(&chrono::Local).date_naive();
            date >= anchor && date <= month_end
        })
        .collect();
    upcoming.sort_by_key(|occ| occ.start);

    if upcoming.is_empty() {
        let placeholder = gtk::Label::builder().label("No events").css_classes(["dim-label", "caption"]).xalign(0.0).build();
        a.events_box.append(&placeholder);
        return;
    }

    for occ in upcoming {
        let row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).build();
        let date = occ.start.with_timezone(&chrono::Local).date_naive();
        let date_label = gtk::Label::builder()
            .label(date.format("%a %d %b").to_string())
            .css_classes(["dim-label", "caption"])
            .width_request(96)
            .xalign(0.0)
            .build();

        let text = if occ.all_day {
            occ.summary.clone().unwrap_or_else(|| "(untitled)".to_string())
        } else {
            let start = occ.start.with_timezone(&chrono::Local).format("%H:%M");
            let end = occ.end.with_timezone(&chrono::Local).format("%H:%M");
            format!("{start} – {end}  {}", occ.summary.as_deref().unwrap_or("(untitled)"))
        };
        let summary_label = gtk::Label::builder()
            .label(&text)
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["caption"])
            .hexpand(true)
            .build();

        row.append(&date_label);
        row.append(&summary_label);
        a.events_box.append(&row);
    }
}

/// The Split view: the month grid on the left with a day-agenda on the right,
/// both anchored to the same date. Read-only, like everything else here.
pub struct SplitView {
    pub root: gtk::Widget,
    pub month: MonthGrid,
    pub agenda: AgendaView,
}

fn build_split_view() -> SplitView {
    let month = build_month_grid();
    month.root.set_width_request(300);
    let agenda = build_agenda_view();

    let paned = gtk::Paned::builder()
        .orientation(gtk::Orientation::Horizontal)
        .start_child(&month.root)
        .end_child(&agenda.root)
        .resize_start_child(false)
        .resize_end_child(true)
        .position(300)
        .build();

    let root_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).vexpand(true).hexpand(true).build();
    root_box.append(&paned);

    SplitView {
        root: root_box.upcast(),
        month,
        agenda,
    }
}

/// The calendar view's main panel: a shared header row (title + Today +
/// prev/next) above a stack of the five views - Month, Work week, Week, Day,
/// and Split - all anchored to a single date so they always agree on what's
/// displayed. `window.rs` drives it the same way it drove the old standalone
/// [`MonthGrid`]: `set_anchor`/`set_occurrences` in, read the anchor back
/// with `anchor()` to decide which month to resync.
pub struct CalendarMain {
    pub root: gtk::Widget,
    pub header_label: gtk::Label,
    pub prev_button: gtk::Button,
    pub next_button: gtk::Button,
    pub today_button: gtk::Button,
    stack: gtk::Stack,
    month: MonthGrid,
    workweek: WeekGrid,
    week: WeekGrid,
    day: DayView,
    agenda: AgendaView,
    split: SplitView,
    anchor: Rc<RefCell<NaiveDate>>,
    occurrences: Rc<RefCell<Vec<EventOccurrence>>>,
}

pub fn build_main() -> CalendarMain {
    install_calendar_css();

    let header_label = gtk::Label::builder().css_classes(["title-2"]).hexpand(true).xalign(0.0).build();
    let prev_button = gtk::Button::from_icon_name("go-previous-symbolic");
    let next_button = gtk::Button::from_icon_name("go-next-symbolic");
    let today_button = gtk::Button::builder().label("Today").build();

    let header_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .margin_start(6)
        .margin_end(6)
        .margin_top(6)
        .margin_bottom(6)
        .build();
    header_row.append(&header_label);
    header_row.append(&today_button);
    header_row.append(&prev_button);
    header_row.append(&next_button);

    let month = build_month_grid();
    let workweek = build_week_grid(&WORK_WEEK_DAYS);
    let week = build_week_grid(&WEEK_DAYS);
    let day = build_day_view();
    let agenda = build_agenda_view();
    let split = build_split_view();

    let stack = gtk::Stack::new();
    stack.add_named(&workweek.root, Some("workweek"));
    stack.add_named(&week.root, Some("week"));
    stack.add_named(&day.root, Some("day"));
    stack.add_named(&split.root, Some("split"));
    stack.add_named(&month.root, Some("month"));
    stack.set_visible_child_name("month");

    let root_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .css_classes(["calendar-main-background"])
        .overflow(gtk::Overflow::Hidden)
        // Matches `card_section()`'s own 6px margin (used by the sidebar to
        // its left), so both panels sit the same distance from the paned
        // divider/window edges.
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(6)
        .margin_end(6)
        .vexpand(true)
        .hexpand(true)
        .build();
    root_box.append(&header_row);
    root_box.append(&stack);

    let anchor = Rc::new(RefCell::new(chrono::Utc::now().date_naive()));
    let calendar_main = CalendarMain {
        root: root_box.upcast(),
        header_label,
        prev_button,
        next_button,
        today_button,
        stack,
        month,
        workweek,
        week,
        day,
        agenda,
        split,
        anchor: anchor.clone(),
        occurrences: Rc::new(RefCell::new(Vec::new())),
    };
    // Extracted into a local first (rather than inlined into the call
    // below) so the `Ref` temporary from `.borrow()` is dropped before
    // `set_anchor` runs - otherwise it would still be alive (temporaries
    // live until the end of the enclosing statement) when `set_anchor`
    // borrows the same cell again via `refresh`, panicking with
    // "already borrowed".
    let initial_anchor = *anchor.borrow();
    set_anchor(&calendar_main, initial_anchor);
    calendar_main
}

/// Switches the main panel to the named view (`"day"`, `"workweek"`, `"week"`,
/// `"month"`, or `"split"`) and re-renders the shared header title for it.
pub fn set_view(c: &CalendarMain, view: &'static str) {
    if c.stack.child_by_name(view).is_some() {
        c.stack.set_visible_child_name(view);
        refresh(c);
    }
}

/// The name of the currently visible view (see [`set_view`]).
pub fn active_view(c: &CalendarMain) -> &'static str {
    match c.stack.visible_child_name().as_deref() {
        Some("day") => "day",
        Some("workweek") => "workweek",
        Some("week") => "week",
        Some("split") => "split",
        _ => "month",
    }
}

/// Re-points every view at `day` (the month/week/day/agenda windows are all
/// derived from it) and redraws them with the currently cached occurrences.
/// The caller reads the anchor back with [`anchor`] to decide what to resync.
pub fn set_anchor(c: &CalendarMain, day: NaiveDate) {
    *c.anchor.borrow_mut() = day;
    refresh(c);
}

/// The date every view is currently anchored to.
pub fn anchor(c: &CalendarMain) -> NaiveDate {
    *c.anchor.borrow()
}

/// Caches the caller's merged occurrences and re-renders every view.
pub fn set_occurrences(c: &CalendarMain, occurrences: &[EventOccurrence]) {
    *c.occurrences.borrow_mut() = occurrences.to_vec();
    refresh(c);
}

/// Moves the anchor by `by` steps in the active view's natural unit: a day
/// for Day/Agenda, a week for Week/Work week, a month for Month/Split.
pub fn step(c: &CalendarMain, by: i64) {
    let anchor = *c.anchor.borrow();
    let new_anchor = match active_view(c) {
        "day" | "agenda" => anchor + chrono::Duration::days(by),
        "workweek" | "week" => anchor + chrono::Duration::days(by * 7),
        _ if by > 0 => anchor.checked_add_months(chrono::Months::new(1)).unwrap_or(anchor),
        _ => anchor.checked_sub_months(chrono::Months::new(1)).unwrap_or(anchor),
    };
    set_anchor(c, new_anchor);
}

/// Re-anchors every view on today (Month shows today's month, Week this week,
/// Day today, etc.).
pub fn go_today(c: &CalendarMain) {
    set_anchor(c, chrono::Utc::now().date_naive());
}

fn refresh(c: &CalendarMain) {
    let anchor = *c.anchor.borrow();
    let occurrences = c.occurrences.borrow();

    set_month(&c.month, anchor);
    set_month_occurrences(&c.month, &occurrences);
    set_month(&c.split.month, anchor);
    set_month_occurrences(&c.split.month, &occurrences);

    set_week(&c.workweek, anchor);
    set_week_occurrences(&c.workweek, &occurrences);
    set_week(&c.week, anchor);
    set_week_occurrences(&c.week, &occurrences);

    set_day(&c.day, anchor);
    set_day_occurrences(&c.day, &occurrences);

    set_agenda(&c.agenda, anchor, &occurrences);
    set_agenda(&c.split.agenda, anchor, &occurrences);

    c.header_label.set_label(&header_text(c));
}

fn header_text(c: &CalendarMain) -> String {
    let anchor = *c.anchor.borrow();
    match active_view(c) {
        "day" => anchor.format("%A %d %B %Y").to_string(),
        "workweek" | "week" => {
            let weekdays = if active_view(c) == "workweek" { &WORK_WEEK_DAYS[..] } else { &WEEK_DAYS[..] };
            let start = week_start(anchor, weekdays[0]);
            let end = start + chrono::Duration::days(weekdays.len() as i64 - 1);
            if start.year() == end.year() {
                format!("{} – {}", start.format("%d %b"), end.format("%d %b %Y"))
            } else {
                format!("{} – {}", start.format("%d %b %Y"), end.format("%d %b %Y"))
            }
        }
        "split" => format!("Split · {}", anchor.format("%d %b %Y")),
        _ => first_of_month(anchor).format("%B %Y").to_string(),
    }
}

/// The first date on/before `anchor` that falls on `first`'s weekday - i.e.
/// the start of the week that `anchor` belongs to, using `first` as the week's
/// first day (Sun for the Week view, Mon for Work week).
fn week_start(anchor: NaiveDate, first: chrono::Weekday) -> NaiveDate {
    let offset = (anchor.weekday().num_days_from_sunday() as i64 - first.num_days_from_sunday() as i64).rem_euclid(7);
    anchor - chrono::Duration::days(offset)
}

/// Appends `occ` to `events_box`, showing at most `cap` event labels and then
/// a single "+N more" dim label that keeps counting up.
fn append_event(events_box: &gtk::Box, occ: &EventOccurrence, cap: usize) {
    if child_count(events_box) >= cap {
        let mut last = events_box.first_child();
        while let Some(sibling) = last.as_ref().and_then(|w| w.next_sibling()) {
            last = Some(sibling);
        }
        if let Some(last_widget) = last {
            if let Some(label) = last_widget.downcast_ref::<gtk::Label>() {
                let text = label.label();
                if let Some(count) = text
                    .as_str()
                    .strip_prefix('+')
                    .and_then(|rest| rest.split(' ').next())
                    .and_then(|n| n.parse::<usize>().ok())
                {
                    label.set_label(&format!("+{} more", count + 1));
                    return;
                }
            }
        }
        events_box.append(&more_label(1));
        return;
    }
    events_box.append(&event_label(occ));
}

fn child_count(container: &gtk::Box) -> usize {
    let mut count = 0;
    let mut child = container.first_child();
    while let Some(widget) = child {
        count += 1;
        child = widget.next_sibling();
    }
    count
}

/// A single event's compact label: "HH:MM Summary", or just "Summary" for an
/// all-day event.
fn event_label(occ: &EventOccurrence) -> gtk::Label {
    let text = if occ.all_day {
        occ.summary.clone().unwrap_or_else(|| "(untitled)".to_string())
    } else {
        let time = occ.start.with_timezone(&chrono::Local).format("%H:%M");
        format!("{time} {}", occ.summary.as_deref().unwrap_or("(untitled)"))
    };
    gtk::Label::builder()
        .label(&text)
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .css_classes(["caption"])
        .build()
}

fn more_label(count: usize) -> gtk::Label {
    gtk::Label::builder()
        .label(format!("+{count} more"))
        .xalign(0.0)
        .css_classes(["dim-label", "caption"])
        .build()
}

/// A second, smaller, event-less calendar grid used as a free-standing date
/// picker in the calendar sidebar - reuses the same `first_grid_day`/
/// `first_of_month` helpers as [`MonthGrid`] (both Sunday-first) so a date
/// clicked here always resolves to the same month boundaries the main grid
/// would show for it. Its own prev/next buttons only page this mini grid;
/// clicking an actual day is what asks the caller (via
/// [`connect_day_selected`]) to navigate the main view.
type DaySelectedCallbacks = Rc<RefCell<Vec<Rc<dyn Fn(NaiveDate)>>>>;

#[derive(Clone)]
pub struct MiniCalendar {
    pub root: gtk::Widget,
    header_label: gtk::Label,
    day_buttons: Vec<gtk::Button>,
    anchor_month: Rc<RefCell<NaiveDate>>,
    on_day_selected: DaySelectedCallbacks,
}

pub fn build_mini() -> MiniCalendar {
    let anchor_month = Rc::new(RefCell::new(first_of_month(chrono::Utc::now().date_naive())));
    let on_day_selected: DaySelectedCallbacks = Rc::new(RefCell::new(Vec::new()));

    let header_label = gtk::Label::builder().css_classes(["heading"]).hexpand(true).xalign(0.0).build();
    let prev_button = gtk::Button::builder().icon_name("go-previous-symbolic").css_classes(["flat"]).build();
    let next_button = gtk::Button::builder().icon_name("go-next-symbolic").css_classes(["flat"]).build();

    let header_row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(2).build();
    header_row.append(&header_label);
    header_row.append(&prev_button);
    header_row.append(&next_button);

    let grid = gtk::Grid::builder().row_homogeneous(true).column_homogeneous(true).build();
    for (col, label) in WEEKDAY_LABELS.iter().enumerate() {
        let weekday_label = gtk::Label::builder().label(*label).css_classes(["dim-label", "caption"]).build();
        grid.attach(&weekday_label, col as i32, 0, 1, 1);
    }

    let mut day_buttons = Vec::with_capacity(42);
    for row in 0..6 {
        for col in 0..7 {
            let index = (row * 7 + col) as i64;
            let button = gtk::Button::builder().css_classes(["flat"]).build();
            {
                let anchor_month = anchor_month.clone();
                let on_day_selected = on_day_selected.clone();
                button.connect_clicked(move |_| {
                    let month = *anchor_month.borrow();
                    let date = first_grid_day(month) + chrono::Duration::days(index);
                    for callback in on_day_selected.borrow().iter() {
                        callback(date);
                    }
                });
            }
            grid.attach(&button, col, row + 1, 1, 1);
            day_buttons.push(button);
        }
    }

    let root_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).build();
    root_box.append(&header_row);
    root_box.append(&grid);

    let mini = MiniCalendar {
        root: root_box.upcast(),
        header_label,
        day_buttons,
        anchor_month,
        on_day_selected,
    };

    {
        let mini_month = mini.anchor_month.clone();
        let mini_header = mini.header_label.clone();
        let mini_buttons = mini.day_buttons.clone();
        prev_button.connect_clicked(move |_| {
            let current = *mini_month.borrow();
            let new_month = current.checked_sub_months(chrono::Months::new(1)).unwrap_or(current);
            relabel_mini(&mini_month, &mini_header, &mini_buttons, new_month);
        });
    }
    {
        let mini_month = mini.anchor_month.clone();
        let mini_header = mini.header_label.clone();
        let mini_buttons = mini.day_buttons.clone();
        next_button.connect_clicked(move |_| {
            let current = *mini_month.borrow();
            let new_month = current.checked_add_months(chrono::Months::new(1)).unwrap_or(current);
            relabel_mini(&mini_month, &mini_header, &mini_buttons, new_month);
        });
    }

    let initial_month = *mini.anchor_month.borrow();
    relabel_mini(&mini.anchor_month, &mini.header_label, &mini.day_buttons, initial_month);
    mini
}

pub fn set_mini_month(mc: &MiniCalendar, month: NaiveDate) {
    relabel_mini(&mc.anchor_month, &mc.header_label, &mc.day_buttons, month);
}

fn relabel_mini(anchor_month: &Rc<RefCell<NaiveDate>>, header_label: &gtk::Label, day_buttons: &[gtk::Button], month: NaiveDate) {
    let month = first_of_month(month);
    *anchor_month.borrow_mut() = month;
    header_label.set_label(&month.format("%B %Y").to_string());

    let today = chrono::Utc::now().date_naive();
    let grid_start = first_grid_day(month);
    for (i, button) in day_buttons.iter().enumerate() {
        let date = grid_start + chrono::Duration::days(i as i64);
        button.set_label(&date.day().to_string());
        button.remove_css_class("dim-label");
        button.remove_css_class("suggested-action");
        if date.month() != month.month() {
            button.add_css_class("dim-label");
        }
        if date == today {
            button.add_css_class("suggested-action");
        }
    }
}

/// Registers `f` to run whenever a day button in `mc` is clicked. Multiple
/// callbacks can be registered (only one is used in practice today, but
/// nothing here assumes a single subscriber).
pub fn connect_day_selected(mc: &MiniCalendar, f: impl Fn(NaiveDate) + 'static) {
    mc.on_day_selected.borrow_mut().push(Rc::new(f));
}

/// One calendar account in the sidebar's "My calendars" checklist: a header
/// row showing the account's display name, its discovered calendars as the
/// checkable rows below it, and an optional status line rendered while the
/// account hasn't delivered any calendars yet (so a connected-but-silent
/// account is never just blank).
pub struct CalendarAccountGroup {
    pub display_name: String,
    pub calendars: Vec<CalendarInfo>,
    /// Short status line for the sidebar, or `None` once the account has
    /// calendars to list (see [`calendar_account_status_text`]).
    pub status: Option<String>,
}

/// Renders a calendar account's latest session state as a short sidebar
/// status line, shown under the account header until it has calendars to
/// list. `None` once calendars exist - the checkboxes speak for themselves.
pub fn calendar_account_status_text(connection_state: &CalendarConnectionState, has_calendars: bool) -> Option<String> {
    if has_calendars {
        return None;
    }
    let text = match connection_state {
        CalendarConnectionState::Connecting => "Connecting…".to_string(),
        CalendarConnectionState::Disconnected => "Disconnected".to_string(),
        CalendarConnectionState::Idle | CalendarConnectionState::Busy => "No calendars found".to_string(),
        CalendarConnectionState::Error { message, .. } => message.clone(),
    };
    Some(text)
}

/// Clears `container` and re-renders the "My calendars" checklist from the
/// given per-account groups: one account header (dim caption-heading) per
/// group, its calendars as checkbuttons (label = display name, active =
/// currently checked), or a dim status line when the account has none yet.
/// Each checkbutton's `connect_toggled` fires `on_toggle`. A plain
/// rebuildable-list function rather than a stateful struct, matching this
/// file's existing data-in/widget-state-out convention - callers own the
/// actual checked/unchecked state and just ask for a fresh render of it.
pub fn rebuild_calendar_checklist(container: &gtk::Box, groups: &[CalendarAccountGroup], checked: &HashSet<CalendarId>, on_toggle: impl Fn(CalendarId, bool) + 'static + Clone) {
    clear_children(container);
    if groups.is_empty() {
        let placeholder = gtk::Label::builder()
            .label("No calendars connected")
            .css_classes(["dim-label", "caption"])
            .xalign(0.0)
            .build();
        container.append(&placeholder);
        return;
    }
    for group in groups {
        let header = gtk::Label::builder()
            .label(&group.display_name)
            .css_classes(["dim-label", "caption-heading"])
            .xalign(0.0)
            .build();
        container.append(&header);
        if group.calendars.is_empty() {
            if let Some(status) = &group.status {
                let label = gtk::Label::builder().label(status).css_classes(["dim-label", "caption"]).xalign(0.0).wrap(true).build();
                container.append(&label);
            }
            continue;
        }
        for calendar in &group.calendars {
            let check = gtk::CheckButton::builder().label(&calendar.display_name).active(checked.contains(&calendar.id)).build();
            let id = calendar.id.clone();
            let on_toggle = on_toggle.clone();
            check.connect_toggled(move |btn| on_toggle(id.clone(), btn.is_active()));
            container.append(&check);
        }
    }
}

/// The calendar view's left sidebar: a mini month-picker, a disabled "Add
/// calendar" placeholder (no calendar-subscription support yet), and a "My
/// calendars" checklist (populated later by the caller via
/// `rebuild_calendar_checklist`, once accounts have actually reported which
/// calendars exist).
pub struct CalendarSidebar {
    pub root: gtk::Widget,
    pub mini_calendar: MiniCalendar,
    pub calendar_list_box: gtk::Box,
}

pub fn build_sidebar() -> CalendarSidebar {
    let mini_calendar = build_mini();

    let add_calendar_button = gtk::Button::builder()
        .label("Add calendar")
        .css_classes(["flat"])
        .halign(gtk::Align::Start)
        .sensitive(false)
        .build();

    let my_calendars_label = gtk::Label::builder().label("My calendars").css_classes(["heading"]).xalign(0.0).margin_top(12).build();
    let calendar_list_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).build();
    let calendar_list_scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .vexpand(true)
        .child(&calendar_list_box)
        .build();

    let root_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .margin_start(8)
        .margin_end(8)
        .margin_top(8)
        .margin_bottom(8)
        .width_request(240)
        .build();
    root_box.append(&mini_calendar.root);
    root_box.append(&add_calendar_button);
    root_box.append(&my_calendars_label);
    root_box.append(&calendar_list_scroller);

    CalendarSidebar {
        root: root_box.upcast(),
        mini_calendar,
        calendar_list_box,
    }
}

fn clear_children(container: &gtk::Box) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
}

fn first_of_month(date: NaiveDate) -> NaiveDate {
    date.with_day(1).unwrap_or(date)
}

/// The last day of `date`'s month.
fn last_of_month(date: NaiveDate) -> NaiveDate {
    let first = first_of_month(date);
    let next = first.checked_add_months(chrono::Months::new(1)).unwrap_or(first);
    next - chrono::Duration::days(1)
}

/// The Sunday that starts the grid's first row - on or before the 1st of `month`.
fn first_grid_day(month: NaiveDate) -> NaiveDate {
    let days_since_sunday = month.weekday().num_days_from_sunday() as i64;
    month - chrono::Duration::days(days_since_sunday)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_grid_day_lands_on_sunday_on_or_before_the_1st() {
        // 2026-07-01 is a Wednesday; the grid should start on Sunday 2026-06-28.
        let month = NaiveDate::from_ymd_opt(2026, 7, 1).unwrap();
        let grid_start = first_grid_day(month);
        assert_eq!(grid_start, NaiveDate::from_ymd_opt(2026, 6, 28).unwrap());
        assert_eq!(grid_start.weekday(), chrono::Weekday::Sun);
    }

    #[test]
    fn first_grid_day_is_unchanged_when_month_already_starts_on_sunday() {
        // 2026-11-01 is itself a Sunday.
        let month = NaiveDate::from_ymd_opt(2026, 11, 1).unwrap();
        assert_eq!(first_grid_day(month), month);
    }

    #[test]
    fn week_start_resolves_to_the_week_s_first_weekday() {
        // 2026-08-06 is a Thursday. Sunday-first week starts 2026-08-02;
        // Monday-first (work week) starts 2026-08-03.
        let thursday = NaiveDate::from_ymd_opt(2026, 8, 6).unwrap();
        assert_eq!(week_start(thursday, chrono::Weekday::Sun), NaiveDate::from_ymd_opt(2026, 8, 2).unwrap());
        assert_eq!(week_start(thursday, chrono::Weekday::Mon), NaiveDate::from_ymd_opt(2026, 8, 3).unwrap());
    }

    #[test]
    fn week_start_surrounds_weekends_correctly() {
        // A Sunday stays in its own Sunday-first week but moves back to the
        // previous Monday in a Monday-first week.
        let sunday = NaiveDate::from_ymd_opt(2026, 8, 9).unwrap();
        assert_eq!(week_start(sunday, chrono::Weekday::Sun), sunday);
        assert_eq!(week_start(sunday, chrono::Weekday::Mon), NaiveDate::from_ymd_opt(2026, 8, 3).unwrap());
    }

    #[test]
    fn last_of_month_handles_short_and_long_months() {
        let feb = NaiveDate::from_ymd_opt(2026, 2, 17).unwrap();
        assert_eq!(last_of_month(feb), NaiveDate::from_ymd_opt(2026, 2, 28).unwrap());
        let august = NaiveDate::from_ymd_opt(2026, 8, 6).unwrap();
        assert_eq!(last_of_month(august), NaiveDate::from_ymd_opt(2026, 8, 31).unwrap());
    }

    #[test]
    fn account_status_is_none_once_calendars_exist() {
        for state in [
            CalendarConnectionState::Disconnected,
            CalendarConnectionState::Connecting,
            CalendarConnectionState::Idle,
            CalendarConnectionState::Busy,
            CalendarConnectionState::Error {
                message: "boom".to_string(),
                retryable: true,
            },
        ] {
            assert_eq!(calendar_account_status_text(&state, true), None);
        }
    }

    #[test]
    fn account_status_maps_connecting_and_disconnected() {
        assert_eq!(calendar_account_status_text(&CalendarConnectionState::Connecting, false).as_deref(), Some("Connecting…"));
        assert_eq!(calendar_account_status_text(&CalendarConnectionState::Disconnected, false).as_deref(), Some("Disconnected"));
    }

    #[test]
    fn account_status_says_no_calendars_when_idle_or_busy_with_none() {
        for state in [CalendarConnectionState::Idle, CalendarConnectionState::Busy] {
            assert_eq!(calendar_account_status_text(&state, false).as_deref(), Some("No calendars found"));
        }
    }

    #[test]
    fn account_status_surfaces_the_session_error() {
        let state = CalendarConnectionState::Error {
            message: "login failed".to_string(),
            retryable: true,
        };
        assert_eq!(calendar_account_status_text(&state, false).as_deref(), Some("login failed"));
    }
}
