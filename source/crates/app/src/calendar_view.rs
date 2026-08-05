use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;

use chrono::{Datelike, NaiveDate, Timelike};
use gtk::cairo::{FontSlant, FontWeight};
use gtk::prelude::*;
use lookout_core::{CalendarId, CalendarInfo, EventOccurrence};
use lookout_dav::session::ConnectionState as CalendarConnectionState;

use crate::calendar_colors;

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
const HOURS_PER_DAY: usize = 24;
/// The local-hour range rendered with a lighter "business hours" background
/// in the Day/Week/Work week grids (8am-6pm, half-open so hour 18 starts the
/// off-hours tone again); everything else is the darker off-hours background.
const BUSINESS_HOURS_START: usize = 8;
const BUSINESS_HOURS_END: usize = 18;
/// Width of the hour-gutter column in the Day/Week grids, wide enough for
/// "5pm"-style labels without pushing the event columns off-balance.
const HOUR_GUTTER_WIDTH: f64 = 52.0;
/// Vertical scale of the Day/Week/Work week time grids: pixels per hour of the
/// day. The full 24-hour timeline is `ALL_DAY_BAND_HEIGHT + 24 * this` tall
/// and scrolls inside a `ScrolledWindow`.
const TIME_SLOT_HEIGHT: f64 = 48.0;
/// Height of the "All day" band above the 24-hour timeline.
const ALL_DAY_BAND_HEIGHT: f64 = 26.0;
/// The time grids are drawn on a single Cairo canvas (so event chips can be
/// positioned by their start/end times and multi-day events can span columns),
/// which means the old `.calendar-hour-cell*` CSS tones are needed in Cairo
/// form. `#26262a` off-hours background...
const GRID_BACKGROUND_RGB: (f64, f64, f64) = (0.149, 0.149, 0.165);
/// ...`#3d3d44` business-hours stripe...
const GRID_BUSINESS_RGB: (f64, f64, f64) = (0.239, 0.239, 0.267);
/// ...and `#2e2e32` for the all-day band (the main panel's own background).
const GRID_ALL_DAY_RGB: (f64, f64, f64) = (0.180, 0.180, 0.196);
/// Dim-label tone for the custom-drawn gutter text.
const GRID_DIM_TEXT_RGBA: (f64, f64, f64, f64) = (0.66, 0.66, 0.72, 1.0);
/// Pixels of slack the text baseline sits above a chip's vertical centre.
const CHIP_TEXT_BASELINE_OFFSET: f64 = 0.36;

struct DayCell {
    container: gtk::Box,
    date_label: gtk::Label,
    events_box: gtk::Box,
}

/// Flat hairline grids (`.calendar-day-cell`) instead of libadwaita's rounded
/// `.card` panels, and a bordered highlight for today (`.calendar-today-cell`) -
/// matches the Outlook reference this view is styled against. The Day/Week/Work
/// week grids are fully custom-drawn now (their old `.calendar-hour-cell` CSS
/// lives on as the Cairo `GRID_*_RGB` tones in [`paint_time_grid`]). Registered
/// once (from [`build_main`]) on the default display, same pattern as
/// `window.rs`'s `install_paned_css()`.
fn install_calendar_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(
        ".calendar-day-cell {
            border: 1px solid alpha(currentColor, 0.08);
        }
        .calendar-today-cell {
            border: 2px solid @accent_bg_color;
        }
        .calendar-main-background {
            background-color: #2e2e32;
            border-radius: 12px;
        }
        .calendar-toggle {
            padding: 2px 4px;
            border-radius: 6px;
            background: transparent;
            box-shadow: none;
        }
        .calendar-toggle:hover,
        .calendar-toggle:active,
        .calendar-toggle:checked {
            background: transparent;
            box-shadow: none;
        }
        .mini-calendar-event-day,
        .mini-calendar-today {
            font-weight: bold;
        }
        /* Half-width variant of the mini month grid, used by the Mail view's
           right-hand overview pane. A `Gtk.Button` under Adwaita asks for
           min-width 16px + 10px horizontal padding either side, so seven of
           them force the grid to ~260px of natural width no matter what
           `width_request` the surrounding box sets. Stripping the padding and
           dropping to a caption-sized font halves each cell, which is what
           lets the pane actually render at its requested width. */
        .mini-calendar-compact button {
            min-width: 0;
            min-height: 0;
            padding: 1px 2px;
        }
        .mini-calendar-compact label {
            font-size: 0.8em;
        }
        .mini-calendar-today {
            color: @accent_bg_color;
        }
        .calendar-account-header {
            font-weight: bold;
        }
        .calendar-toggle-label {
            font-weight: normal;
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
/// Occurrences are bucketed by *every* date they cover (so multi-day events
/// appear in each day cell they occupy, and events that started before the
/// grid's window still show on their in-grid days); dates outside the grid's
/// currently-displayed 6-week span are silently ignored.
pub fn set_month_occurrences(mg: &MonthGrid, occurrences: &[EventOccurrence], colors: &HashMap<CalendarId, String>) {
    let grid_start = first_grid_day(*mg.anchor_month.borrow());

    for cell in &mg.day_cells {
        clear_children(&cell.events_box);
    }

    let by_date = bucket_by_date(occurrences, grid_start, mg.day_cells.len());

    for (i, cell) in mg.day_cells.iter().enumerate() {
        let date = grid_start + chrono::Duration::days(i as i64);
        let Some(day_occurrences) = by_date.get(&date) else { continue };
        for occ in &day_occurrences[..day_occurrences.len().min(MAX_VISIBLE_EVENTS_PER_DAY)] {
            cell.events_box.append(&event_label(occ, colors));
        }
        if day_occurrences.len() > MAX_VISIBLE_EVENTS_PER_DAY {
            cell.events_box.append(&more_label(day_occurrences.len() - MAX_VISIBLE_EVENTS_PER_DAY));
        }
    }
}

/// Buckets `occurrences` by every local date they cover within the `n`-column
/// window starting at `grid_start`, using [`occurrence_day_range`] so
/// multi-day events land in each day cell they occupy (and events that started
/// before the window still show on their in-grid days). Pure data-in/
/// data-out - the widget-level rendering in [`set_month_occurrences`] is a
/// thin layer over this.
fn bucket_by_date(occurrences: &[EventOccurrence], grid_start: NaiveDate, n: usize) -> HashMap<NaiveDate, Vec<&EventOccurrence>> {
    let mut by_date: HashMap<NaiveDate, Vec<&EventOccurrence>> = HashMap::new();
    for occ in occurrences {
        if let Some((start_col, end_col)) = occurrence_day_range(occ, grid_start, n) {
            for col in start_col..end_col {
                by_date.entry(grid_start + chrono::Duration::days(col as i64)).or_default().push(occ);
            }
        }
    }
    by_date
}

/// The half-open `[start_col, end_col)` range of day-column indices (relative
/// to `first`, the grid's first day) that `occ` covers, clipped to a
/// `n`-column window - or `None` if it doesn't touch the window at all. Uses
/// the same column maths as [`compute_time_grid_chips`]: an occurrence ending
/// exactly at local midnight of the day after it starts occupies no minutes of
/// that day, so its end column drops off. Events starting before the window
/// (and events ending after it) are clamped so they still render on their
/// in-window days.
fn occurrence_day_range(occ: &EventOccurrence, first: NaiveDate, n: usize) -> Option<(usize, usize)> {
    if n == 0 {
        return None;
    }
    let start_local = occ.start.with_timezone(&chrono::Local);
    let end_local = occ.end.with_timezone(&chrono::Local);
    let start_date = start_local.date_naive();
    let end_date = end_local.date_naive();

    let start_col = (start_date - first).num_days();
    let mut end_col = (end_date - first).num_days();
    if end_date != start_date && end_local.hour() == 0 && end_local.minute() == 0 && end_local.second() == 0 {
        end_col -= 1;
    }

    let col_start = start_col.max(0);
    let col_end = end_col.min(n as i64 - 1);
    if col_start <= col_end {
        Some((col_start as usize, col_end as usize + 1))
    } else {
        None
    }
}

/// The local dates in the inclusive `[window_start, window_end]` range that
/// `occ` covers, using the same column maths as [`occurrence_day_range`] - so
/// multi-day events contribute every day they occupy, and events that start
/// before or end after the window are clipped into it rather than dropped.
/// Shared with window.rs so the mini-calendar's event-day markers and the Mail
/// overview's day list agree with the month grid.
pub(crate) fn covered_local_dates(occ: &EventOccurrence, window_start: NaiveDate, window_end: NaiveDate) -> Vec<NaiveDate> {
    let days = (window_end - window_start).num_days();
    if days < 0 {
        return Vec::new();
    }
    let n = days as usize + 1;
    occurrence_day_range(occ, window_start, n)
        .map(|(start_col, end_col)| (start_col..end_col).map(|col| window_start + chrono::Duration::days(col as i64)).collect())
        .unwrap_or_default()
}

/// One event rendered on a [`TimeGrid`] canvas: a rectangle whose horizontal
/// extent is the day column(s) it covers and whose vertical extent is its
/// start/end time. Single-day timed events are lane-assigned within their
/// column so overlapping events render side by side; events spanning days
/// (and all-day events) get a full-width chip spanning their columns.
struct TimeChip {
    /// Index of the first day column this chip covers.
    column: usize,
    /// Number of consecutive day columns this chip spans.
    span: usize,
    /// Horizontal lane within the day column (`0` for full-width spanning
    /// chips, otherwise `lanes`-ths of the column width).
    lane: usize,
    /// Total lanes sharing the day column.
    lanes: usize,
    all_day: bool,
    /// Minutes since local midnight of `column` (timed chips).
    start_minutes: i64,
    /// Minutes since local midnight of the chip's *last* column, exclusive
    /// (`1440` means "runs to the next midnight").
    end_minutes: i64,
    /// Index into the caller's `EventOccurrence` slice.
    occurrence: usize,
}

/// A scrollable, position-based Day/Week/Work week time grid. Each grid is a
/// split view: a fixed all-day band (drawn on `band`, never scrolls away) above
/// a scrollable 24-hour timeline (drawn on `canvas`). Event chips are
/// positioned by their exact start/end times - the old bucket-per-hour cells
/// are gone - and multi-day events can span columns. Two week instances are
/// built (Sunday-first "Week", Monday-first five-column "Work week") plus one
/// single-column "Day" instance, sharing this widget.
pub struct TimeGrid {
    pub root: gtk::Widget,
    /// The fixed all-day band (never scrolls away) - see the struct doc.
    band: gtk::DrawingArea,
    canvas: gtk::DrawingArea,
    /// Weekday/date header labels above the day columns (Week/Work week only;
    /// the Day view's date lives in [`CalendarMain`]'s shared header).
    headers: Vec<gtk::Label>,
    anchor: Rc<RefCell<NaiveDate>>,
    /// Which weekdays make up the columns. The Day view is the single-column
    /// special case (`day_view`), which always shows the anchor date itself.
    weekdays: Vec<chrono::Weekday>,
    day_view: bool,
    data: TimeGridData,
}

/// The per-grid render state shared with the draw/hover closures.
#[derive(Clone)]
struct TimeGridData {
    occurrences: Rc<RefCell<Vec<EventOccurrence>>>,
    colors: Rc<RefCell<HashMap<CalendarId, String>>>,
    /// The consecutive local dates currently displayed, one per column.
    dates: Rc<RefCell<Vec<NaiveDate>>>,
    chips: Rc<RefCell<Vec<TimeChip>>>,
}

fn build_time_grid(weekdays: &[chrono::Weekday], day_view: bool) -> TimeGrid {
    // Two stacked DrawingAreas: the all-day band (fixed, above the scroller)
    // and the 24-hour timeline (scrollable). Both share one `TimeGridData` so
    // they stay in sync, and their column separators align because both are
    // hexpand to the same width.
    let band = gtk::DrawingArea::new();
    band.set_height_request(ALL_DAY_BAND_HEIGHT as i32);
    band.set_hexpand(true);
    let canvas = gtk::DrawingArea::new();
    canvas.set_height_request(HOURS_PER_DAY as i32 * TIME_SLOT_HEIGHT as i32);
    canvas.set_hexpand(true);

    let data = TimeGridData {
        occurrences: Rc::new(RefCell::new(Vec::new())),
        colors: Rc::new(RefCell::new(HashMap::new())),
        dates: Rc::new(RefCell::new(Vec::new())),
        chips: Rc::new(RefCell::new(Vec::new())),
    };
    let hover: Rc<Cell<Option<usize>>> = Rc::new(Cell::new(None));

    {
        let data = data.clone();
        let hover = hover.clone();
        band.set_draw_func(move |_band, cr, width, height| {
            paint_all_day_band(cr, width as f64, height as f64, &data, hover.get());
        });
    }
    {
        let data = data.clone();
        let hover = hover.clone();
        canvas.set_draw_func(move |_canvas, cr, width, height| {
            paint_time_grid(cr, width as f64, height as f64, &data, hover.get());
        });
    }

    attach_hover(&band, true, &data, &hover);
    attach_hover(&canvas, false, &data, &hover);

    let mut headers = Vec::new();
    let root_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).vexpand(true).hexpand(true).build();
    if !day_view {
        let header_row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(1).build();
        header_row.append(&gtk::Label::builder().width_request(HOUR_GUTTER_WIDTH as i32).build());
        for _ in weekdays {
            let label = gtk::Label::builder().css_classes(["dim-label", "caption-heading"]).hexpand(true).build();
            header_row.append(&label);
            headers.push(label);
        }
        root_box.append(&header_row);
    }
    root_box.append(&band);

    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .vexpand(true)
        .hexpand(true)
        .child(&canvas)
        .build();
    root_box.append(&scroller);
    scroller.connect_map(scroll_time_grid_to_now);

    TimeGrid {
        root: root_box.upcast(),
        band,
        canvas,
        headers,
        anchor: Rc::new(RefCell::new(chrono::Utc::now().date_naive())),
        weekdays: weekdays.to_vec(),
        day_view,
        data,
    }
}

/// Wires a canvas up so hovering a chip shows a tooltip with the event's
/// summary + time range and a white highlight ring. `band` selects which of
/// the split halves the controller belongs to (all-day chips live on the band,
/// timed chips on the timeline); both share `hover` so the highlight moves
/// across the two canvases as a unit.
fn attach_hover(canvas: &gtk::DrawingArea, band: bool, data: &TimeGridData, hover: &Rc<Cell<Option<usize>>>) {
    let motion = gtk::EventControllerMotion::new();
    {
        let canvas_widget = canvas.clone();
        let data = data.clone();
        let hover = hover.clone();
        motion.connect_motion(move |_, x, y| {
            let dates_guard = data.dates.borrow();
            let chips_guard = data.chips.borrow();
            let hit = hovered_chip(&chips_guard, canvas_widget.width() as f64, dates_guard.len(), x, y, band);
            if hover.get() != hit {
                hover.set(hit);
                canvas_widget.queue_draw();
            }
            let tooltip = hit.and_then(|i| {
                let chip = &chips_guard[i];
                data.occurrences.borrow().get(chip.occurrence).map(|occ| chip_tooltip(occ, chip))
            });
            canvas_widget.set_tooltip_text(tooltip.as_deref());
        });
    }
    {
        let canvas_widget = canvas.clone();
        let hover = hover.clone();
        motion.connect_leave(move |_| {
            if hover.take().is_some() {
                canvas_widget.queue_draw();
            }
        });
    }
    canvas.add_controller(motion);
}

/// The index of the chip under `(x, y)` among the chips of one split half
/// (`band` = all-day chips, otherwise timed chips), or `None`. Uses the same
/// geometry maths as the paint code, so hover hits exactly what is drawn.
fn hovered_chip(chips: &[TimeChip], canvas_width: f64, n_cols: usize, x: f64, y: f64, band: bool) -> Option<usize> {
    let col_width = if n_cols > 0 {
        ((canvas_width - HOUR_GUTTER_WIDTH).max(0.0)) / n_cols as f64
    } else {
        0.0
    };
    chips
        .iter()
        .enumerate()
        .find(|(_, chip)| {
            chip.all_day == band && {
                let (cx, cy, cw, ch) = chip_geometry(chip, col_width);
                x >= cx && x <= cx + cw && y >= cy && y <= cy + ch
            }
        })
        .map(|(i, _)| i)
}

/// Re-points the grid at `anchor` (the week containing it for Week/Work week,
/// the date itself for Day) and re-renders it with the given occurrences,
/// replacing the old `set_week`/`set_week_occurrences`/`set_day`/
/// `set_day_occurrences` calls.
pub fn set_time_grid(t: &TimeGrid, anchor: NaiveDate, occurrences: &[EventOccurrence], colors: &HashMap<CalendarId, String>) {
    *t.anchor.borrow_mut() = anchor;
    *t.data.occurrences.borrow_mut() = occurrences.to_vec();
    *t.data.colors.borrow_mut() = colors.clone();

    let dates = grid_dates(t);
    let chips = compute_time_grid_chips(&dates, occurrences);
    *t.data.dates.borrow_mut() = dates.clone();
    *t.data.chips.borrow_mut() = chips;

    let today = chrono::Utc::now().date_naive();
    for (i, label) in t.headers.iter().enumerate() {
        let date = dates[i];
        let weekday = WEEKDAY_LABELS[date.weekday().num_days_from_sunday() as usize];
        label.set_label(&format!("{weekday} {}", date.day()));
        if date == today {
            label.add_css_class("accent");
        } else {
            label.remove_css_class("accent");
        }
    }
    t.band.queue_draw();
    t.canvas.queue_draw();
}

/// Scrolls the grid's timeline so the current time sits ~100px from the top.
/// Deferred to an idle callback so the scroll adjustment has been laid out by
/// the time it runs (idle callbacks run after the frame's layout/draw phases).
/// Wired up in [`build_time_grid`] to the scroller's `map` signal, so a
/// time-grid view jumps to "now" every time it becomes visible (stack pages
/// are created once but only the active one is mapped).
fn scroll_time_grid_to_now(scroller: &gtk::ScrolledWindow) {
    let scroller = scroller.clone();
    gtk::glib::idle_add_local_once(move || {
        let now = chrono::Local::now();
        let minutes = now.hour() as i64 * 60 + now.minute() as i64;
        // The all-day band is fixed above the scroller, so only the hour
        // timeline scrolls: place "now" ~100px from the top of that timeline.
        let target = (minutes as f64 * TIME_SLOT_HEIGHT / 60.0 - 100.0).max(0.0);
        let adj = scroller.vadjustment();
        let max = (adj.upper() - adj.page_size()).max(0.0);
        adj.set_value(target.min(max));
    });
}

/// The consecutive local dates the grid's columns render: the anchor date
/// alone for the Day view, otherwise the week (starting on `weekdays[0]`)
/// containing the anchor.
fn grid_dates(t: &TimeGrid) -> Vec<NaiveDate> {
    let anchor = *t.anchor.borrow();
    if t.day_view {
        vec![anchor]
    } else {
        let start = week_start(anchor, t.weekdays[0]);
        (0..t.weekdays.len()).map(|i| start + chrono::Duration::days(i as i64)).collect()
    }
}

/// Maps `occurrences` onto the `dates` grid as positioned [`TimeChip`]s:
/// all-day events stack in the top band, timed events spanning days get a
/// full-width chip, and single-day timed events are lane-assigned within their
/// column. Occurrences that don't touch the grid are skipped. Pure
/// data-in/data-out (no widget state), so the layout logic is unit-testable.
fn compute_time_grid_chips(dates: &[NaiveDate], occurrences: &[EventOccurrence]) -> Vec<TimeChip> {
    let n = dates.len() as i64;
    if n == 0 {
        return Vec::new();
    }
    let first = dates[0];

    // (start_col, end_col inclusive, occurrence index) for all-day chips.
    let mut all_day: Vec<(i64, i64, usize)> = Vec::new();
    // (column, start_minutes, end_minutes, occurrence index) for single-day timed events.
    let mut timed: Vec<(i64, i64, i64, usize)> = Vec::new();
    // (start_col, span_columns, start_minutes, end_minutes, occurrence index)
    // for timed events that cross midnight / span days.
    let mut multi: Vec<(i64, i64, i64, i64, usize)> = Vec::new();

    for (index, occ) in occurrences.iter().enumerate() {
        let start_local = occ.start.with_timezone(&chrono::Local);
        let end_local = occ.end.with_timezone(&chrono::Local);
        let start_date = start_local.date_naive();
        let end_date = end_local.date_naive();

        let start_col = (start_date - first).num_days();
        let start_minutes = start_local.hour() as i64 * 60 + start_local.minute() as i64;

        // `end` is exclusive: an event ending exactly at midnight of the day
        // after it starts occupies no minutes of that day, so that column
        // drops off and the chip just runs to the full 24:00 of its last day.
        let mut end_col = if end_date == start_date { start_col } else { (end_date - first).num_days() };
        let mut end_minutes = end_local.hour() as i64 * 60 + end_local.minute() as i64;
        if end_date != start_date && end_minutes == 0 {
            end_col -= 1;
            end_minutes = 1440;
        }

        if occ.all_day {
            if end_date == start_date {
                end_col = start_col;
            }
            let col_start = start_col.max(0);
            let col_end = end_col.min(n - 1);
            if col_start <= col_end {
                all_day.push((col_start, col_end, index));
            }
            continue;
        }

        let col_start = start_col.max(0);
        let col_end = end_col.min(n - 1);
        if col_start > col_end {
            continue;
        }
        // An event starting before (or ending after) the grid's window fills
        // its first (or last) column from midnight.
        let eff_start_minutes = if start_col < 0 { 0 } else { start_minutes };
        let eff_end_minutes = if end_col > n - 1 { 1440 } else { end_minutes };
        if col_end - col_start >= 1 {
            multi.push((col_start, col_end - col_start + 1, eff_start_minutes, eff_end_minutes, index));
        } else {
            timed.push((col_start, eff_start_minutes, eff_end_minutes, index));
        }
    }

    let mut chips: Vec<TimeChip> = Vec::new();

    // All-day chips first (drawn first; they sit in the band above the timed
    // area). One global greedy lane pass over the whole band keeps a
    // multi-day chip's lane/width consistent across the columns it covers.
    if !all_day.is_empty() {
        all_day.sort_by_key(|(c0, c1, _)| (*c0, *c1));
        let ranges: Vec<(i64, i64)> = all_day.iter().map(|(c0, c1, _)| (*c0, *c1 + 1)).collect();
        let lanes = assign_lanes(&ranges);
        let total = lanes.iter().copied().max().map(|l| l + 1).unwrap_or(1);
        for (i, (c0, c1, occ_index)) in all_day.iter().enumerate() {
            chips.push(TimeChip {
                column: *c0 as usize,
                span: (*c1 - *c0 + 1) as usize,
                lane: lanes[i],
                lanes: total,
                all_day: true,
                start_minutes: 0,
                end_minutes: 1440,
                occurrence: *occ_index,
            });
        }
    }

    // Multi-day timed chips next, as full-width blocks under the lane-shared
    // single-day chips.
    for (col, span, start_min, end_min, occ_index) in multi {
        chips.push(TimeChip {
            column: col as usize,
            span: span as usize,
            lane: 0,
            lanes: 1,
            all_day: false,
            start_minutes: start_min,
            end_minutes: end_min,
            occurrence: occ_index,
        });
    }

    // Single-day timed events last: greedy lane assignment per day column so
    // concurrent events render side by side instead of on top of each other.
    if !timed.is_empty() {
        let mut by_column: BTreeMap<i64, Vec<(usize, i64, i64)>> = BTreeMap::new();
        for (col, start_min, end_min, occ_index) in timed {
            by_column.entry(col).or_default().push((occ_index, start_min, end_min));
        }
        for (col, mut events) in by_column {
            events.sort_by_key(|&(_, start, end)| (start, end));
            let ranges: Vec<(i64, i64)> = events.iter().map(|&(_, start, end)| (start, end)).collect();
            let lanes = assign_lanes(&ranges);
            let total = lanes.iter().copied().max().map(|l| l + 1).unwrap_or(1);
            for (i, (occ_index, start_min, end_min)) in events.iter().enumerate() {
                chips.push(TimeChip {
                    column: col as usize,
                    span: 1,
                    lane: lanes[i],
                    lanes: total,
                    all_day: false,
                    start_minutes: *start_min,
                    end_minutes: *end_min,
                    occurrence: *occ_index,
                });
            }
        }
    }

    chips
}

/// Greedy interval-partitioning lane assignment: given `ranges` sorted by
/// start, returns each range's lane index so no two ranges sharing a lane
/// overlap. Uses the minimum possible number of lanes.
fn assign_lanes(ranges: &[(i64, i64)]) -> Vec<usize> {
    let mut lane_ends: Vec<i64> = Vec::new();
    let mut lanes = Vec::with_capacity(ranges.len());
    for &(start, end) in ranges {
        let mut placed = None;
        for (i, lane_end) in lane_ends.iter().enumerate() {
            if *lane_end <= start {
                placed = Some(i);
                break;
            }
        }
        let lane = match placed {
            Some(i) => i,
            None => {
                lane_ends.push(i64::MIN);
                lane_ends.len() - 1
            }
        };
        lane_ends[lane] = end;
        lanes.push(lane);
    }
    lanes
}

/// The pixel rectangle `chip` occupies for a given day-column width. `x` is
/// offset by the chip's lane, `w` by its span, `y` by its start/end times (or
/// the all-day band for `all_day` chips). Coordinates are relative to the
/// chip's own canvas: the all-day band (for `all_day` chips) or the scrollable
/// timeline below it. Also used for hover hit-testing.
fn chip_geometry(chip: &TimeChip, col_width: f64) -> (f64, f64, f64, f64) {
    let lane_width = col_width / chip.lanes.max(1) as f64;
    let x = HOUR_GUTTER_WIDTH + chip.column as f64 * col_width + chip.lane as f64 * lane_width + 1.0;
    let w = (chip.span as f64 * col_width - 2.0).max(2.0);
    if chip.all_day {
        (x, 1.5, w, ALL_DAY_BAND_HEIGHT - 3.0)
    } else {
        let y = chip.start_minutes as f64 * TIME_SLOT_HEIGHT / 60.0 + 1.0;
        let y_end = chip.end_minutes as f64 * TIME_SLOT_HEIGHT / 60.0 - 1.0;
        (x, y, w, (y_end - y).max(6.0))
    }
}

/// The event chip's label text: "9:30am Summary" when the chip is tall enough
/// to fit a time prefix, otherwise just the summary (all-day chips always
/// show just the summary).
fn chip_text(occ: &EventOccurrence, chip: &TimeChip, chip_height: f64) -> String {
    let summary = occ.summary.as_deref().unwrap_or("(untitled)");
    if chip.all_day {
        summary.to_string()
    } else if chip_height >= 20.0 {
        let time = format_event_time(&occ.start.with_timezone(&chrono::Local));
        format!("{time} {summary}")
    } else {
        summary.to_string()
    }
}

/// The tooltip text for a hovered chip: the summary plus its full time range.
fn chip_tooltip(occ: &EventOccurrence, chip: &TimeChip) -> String {
    if chip.all_day {
        occ.summary.clone().unwrap_or_else(|| "(untitled)".to_string())
    } else {
        let start = format_event_time(&occ.start.with_timezone(&chrono::Local));
        let end = format_event_time(&occ.end.with_timezone(&chrono::Local));
        format!("{start} – {end}  {}", occ.summary.as_deref().unwrap_or("(untitled)"))
    }
}

/// Paints the whole time grid: the all-day band and 24-hour timeline with
/// business-hours shading and hairlines, the hour-gutter labels, and every
/// positioned event chip. Fully custom-drawn so chips sit at their exact
/// start/end times and multi-day events can span columns; text uses cairo's
/// toy font API since themed widget labels can't be placed inside a canvas.
/// Paints the fixed all-day band: the band's background, its column
/// separators, the "All day" gutter label, and every all-day chip. It sits
/// above the scroller so these chips stay visible while the timeline scrolls.
fn paint_all_day_band(cr: &gtk::cairo::Context, width: f64, height: f64, data: &TimeGridData, hover: Option<usize>) {
    let dates = data.dates.borrow();
    let chips = data.chips.borrow();
    let occurrences = data.occurrences.borrow();
    let colors = data.colors.borrow();
    let n = dates.len();
    let columns_width = (width - HOUR_GUTTER_WIDTH).max(0.0);
    let col_width = if n > 0 { columns_width / n as f64 } else { 0.0 };

    cr.set_source_rgb(GRID_BACKGROUND_RGB.0, GRID_BACKGROUND_RGB.1, GRID_BACKGROUND_RGB.2);
    let _ = cr.paint();
    cr.rectangle(HOUR_GUTTER_WIDTH, 0.0, columns_width, height);
    cr.set_source_rgb(GRID_ALL_DAY_RGB.0, GRID_ALL_DAY_RGB.1, GRID_ALL_DAY_RGB.2);
    let _ = cr.fill();

    // Column separators, aligned with the timeline's below.
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.06);
    cr.set_line_width(1.0);
    for col in 0..=n {
        let x = HOUR_GUTTER_WIDTH + col as f64 * col_width;
        cr.move_to(x, 0.0);
        cr.line_to(x, height);
    }
    let _ = cr.stroke();

    paint_right_text(cr, "All day", HOUR_GUTTER_WIDTH - 5.0, 3.0, 10.0, GRID_DIM_TEXT_RGBA, FontWeight::Normal);

    for (i, chip) in chips.iter().enumerate() {
        if chip.all_day {
            paint_chip(cr, chip, &occurrences[chip.occurrence], &colors, col_width, Some(i) == hover);
        }
    }
}

/// Paints the scrollable 24-hour timeline: the dark off-hours background with
/// the business-hours stripe and hairlines, the hour-gutter labels, and every
/// timed event chip (single-day and multi-day spans) positioned at its exact
/// start/end times. Fully custom-drawn so chips sit by the clock and multi-day
/// events can span columns; text uses cairo's toy font API since themed widget
/// labels can't be placed inside a canvas.
fn paint_time_grid(cr: &gtk::cairo::Context, width: f64, height: f64, data: &TimeGridData, hover: Option<usize>) {
    let dates = data.dates.borrow();
    let chips = data.chips.borrow();
    let occurrences = data.occurrences.borrow();
    let colors = data.colors.borrow();
    let n = dates.len();
    let columns_width = (width - HOUR_GUTTER_WIDTH).max(0.0);
    let col_width = if n > 0 { columns_width / n as f64 } else { 0.0 };

    // Background: the dark off-hours tone, then the business-hours stripe (the
    // same tones the old `.calendar-hour-cell` CSS rules used).
    cr.set_source_rgb(GRID_BACKGROUND_RGB.0, GRID_BACKGROUND_RGB.1, GRID_BACKGROUND_RGB.2);
    let _ = cr.paint();
    let business_top = BUSINESS_HOURS_START as f64 * TIME_SLOT_HEIGHT;
    let business_height = (BUSINESS_HOURS_END as f64 - BUSINESS_HOURS_START as f64) * TIME_SLOT_HEIGHT;
    cr.rectangle(HOUR_GUTTER_WIDTH, business_top, columns_width, business_height);
    cr.set_source_rgb(GRID_BUSINESS_RGB.0, GRID_BUSINESS_RGB.1, GRID_BUSINESS_RGB.2);
    let _ = cr.fill();

    // Hairline gridlines: hour rows across the columns, plus column separators.
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.06);
    cr.set_line_width(1.0);
    for hh in 0..=HOURS_PER_DAY {
        let y = hh as f64 * TIME_SLOT_HEIGHT;
        if y > height {
            break;
        }
        cr.move_to(HOUR_GUTTER_WIDTH, y);
        cr.line_to(width, y);
    }
    for col in 0..=n {
        let x = HOUR_GUTTER_WIDTH + col as f64 * col_width;
        cr.move_to(x, 0.0);
        cr.line_to(x, height);
    }
    let _ = cr.stroke();

    // Gutter labels: right-aligned hour markers beside their rows.
    for hh in 0..HOURS_PER_DAY {
        let y = hh as f64 * TIME_SLOT_HEIGHT + 3.0;
        paint_right_text(cr, &hour_gutter_text(hh), HOUR_GUTTER_WIDTH - 5.0, y, 10.0, GRID_DIM_TEXT_RGBA, FontWeight::Normal);
    }

    for (i, chip) in chips.iter().enumerate() {
        if !chip.all_day {
            paint_chip(cr, chip, &occurrences[chip.occurrence], &colors, col_width, Some(i) == hover);
        }
    }
}

/// Paints one event chip (fill, hairline border, hover ring, and its label).
fn paint_chip(cr: &gtk::cairo::Context, chip: &TimeChip, occ: &EventOccurrence, colors: &HashMap<CalendarId, String>, col_width: f64, hovered: bool) {
    let color = colors.get(&occ.calendar_id).map(String::as_str).unwrap_or(calendar_colors::DEFAULT_CHECK_COLOR);
    let (r, g, b) = css_color_rgb(color);
    let (cx, cy, cw, ch) = chip_geometry(chip, col_width);
    if cw < 2.0 || ch < 2.0 {
        return;
    }
    cr.rectangle(cx, cy, cw, ch);
    cr.set_source_rgba(r, g, b, 0.92);
    let _ = cr.fill();
    cr.rectangle(cx + 0.5, cy + 0.5, cw - 1.0, ch - 1.0);
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.35);
    cr.set_line_width(1.0);
    let _ = cr.stroke();
    if hovered {
        cr.rectangle(cx + 0.5, cy + 0.5, cw - 1.0, ch - 1.0);
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.55);
        let _ = cr.stroke();
    }
    let fg = if calendar_colors::readable_foreground(color) == "white" {
        (1.0, 1.0, 1.0, 0.95)
    } else {
        (0.0, 0.0, 0.0, 0.85)
    };
    let text = ellipsize_text(cr, &chip_text(occ, chip, ch), (cw - 8.0).max(4.0));
    if !text.is_empty() {
        cr.set_source_rgba(fg.0, fg.1, fg.2, fg.3);
        cr.select_font_face("Sans", FontSlant::Normal, FontWeight::Normal);
        cr.set_font_size(10.0);
        cr.move_to(cx + 4.0, cy + ch / 2.0 + 10.0 * CHIP_TEXT_BASELINE_OFFSET);
        let _ = cr.show_text(&text);
    }
}

/// Draws `text` right-aligned so its right edge lands on `right_x`, with `y`
/// as the baseline, in the given size/colour/weight. Returns the drawn width
/// (for callers that measure before drawing).
fn paint_right_text(cr: &gtk::cairo::Context, text: &str, right_x: f64, y: f64, size: f64, color: (f64, f64, f64, f64), weight: FontWeight) -> f64 {
    cr.select_font_face("Sans", FontSlant::Normal, weight);
    cr.set_font_size(size);
    let advance = cr.text_extents(text).map(|e| e.x_advance()).unwrap_or(0.0);
    cr.set_source_rgba(color.0, color.1, color.2, color.3);
    cr.move_to(right_x - advance, y);
    let _ = cr.show_text(text);
    advance
}
/// Truncates `text` with an ellipsis so it fits within `max_width` at the
/// current font size, measuring with cairo's toy-text extents. Chips are
/// short strings, so linear truncation is plenty.
fn ellipsize_text(cr: &gtk::cairo::Context, text: &str, max_width: f64) -> String {
    if max_width <= 0.0 {
        return String::new();
    }
    if cr.text_extents(text).map(|e| e.x_advance()).unwrap_or(0.0) <= max_width {
        return text.to_string();
    }
    let mut truncated = text.to_string();
    while !truncated.is_empty() {
        truncated.pop();
        let candidate = format!("{truncated}…");
        if cr.text_extents(&candidate).map(|e| e.x_advance()).unwrap_or(0.0) <= max_width {
            return candidate;
        }
    }
    "…".to_string()
}

/// Parses a `#rgb`/`#rrggbb`/`#rrggbbaa` hex colour to an `(r, g, b)` tuple in
/// 0..=1 for Cairo, falling back to a neutral grey on anything unparseable.
fn css_color_rgb(color: &str) -> (f64, f64, f64) {
    let body = color.trim_start_matches('#');
    let expanded = if body.len() == 3 && body.chars().all(|c| c.is_ascii_hexdigit()) {
        body.chars().flat_map(|c| [c, c]).collect::<String>()
    } else {
        body.to_string()
    };
    let rgba = gtk::gdk::RGBA::parse(format!("#{expanded}")).unwrap_or_else(|_| gtk::gdk::RGBA::new(0.6, 0.6, 0.6, 1.0));
    (rgba.red() as f64, rgba.green() as f64, rgba.blue() as f64)
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

/// Rebuilds the agenda's rows for `anchor` (its own month forward), grouped by
/// day: a day header per local date ("Today"/"Tomorrow"/"Wed 12 Aug") with its
/// events under it, each row a time column plus "5:00pm – 6:00pm summary" (or
/// "All day summary").
pub fn set_agenda(a: &AgendaView, anchor: NaiveDate, occurrences: &[EventOccurrence], colors: &HashMap<CalendarId, String>) {
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

    let today = chrono::Utc::now().date_naive();
    let mut day_groups: Vec<(NaiveDate, Vec<&EventOccurrence>)> = Vec::new();
    for occ in upcoming {
        let date = occ.start.with_timezone(&chrono::Local).date_naive();
        if let Some((_, group)) = day_groups.last_mut().filter(|(d, _)| *d == date) {
            group.push(occ);
        } else {
            day_groups.push((date, vec![occ]));
        }
    }

    for (date, group) in day_groups {
        let header = gtk::Label::builder()
            .label(agenda_day_header(date, today))
            .css_classes(["caption-heading"])
            .xalign(0.0)
            .margin_top(10)
            .build();
        a.events_box.append(&header);

        for occ in group {
            let row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).build();
            let time_label = gtk::Label::builder()
                .label(if occ.all_day {
                    "All day".to_string()
                } else {
                    format_event_time(&occ.start.with_timezone(&chrono::Local))
                })
                .css_classes(["dim-label", "caption"])
                .width_request(96)
                .xalign(0.0)
                .build();

            let text = if occ.all_day {
                occ.summary.clone().unwrap_or_else(|| "(untitled)".to_string())
            } else {
                let start = format_event_time(&occ.start.with_timezone(&chrono::Local));
                let end = format_event_time(&occ.end.with_timezone(&chrono::Local));
                format!("{start} – {end}  {}", occ.summary.as_deref().unwrap_or("(untitled)"))
            };
            let summary_label = gtk::Label::builder()
                .label(&text)
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .css_classes(["caption"])
                .hexpand(true)
                .build();
            apply_event_color(&summary_label, &occ.calendar_id, colors);

            row.append(&time_label);
            row.append(&summary_label);
            a.events_box.append(&row);
        }
    }
}

/// The agenda's day-group header: "Today"/"Tomorrow" when relevant, otherwise
/// the weekday + day + month (e.g. "Wed 12 Aug").
fn agenda_day_header(date: NaiveDate, today: NaiveDate) -> String {
    if date == today {
        "Today".to_string()
    } else if date == today + chrono::Duration::days(1) {
        "Tomorrow".to_string()
    } else {
        date.format("%A %d %b").to_string()
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
    workweek: TimeGrid,
    week: TimeGrid,
    day: TimeGrid,
    agenda: AgendaView,
    split: SplitView,
    anchor: Rc<RefCell<NaiveDate>>,
    occurrences: Rc<RefCell<Vec<EventOccurrence>>>,
    /// The current `CalendarId` -> colour map, mirrored from the sidebar's
    /// checklist so event chips can be tinted to their calendar's colour.
    /// Updated by [`set_calendar_colors`] (from the persisted
    /// `calendar_colors` map) whenever the checklist rebuilds.
    colors: Rc<RefCell<HashMap<CalendarId, String>>>,
    /// Display-level CSS provider whose rules this module rewrites whenever the
    /// "My calendars" checklist is rebuilt, writing the `.calendar-event-<hex>`
    /// chip rules that tint each calendar's event labels. Owned here purely as
    /// a lifetime home for the provider - the rules affect the whole display,
    /// and the window.rs caller rebuilds them via
    /// [`rebuild_calendar_checklist`].
    pub check_colors: gtk::CssProvider,
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
    let workweek = build_time_grid(&WORK_WEEK_DAYS, false);
    let week = build_time_grid(&WEEK_DAYS, false);
    let day = build_time_grid(&[chrono::Weekday::Mon], true);
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
    // Empty until `rebuild_calendar_checklist` writes the per-calendar
    // checkbox rules; registered up-front so the provider is live for the
    // whole lifetime of the main panel.
    let check_colors = gtk::CssProvider::new();
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(&display, &check_colors, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    }
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
        colors: Rc::new(RefCell::new(HashMap::new())),
        check_colors,
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
/// Switching to a time-grid view also scrolls its timeline to the current time
/// (wired to the scroller's `map` signal in [`build_time_grid`]).
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

/// Updates the `CalendarId` -> colour map used to tint event chips. The
/// corresponding `.calendar-event-*` CSS rules are written into
/// `check_colors` by [`rebuild_calendar_checklist`], so this only stores the
/// lookup table - callers are expected to re-render (they normally do, as
/// part of the same checklist refresh).
pub fn set_calendar_colors(c: &CalendarMain, colors: &HashMap<CalendarId, String>) {
    *c.colors.borrow_mut() = colors.clone();
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
    let colors = c.colors.borrow();

    set_month(&c.month, anchor);
    set_month_occurrences(&c.month, &occurrences, &colors);
    set_month(&c.split.month, anchor);
    set_month_occurrences(&c.split.month, &occurrences, &colors);

    set_time_grid(&c.workweek, anchor, &occurrences, &colors);
    set_time_grid(&c.week, anchor, &occurrences, &colors);
    set_time_grid(&c.day, anchor, &occurrences, &colors);

    set_agenda(&c.agenda, anchor, &occurrences, &colors);
    set_agenda(&c.split.agenda, anchor, &occurrences, &colors);

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

/// A single event's compact label: "5:00pm Summary", or just "Summary" for an
/// all-day event. The label carries the `.calendar-event-<hex>` class matching
/// its calendar's colour so it renders as a coloured chip.
fn event_label(occ: &EventOccurrence, colors: &HashMap<CalendarId, String>) -> gtk::Label {
    let text = if occ.all_day {
        occ.summary.clone().unwrap_or_else(|| "(untitled)".to_string())
    } else {
        let time = format_event_time(&occ.start.with_timezone(&chrono::Local));
        format!("{time} {}", occ.summary.as_deref().unwrap_or("(untitled)"))
    };
    let label = gtk::Label::builder()
        .label(&text)
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .css_classes(["caption"])
        .build();
    apply_event_color(&label, &occ.calendar_id, colors);
    label
}

/// Adds the `.calendar-event-<hex>` class to `label` for `calendar_id`'s
/// colour, or does nothing if the colour is unknown. The class's CSS rule is
/// generated alongside the checkbox rules in [`rebuild_calendar_checklist`].
fn apply_event_color(label: &gtk::Label, calendar_id: &CalendarId, colors: &HashMap<CalendarId, String>) {
    if let Some(color) = colors.get(calendar_id) {
        label.add_css_class(&format!("calendar-event-{}", color.trim_start_matches('#')));
    }
}

/// Compact 12-hour hour-gutter label for the Day/Week/Work week grids:
/// `5am`, `12pm`, `11pm` (midnight and noon read as `12am`/`12pm`).
fn hour_gutter_text(h: usize) -> String {
    let h = (h as u32) % 24;
    format!("{}{}", hour_12(h), meridiem(h))
}

/// Local time as a compact 12-hour timestamp with minutes: `9:30am`,
/// `5:00pm`.
fn format_event_time(local: &chrono::DateTime<chrono::Local>) -> String {
    format!("{}:{:02}{}", hour_12(local.hour()), local.minute(), meridiem(local.hour()))
}

/// The 12-hour-clock face for a 0-23 hour (12 for both midnight and noon).
fn hour_12(h: u32) -> u32 {
    let h = h % 12;
    if h == 0 {
        12
    } else {
        h
    }
}

fn meridiem(h: u32) -> &'static str {
    if h < 12 {
        "am"
    } else {
        "pm"
    }
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
    /// The local dates (within the currently-displayed month) that have at
    /// least one occurrence from a checked calendar - their day buttons get
    /// bold numerals (`.mini-calendar-event-day`). Kept so the mini grid's own
    /// prev/next paging re-applies the markers without needing the caller;
    /// updated by [`set_mini_month`]/[`set_mini_event_days`].
    event_days: Rc<RefCell<HashSet<NaiveDate>>>,
    on_day_selected: DaySelectedCallbacks,
}

pub fn build_mini() -> MiniCalendar {
    let anchor_month = Rc::new(RefCell::new(first_of_month(chrono::Utc::now().date_naive())));
    let event_days = Rc::new(RefCell::new(HashSet::new()));
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
        event_days,
        on_day_selected,
    };

    {
        let mini_month = mini.anchor_month.clone();
        let mini_header = mini.header_label.clone();
        let mini_buttons = mini.day_buttons.clone();
        let mini_days = mini.event_days.clone();
        prev_button.connect_clicked(move |_| {
            let current = *mini_month.borrow();
            let event_days = mini_days.borrow();
            let new_month = current.checked_sub_months(chrono::Months::new(1)).unwrap_or(current);
            relabel_mini(&mini_month, &mini_header, &mini_buttons, new_month, &event_days);
        });
    }
    {
        let mini_month = mini.anchor_month.clone();
        let mini_header = mini.header_label.clone();
        let mini_buttons = mini.day_buttons.clone();
        let mini_days = mini.event_days.clone();
        next_button.connect_clicked(move |_| {
            let current = *mini_month.borrow();
            let event_days = mini_days.borrow();
            let new_month = current.checked_add_months(chrono::Months::new(1)).unwrap_or(current);
            relabel_mini(&mini_month, &mini_header, &mini_buttons, new_month, &event_days);
        });
    }

    let initial_month = *mini.anchor_month.borrow();
    let initial_days = mini.event_days.borrow().clone();
    relabel_mini(&mini.anchor_month, &mini.header_label, &mini.day_buttons, initial_month, &initial_days);
    mini
}

/// Re-points the mini grid at `month`'s month and records which local dates
/// within it have events (`event_days`) so their day buttons render bold.
pub fn set_mini_month(mc: &MiniCalendar, month: NaiveDate, event_days: &HashSet<NaiveDate>) {
    *mc.event_days.borrow_mut() = event_days.clone();
    relabel_mini(&mc.anchor_month, &mc.header_label, &mc.day_buttons, month, event_days);
}

/// Updates which dates in the currently-displayed month have events, without
/// moving the month - used when a `SyncMonth` fetch lands for the month the
/// mini grid is already showing.
pub fn set_mini_event_days(mc: &MiniCalendar, event_days: &HashSet<NaiveDate>) {
    *mc.event_days.borrow_mut() = event_days.clone();
    let month = *mc.anchor_month.borrow();
    relabel_mini(&mc.anchor_month, &mc.header_label, &mc.day_buttons, month, event_days);
}

/// The first-of-month date the mini grid is currently showing (see
/// [`set_mini_month`]).
pub fn mini_month(mc: &MiniCalendar) -> NaiveDate {
    *mc.anchor_month.borrow()
}

fn relabel_mini(anchor_month: &Rc<RefCell<NaiveDate>>, header_label: &gtk::Label, day_buttons: &[gtk::Button], month: NaiveDate, event_days: &HashSet<NaiveDate>) {
    let month = first_of_month(month);
    *anchor_month.borrow_mut() = month;
    header_label.set_label(&month.format("%B %Y").to_string());

    let today = chrono::Utc::now().date_naive();
    let grid_start = first_grid_day(month);
    for (i, button) in day_buttons.iter().enumerate() {
        let date = grid_start + chrono::Duration::days(i as i64);
        button.set_label(&date.day().to_string());
        button.remove_css_class("dim-label");
        button.remove_css_class("mini-calendar-event-day");
        button.remove_css_class("mini-calendar-today");
        if date.month() != month.month() {
            button.add_css_class("dim-label");
        }
        if event_days.contains(&date) {
            button.add_css_class("mini-calendar-event-day");
        }
        if date == today {
            button.add_css_class("mini-calendar-today");
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
/// given per-account groups: one account header (dim caption, deliberately not
/// the bold `caption-heading`) per
/// group, its calendars as custom coloured toggle rows (name + hand-drawn
/// radio indicator in the calendar's colour, active = currently checked), or
/// a dim status line when the account has none yet. Each row's `toggled`
/// handler fires `on_toggle`. A plain rebuildable-list function rather than a
/// stateful struct, matching this file's existing data-in/widget-state-out
/// convention - callers own the actual checked/unchecked state and just ask
/// for a fresh render of it.
///
/// Every calendar's row is tinted with its assigned colour from `colors` (the
/// colour normally lives in `calendar_colors::load`, but this function only
/// needs the map - the caller resolves/persists assignments). The same pass
/// writes the matching `.calendar-event-<hex>` chip rules into `check_colors`,
/// a display-level provider owned by the caller.
pub fn rebuild_calendar_checklist(
    container: &gtk::Box,
    groups: &[CalendarAccountGroup],
    checked: &HashSet<CalendarId>,
    colors: &HashMap<CalendarId, String>,
    check_colors: &gtk::CssProvider,
    on_toggle: impl Fn(CalendarId, bool) + 'static + Clone,
) {
    clear_children(container);
    let mut css = String::new();
    if groups.is_empty() {
        let placeholder = gtk::Label::builder()
            .label("No calendars connected")
            .css_classes(["dim-label", "caption"])
            .xalign(0.0)
            .build();
        container.append(&placeholder);
    }
    for group in groups {
        let header = gtk::Label::builder()
            .label(&group.display_name)
            .css_classes(["dim-label", "caption", "calendar-account-header"])
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
            let color = colors.get(&calendar.id).map(String::as_str).unwrap_or(calendar_colors::DEFAULT_CHECK_COLOR);
            css.push_str(&calendar_color_css(color));
            let id = calendar.id.clone();
            let on_toggle = on_toggle.clone();
            let toggle = calendar_toggle_row(&calendar.display_name, color, checked.contains(&calendar.id), move |is_checked| {
                on_toggle(id.clone(), is_checked)
            });
            container.append(&toggle);
        }
    }
    check_colors.load_from_string(&css);
}

/// One row in the "My calendars" checklist: a flat `ToggleButton` carrying the
/// calendar's name plus a hand-drawn 16px radio indicator (a `DrawingArea`)
/// painted in the calendar's colour - checked = solid disc with a white inner
/// dot, unchecked = a hollow ring. The indicator is drawn rather than themed
/// because a stock GTK checkbox paints its `.check` node through an internal
/// path that ignores display-level overrides, whereas the `DrawingArea` gives
/// the colour full control. `active` seeds the button's state; `on_toggle`
/// fires with the new state whenever it changes.
fn calendar_toggle_row(name: &str, color: &str, active: bool, on_toggle: impl Fn(bool) + 'static) -> gtk::ToggleButton {
    let button = gtk::ToggleButton::new();
    button.add_css_class("flat");
    button.add_css_class("calendar-toggle");
    button.set_active(active);

    let checked = Rc::new(Cell::new(active));
    let indicator = gtk::DrawingArea::builder().width_request(16).height_request(16).build();
    {
        let checked = checked.clone();
        let color = color.to_string();
        indicator.set_draw_func(move |_, cr, width, height| draw_calendar_indicator(cr, width, height, &color, checked.get()));
    }
    let label = gtk::Label::builder().label(name).xalign(0.0).hexpand(true).css_classes(["calendar-toggle-label"]).build();
    let content = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).build();
    content.append(&indicator);
    content.append(&label);
    button.set_child(Some(&content));

    {
        let checked = checked.clone();
        let indicator = indicator.clone();
        button.connect_toggled(move |btn| {
            checked.set(btn.is_active());
            indicator.queue_draw();
            on_toggle(btn.is_active());
        });
    }

    button
}

/// Paints one calendar's radio indicator into `cr`: a solid disc in `color`
/// with a white inner dot when `checked`, otherwise a hollow ring. `color` is
/// a `#rgb`/`#rrggbb`/`#rrggbbaa` hex value; anything unparseable falls back
/// to a neutral grey.
fn draw_calendar_indicator(cr: &gtk::cairo::Context, width: i32, height: i32, color: &str, checked: bool) {
    let rgba = gtk::gdk::RGBA::parse(color).unwrap_or_else(|_| gtk::gdk::RGBA::new(0.6, 0.6, 0.6, 1.0));
    let (r, g, b) = (rgba.red(), rgba.green(), rgba.blue());
    let (w, h) = (f64::from(width.max(1)), f64::from(height.max(1)));
    let (cx, cy) = (w / 2.0, h / 2.0);
    let radius = w.min(h) / 2.0 - 1.0;
    if checked {
        cr.set_source_rgba(r as f64, g as f64, b as f64, 1.0);
        cr.arc(cx, cy, radius, 0.0, std::f64::consts::TAU);
        let _ = cr.fill();
        cr.set_source_rgba(1.0, 1.0, 1.0, 1.0);
        cr.arc(cx, cy, radius * 0.32, 0.0, std::f64::consts::TAU);
        let _ = cr.fill();
    } else {
        cr.set_source_rgba(r as f64, g as f64, b as f64, 0.8);
        cr.set_line_width(1.5);
        cr.arc(cx, cy, radius - 0.75, 0.0, std::f64::consts::TAU);
        let _ = cr.stroke();
    }
}

/// The CSS rule for one calendar's colour: the `.calendar-event-<hex>` chip
/// rule that [`event_label`] applies to occurrences of each calendar. The
/// checklist's own indicator is hand-drawn (see [`calendar_toggle_row`]) and
/// needs no CSS.
fn calendar_color_css(color: &str) -> String {
    let fg = calendar_colors::readable_foreground(color);
    format!(
        ".calendar-event-{} {{ background-color: {color}; color: {fg}; border-radius: 4px; padding: 1px 4px; }}\n",
        color.trim_start_matches('#')
    )
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
    use chrono::NaiveDateTime;
    use lookout_core::EventUid;

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
    fn hour_gutter_labels_are_compact_12_hour() {
        assert_eq!(hour_gutter_text(0), "12am");
        assert_eq!(hour_gutter_text(5), "5am");
        assert_eq!(hour_gutter_text(8), "8am");
        assert_eq!(hour_gutter_text(12), "12pm");
        assert_eq!(hour_gutter_text(13), "1pm");
        assert_eq!(hour_gutter_text(17), "5pm");
        assert_eq!(hour_gutter_text(23), "11pm");
    }

    #[test]
    fn event_times_are_compact_12_hour_with_minutes() {
        use chrono::TimeZone;
        let at = |hour: u32, minute: u32| {
            let naive = NaiveDate::from_ymd_opt(2026, 8, 6).unwrap().and_hms_opt(hour, minute, 0).unwrap();
            chrono::Local.from_local_datetime(&naive).single().unwrap()
        };
        assert_eq!(format_event_time(&at(9, 30)), "9:30am");
        assert_eq!(format_event_time(&at(17, 0)), "5:00pm");
        assert_eq!(format_event_time(&at(0, 5)), "12:05am");
        assert_eq!(format_event_time(&at(12, 15)), "12:15pm");
    }

    fn test_week() -> Vec<NaiveDate> {
        // Sunday 2026-08-02 through Saturday 2026-08-08 (same window the other
        // date tests use).
        (0..7).map(|i| NaiveDate::from_ymd_opt(2026, 8, 2).unwrap() + chrono::Duration::days(i as i64)).collect()
    }

    /// Builds an occurrence whose start/end land on the given naive local
    /// times regardless of the test host's timezone (they round-trip through
    /// Local on the way in and out of [`compute_time_grid_chips`]).
    fn occ(summary: &str, start: NaiveDateTime, end: NaiveDateTime, all_day: bool) -> EventOccurrence {
        use chrono::TimeZone;
        EventOccurrence {
            uid: EventUid(summary.to_string()),
            calendar_id: CalendarId("test".to_string()),
            summary: Some(summary.to_string()),
            start: chrono::Local.from_local_datetime(&start).single().unwrap().with_timezone(&chrono::Utc),
            end: chrono::Local.from_local_datetime(&end).single().unwrap().with_timezone(&chrono::Utc),
            all_day,
        }
    }

    #[test]
    fn assign_lanes_reuses_lanes_for_non_overlapping_ranges() {
        // Sorted by start: 9-10, 9:30-10:30, 10-11. The third reuses lane 0
        // because the first ended at 10:00 (ends are exclusive).
        let ranges = [(9 * 60, 10 * 60), (9 * 60 + 30, 10 * 60 + 30), (10 * 60, 11 * 60)];
        assert_eq!(assign_lanes(&ranges), vec![0, 1, 0]);
    }

    #[test]
    fn assign_lanes_stacks_nested_ranges() {
        let ranges = [(9 * 60, 12 * 60), (9 * 60 + 30, 11 * 60 + 30), (10 * 60, 11 * 60)];
        assert_eq!(assign_lanes(&ranges), vec![0, 1, 2]);
    }

    #[test]
    fn grid_chips_lane_concurrent_timed_events_side_by_side() {
        let day = NaiveDate::from_ymd_opt(2026, 8, 6).unwrap();
        let occurrences = vec![
            occ("early", day.and_hms_opt(9, 0, 0).unwrap(), day.and_hms_opt(10, 0, 0).unwrap(), false),
            occ("late", day.and_hms_opt(9, 30, 0).unwrap(), day.and_hms_opt(11, 0, 0).unwrap(), false),
        ];
        let chips = compute_time_grid_chips(&test_week(), &occurrences);
        assert_eq!(chips.len(), 2);
        assert_eq!(chips[0].occurrence, 0);
        assert_eq!(chips[0].lane, 0);
        assert_eq!(chips[1].occurrence, 1);
        assert_eq!(chips[1].lane, 1);
        assert_eq!(chips[1].lanes, 2);
        for chip in &chips {
            assert!(!chip.all_day);
            assert_eq!(chip.span, 1);
            assert_eq!(chip.column, 4); // 2026-08-06 is Thursday, the 5th column
        }
    }

    #[test]
    fn grid_chips_span_days_for_multi_day_timed_events() {
        let wed = NaiveDate::from_ymd_opt(2026, 8, 5).unwrap();
        let thu = NaiveDate::from_ymd_opt(2026, 8, 6).unwrap();
        let occurrences = vec![occ("overnight", wed.and_hms_opt(22, 0, 0).unwrap(), thu.and_hms_opt(2, 0, 0).unwrap(), false)];
        let chips = compute_time_grid_chips(&test_week(), &occurrences);
        assert_eq!(chips.len(), 1);
        assert_eq!(chips[0].column, 3);
        assert_eq!(chips[0].span, 2);
        assert_eq!(chips[0].start_minutes, 22 * 60);
        assert_eq!(chips[0].end_minutes, 2 * 60);
        assert_eq!(chips[0].lanes, 1);
    }

    #[test]
    fn grid_chips_collapse_midnight_enders_to_one_column() {
        let wed = NaiveDate::from_ymd_opt(2026, 8, 5).unwrap();
        let thu = NaiveDate::from_ymd_opt(2026, 8, 6).unwrap();
        let occurrences = vec![occ("closer", wed.and_hms_opt(10, 0, 0).unwrap(), thu.and_hms_opt(0, 0, 0).unwrap(), false)];
        let chips = compute_time_grid_chips(&test_week(), &occurrences);
        assert_eq!(chips.len(), 1);
        assert_eq!(chips[0].span, 1);
        assert_eq!(chips[0].start_minutes, 10 * 60);
        assert_eq!(chips[0].end_minutes, 1440);
    }

    #[test]
    fn grid_chips_clip_all_day_events_to_the_grid_window() {
        let fri = NaiveDate::from_ymd_opt(2026, 8, 7).unwrap();
        let mon = NaiveDate::from_ymd_opt(2026, 8, 10).unwrap();
        let before = NaiveDate::from_ymd_opt(2026, 8, 1).unwrap();
        let occurrences = vec![
            occ("weekender", fri.and_hms_opt(0, 0, 0).unwrap(), mon.and_hms_opt(0, 0, 0).unwrap(), true),
            occ("outside", before.and_hms_opt(0, 0, 0).unwrap(), before.and_hms_opt(0, 0, 0).unwrap(), true),
        ];
        let chips = compute_time_grid_chips(&test_week(), &occurrences);
        assert_eq!(chips.len(), 1);
        assert_eq!(chips[0].occurrence, 0);
        assert!(chips[0].all_day);
        assert_eq!(chips[0].column, 5);
        assert_eq!(chips[0].span, 2);
        assert_eq!(chips[0].start_minutes, 0);
        assert_eq!(chips[0].end_minutes, 1440);
    }

    #[test]
    fn grid_chips_skip_events_outside_the_window() {
        let earlier = NaiveDate::from_ymd_opt(2026, 7, 30).unwrap();
        let later = NaiveDate::from_ymd_opt(2026, 8, 10).unwrap();
        let occurrences = vec![
            occ("too early", earlier.and_hms_opt(9, 0, 0).unwrap(), earlier.and_hms_opt(10, 0, 0).unwrap(), false),
            occ("too late", later.and_hms_opt(9, 0, 0).unwrap(), later.and_hms_opt(10, 0, 0).unwrap(), false),
        ];
        assert!(compute_time_grid_chips(&test_week(), &occurrences).is_empty());
    }

    #[test]
    fn occurrence_day_range_covers_every_day_of_a_multi_day_all_day_event() {
        // Friday 2026-08-07 through Monday 2026-08-10 (all-day) covers Fri,
        // Sat and Sun; a month-length window from the week's first day keeps
        // the whole span visible.
        let fri = NaiveDate::from_ymd_opt(2026, 8, 7).unwrap();
        let mon = NaiveDate::from_ymd_opt(2026, 8, 10).unwrap();
        let occ = occ("weekender", fri.and_hms_opt(0, 0, 0).unwrap(), mon.and_hms_opt(0, 0, 0).unwrap(), true);
        let first = test_week()[0];
        let range = occurrence_day_range(&occ, first, 31).unwrap();
        assert_eq!(range, (5, 8));
        for col in range.0..range.1 {
            let covered = first + chrono::Duration::days(col as i64);
            assert!(occ.start.with_timezone(&chrono::Local).date_naive() <= covered);
            assert!(covered < occ.end.with_timezone(&chrono::Local).date_naive());
        }
    }

    #[test]
    fn occurrence_day_range_covers_both_days_of_a_midnight_crosser() {
        // 22:00 Wednesday 2026-08-05 through 02:00 Thursday 2026-08-06.
        let wed = NaiveDate::from_ymd_opt(2026, 8, 5).unwrap();
        let thu = NaiveDate::from_ymd_opt(2026, 8, 6).unwrap();
        let occ = occ("overnight", wed.and_hms_opt(22, 0, 0).unwrap(), thu.and_hms_opt(2, 0, 0).unwrap(), false);
        let week = test_week();
        let first = week[0];
        assert_eq!(occurrence_day_range(&occ, first, week.len()).unwrap(), (3, 5));
    }

    #[test]
    fn occurrence_day_range_drops_the_day_an_event_ends_at_midnight() {
        // 10:00 Wednesday 2026-08-05 through 00:00 Thursday 2026-08-06 occupies
        // no minutes of Thursday, so it must only cover Wednesday.
        let wed = NaiveDate::from_ymd_opt(2026, 8, 5).unwrap();
        let thu = NaiveDate::from_ymd_opt(2026, 8, 6).unwrap();
        let occ = occ("closer", wed.and_hms_opt(10, 0, 0).unwrap(), thu.and_hms_opt(0, 0, 0).unwrap(), false);
        let week = test_week();
        let first = week[0];
        assert_eq!(occurrence_day_range(&occ, first, week.len()).unwrap(), (3, 4));
    }

    #[test]
    fn occurrence_day_range_clips_events_starting_before_the_window() {
        // Starts 2026-08-01, before the week's Sunday 2026-08-02; must clamp to
        // the window (covering Sun/Mon/Tue) rather than vanishing.
        let before = NaiveDate::from_ymd_opt(2026, 8, 1).unwrap();
        let wed = NaiveDate::from_ymd_opt(2026, 8, 5).unwrap();
        let occ = occ("vacation", before.and_hms_opt(0, 0, 0).unwrap(), wed.and_hms_opt(0, 0, 0).unwrap(), true);
        let week = test_week();
        let first = week[0];
        assert_eq!(occurrence_day_range(&occ, first, week.len()).unwrap(), (0, 3));
    }

    #[test]
    fn occurrence_day_range_returns_none_for_events_outside_the_window() {
        let earlier = NaiveDate::from_ymd_opt(2026, 7, 30).unwrap();
        let later = NaiveDate::from_ymd_opt(2026, 8, 10).unwrap();
        let occurrences = vec![
            occ("too early", earlier.and_hms_opt(9, 0, 0).unwrap(), earlier.and_hms_opt(10, 0, 0).unwrap(), false),
            occ("too late", later.and_hms_opt(9, 0, 0).unwrap(), later.and_hms_opt(10, 0, 0).unwrap(), false),
        ];
        let week = test_week();
        let first = week[0];
        for occ in &occurrences {
            assert!(occurrence_day_range(occ, first, week.len()).is_none());
        }
    }

    #[test]
    fn covered_local_dates_clips_to_the_window_and_skips_outsiders() {
        // All-day trip covering 2026-08-04 through 2026-08-07 (Tue/Wed/Thu).
        let trip = occ(
            "Trip",
            NaiveDate::from_ymd_opt(2026, 8, 4).unwrap().and_hms_opt(0, 0, 0).unwrap(),
            NaiveDate::from_ymd_opt(2026, 8, 7).unwrap().and_hms_opt(0, 0, 0).unwrap(),
            true,
        );
        let window_start = NaiveDate::from_ymd_opt(2026, 8, 1).unwrap();
        let window_end = NaiveDate::from_ymd_opt(2026, 8, 31).unwrap();
        let covered = covered_local_dates(&trip, window_start, window_end);
        assert_eq!(
            covered,
            vec![
                NaiveDate::from_ymd_opt(2026, 8, 4).unwrap(),
                NaiveDate::from_ymd_opt(2026, 8, 5).unwrap(),
                NaiveDate::from_ymd_opt(2026, 8, 6).unwrap(),
            ]
        );

        // An event entirely outside the window contributes nothing.
        let outside = occ(
            "Outside",
            NaiveDate::from_ymd_opt(2026, 9, 5).unwrap().and_hms_opt(9, 0, 0).unwrap(),
            NaiveDate::from_ymd_opt(2026, 9, 5).unwrap().and_hms_opt(10, 0, 0).unwrap(),
            false,
        );
        assert!(covered_local_dates(&outside, window_start, window_end).is_empty());
    }

    #[test]
    fn bucket_by_date_places_multi_day_events_on_every_covered_day() {
        // An all-day trip covering Tue 4 Aug through Fri 7 Aug (i.e. Tue/Wed/Thu),
        // plus a same-day meeting on the Wednesday. The grid starts Sunday 26 Jul
        // (Sunday before 1 Aug 2026).
        let trip = occ(
            "Trip",
            NaiveDate::from_ymd_opt(2026, 8, 4).unwrap().and_hms_opt(0, 0, 0).unwrap(),
            NaiveDate::from_ymd_opt(2026, 8, 7).unwrap().and_hms_opt(0, 0, 0).unwrap(),
            true,
        );
        let meeting = occ(
            "Meeting",
            NaiveDate::from_ymd_opt(2026, 8, 5).unwrap().and_hms_opt(9, 0, 0).unwrap(),
            NaiveDate::from_ymd_opt(2026, 8, 5).unwrap().and_hms_opt(10, 0, 0).unwrap(),
            false,
        );
        let grid_start = NaiveDate::from_ymd_opt(2026, 7, 26).unwrap();
        let occurrences = [trip, meeting];
        let by_date = bucket_by_date(&occurrences, grid_start, 42);

        let summaries = |date: NaiveDate| {
            let mut names: Vec<String> = by_date.get(&date).into_iter().flat_map(|occ| occ.iter()).filter_map(|occ| occ.summary.clone()).collect();
            names.sort();
            names
        };
        // The trip appears on each day it covers, alongside the meeting on the
        // Wednesday, and doesn't leak into Friday (its end, midnight exclusive).
        assert_eq!(summaries(NaiveDate::from_ymd_opt(2026, 8, 4).unwrap()), vec!["Trip"]);
        assert_eq!(summaries(NaiveDate::from_ymd_opt(2026, 8, 5).unwrap()), vec!["Meeting", "Trip"]);
        assert_eq!(summaries(NaiveDate::from_ymd_opt(2026, 8, 6).unwrap()), vec!["Trip"]);
        assert!(summaries(NaiveDate::from_ymd_opt(2026, 8, 7).unwrap()).is_empty());
    }

    #[test]
    fn hovered_chip_matches_only_its_split_half() {
        let fri = NaiveDate::from_ymd_opt(2026, 8, 7).unwrap();
        let thu = NaiveDate::from_ymd_opt(2026, 8, 6).unwrap();
        let occurrences = vec![
            occ("band event", fri.and_hms_opt(0, 0, 0).unwrap(), fri.and_hms_opt(0, 0, 0).unwrap(), true),
            occ("timed event", thu.and_hms_opt(9, 0, 0).unwrap(), thu.and_hms_opt(10, 0, 0).unwrap(), false),
        ];
        let chips = compute_time_grid_chips(&test_week(), &occurrences);
        assert_eq!(chips.len(), 2);
        let (band_chip, timed_chip) = if chips[0].all_day { (&chips[0], &chips[1]) } else { (&chips[1], &chips[0]) };

        // Synthetic canvas width that resolves to a 100px column so the
        // geometry computed here matches what `hovered_chip` derives.
        let canvas_width = 100.0 * 7.0 + HOUR_GUTTER_WIDTH;

        let (bx, by, bw, bh) = chip_geometry(band_chip, 100.0);
        let band_centre = (bx + bw / 2.0, by + bh / 2.0);
        assert_eq!(
            hovered_chip(&chips, canvas_width, 7, band_centre.0, band_centre.1, true).map(|i| chips[i].all_day),
            Some(true)
        );
        assert!(hovered_chip(&chips, canvas_width, 7, band_centre.0, band_centre.1, false).is_none());

        let (tx, ty, tw, th) = chip_geometry(timed_chip, 100.0);
        let timed_centre = (tx + tw / 2.0, ty + th / 2.0);
        assert_eq!(
            hovered_chip(&chips, canvas_width, 7, timed_centre.0, timed_centre.1, false).map(|i| chips[i].all_day),
            Some(false)
        );
        assert!(hovered_chip(&chips, canvas_width, 7, timed_centre.0, timed_centre.1, true).is_none());
    }

    #[test]
    fn calendar_color_css_parses_as_valid_gtk_css() {
        // `load_from_string` swallows parse errors (logging them to GLib), so
        // assert on `to_str()` instead: a provider that failed to parse the
        // rules round-trips to an empty string. Skipped when the test host has
        // no display to initialise GTK against.
        if gtk::init().is_err() {
            return;
        }
        let provider = gtk::CssProvider::new();
        provider.load_from_string(&calendar_color_css("#3584e4"));
        let round_tripped = provider.to_str();
        assert!(!round_tripped.is_empty(), "calendar colour CSS did not parse");
        assert!(round_tripped.contains("calendar-event-3584e4"));

        let multi = format!("{}{}", calendar_color_css("#e5a50a"), calendar_color_css("#56b9c4"));
        provider.load_from_string(&multi);
        let round_tripped = provider.to_str();
        assert!(!round_tripped.is_empty(), "multi-colour CSS did not parse");
        assert!(round_tripped.contains("calendar-event-e5a50a"));
        assert!(round_tripped.contains("calendar-event-56b9c4"));
    }

    #[test]
    fn calendar_toggle_row_fires_callback_with_new_state() {
        // The custom checklist rows are plain ToggleButtons with a drawn
        // indicator; verify they seed the given checked state and fire
        // `on_toggle` with the new state as they flip. Skipped when the test
        // host has no display to initialise GTK against.
        if gtk::init().is_err() {
            return;
        }
        let calls: Rc<RefCell<Vec<bool>>> = Rc::new(RefCell::new(Vec::new()));
        let toggle = calendar_toggle_row("Work", "#3584e4", false, {
            let calls = calls.clone();
            move |is_checked| calls.borrow_mut().push(is_checked)
        });
        assert!(!toggle.is_active(), "row seeded unchecked");
        assert!(toggle.child().is_some(), "row carries content (indicator + label)");

        toggle.set_active(true);
        assert!(toggle.is_active(), "first toggle checks the row");
        toggle.set_active(false);
        assert!(!toggle.is_active(), "second toggle unchecks the row");
        assert_eq!(calls.borrow().as_slice(), &[true, false][..]);

        let seeded = calendar_toggle_row("Private", "#e5a50a", true, |_| {});
        assert!(seeded.is_active(), "row seeded checked");
    }

    #[test]
    fn application_priority_css_beats_theme_on_toggle_button() {
        // The checklist's custom toggle rows rely on display-level CSS at
        // STYLE_PROVIDER_PRIORITY_APPLICATION (their `flat`/`calendar-toggle`
        // classes), so verify that a provider at that priority is consulted
        // for a real widget's style context (reachability) and that its rules
        // beat the active libadwaita/Yaru theme (precedence). Uses the
        // deprecated GtkStyleContext lookup because gtk4-rs 0.11 does not wrap
        // gtk_widget_css_lookup_color; skipped when the test host has no
        // display to initialise GTK against.
        if gtk::init().is_err() {
            return;
        }
        let provider = gtk::CssProvider::new();
        provider.load_from_string(
            "@define-color probe #3584e4;\n\
             .calendar-toggle { color: #123456; }\n\
             .calendar-toggle:checked { color: #89abcd; }",
        );
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::style_context_add_provider_for_display(&display, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
        }
        let window = gtk::Window::builder().default_width(40).default_height(40).build();
        let toggle = gtk::ToggleButton::new();
        toggle.add_css_class("calendar-toggle");
        toggle.set_active(true);
        window.set_child(Some(&toggle));
        window.present();
        for _ in 0..10 {
            while gtk::glib::MainContext::default().iteration(false) {}
        }
        use gtk::glib::prelude::Cast;
        use gtk::glib::translate::ToGlibPtr;
        let widget = toggle.upcast_ref::<gtk::Widget>();
        let context = unsafe { gtk::ffi::gtk_widget_get_style_context(widget.to_glib_none().0) };
        assert!(!context.is_null(), "no style context for the toggle button");

        let mut probe = gtk::gdk::ffi::GdkRGBA {
            red: 0.0,
            green: 0.0,
            blue: 0.0,
            alpha: 0.0,
        };
        let resolved = unsafe { gtk::ffi::gtk_style_context_lookup_color(context, c"probe".as_ptr(), &mut probe) };
        assert!(resolved != 0, "display-level @define-color did not resolve on the widget's style context");
        assert!((probe.red - ((0x35_u32 as f64) / 255.0) as f32).abs() < 0.01, "probe red = {}", probe.red);

        let mut color = gtk::gdk::ffi::GdkRGBA {
            red: 0.0,
            green: 0.0,
            blue: 0.0,
            alpha: 0.0,
        };
        unsafe {
            gtk::ffi::gtk_style_context_get_color(context, &mut color);
        }
        let (r, g, b) = (color.red, color.green, color.blue);
        assert!(
            (r - ((0x89_u32 as f64) / 255.0) as f32).abs() < 0.01 && (g - ((0xab_u32 as f64) / 255.0) as f32).abs() < 0.01 && (b - ((0xcd_u32 as f64) / 255.0) as f32).abs() < 0.01,
            "computed color = #{:02x}{:02x}{:02x}, expected the checked rule #89abcd (APPLICATION priority lost to the theme?)",
            (r * 255.0) as u8,
            (g * 255.0) as u8,
            (b * 255.0) as u8
        );
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
