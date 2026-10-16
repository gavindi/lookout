/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;

use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, Timelike, Utc};
use gtk::cairo::{FontSlant, FontWeight};
use gtk::prelude::*;
use lookout_core::calendar_protocol::ConnectionState as CalendarConnectionState;
use lookout_core::{BusyStatus, CalendarId, CalendarInfo, EventOccurrence};

use crate::calendar_colors;
use crate::context_menu::{ActionSpec, ContextMenu, Dispatch, MenuDescription};

/// The short label for each weekday, indexed by `num_days_from_sunday`. The
/// displayed week order is driven by the `calendar-week-start` preference (see
/// [`week_days`]/[`ordered_weekday_labels`]), not this array's own order.
const WEEKDAY_LABELS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MAX_VISIBLE_EVENTS_PER_DAY: usize = 3;
const HOURS_PER_DAY: usize = 24;

/// The short label for `day` ("Sun".."Sat").
fn weekday_short(day: chrono::Weekday) -> &'static str {
    WEEKDAY_LABELS[day.num_days_from_sunday() as usize]
}

/// An occurrence's span as wall-clock times in the calendar's display zone
/// (item 3.1); all-day spans read as their dates, never zone-shifted (see
/// `calendar_tz`).
pub(crate) fn local_span(occ: &EventOccurrence) -> (NaiveDateTime, NaiveDateTime) {
    crate::calendar_tz::occurrence_span(crate::calendar_tz::display_zone(), occ.start, occ.end, occ.all_day)
}

/// Today in the calendar's display zone.
fn today() -> NaiveDate {
    crate::calendar_tz::display_zone().today()
}

/// The seven weekdays beginning at `week_start`, in week order.
fn week_days(week_start: chrono::Weekday) -> Vec<chrono::Weekday> {
    let start = crate::settings::weekday_sun_index(week_start);
    (0..7)
        .map(|i| crate::settings::weekday_from_sun_index((start + i) % 7).expect("0..=6 is a weekday"))
        .collect()
}

/// The seven weekday labels in week order, `week_start` first.
fn ordered_weekday_labels(week_start: chrono::Weekday) -> [&'static str; 7] {
    let mut labels = [""; 7];
    for (slot, day) in labels.iter_mut().zip(week_days(week_start)) {
        *slot = weekday_short(day);
    }
    labels
}
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
/// positioned by their start/end times and multi-day events can span columns).
/// The canvas paints no background of its own - the main panel's translucent
/// `@lookout-pane-bg` (and so the window background image) shows through, the
/// same as every other pane - and every tone below is the canvas's own theme
/// foreground colour at a given alpha, so the grid reads on the dark and light
/// themes alike. The business-hours stripe...
const GRID_BUSINESS_ALPHA: f64 = 0.06;
/// ...the hour and column hairlines...
const GRID_LINE_ALPHA: f64 = 0.06;
/// ...the dim-label gutter text...
const GRID_DIM_TEXT_ALPHA: f64 = 0.6;
/// ...and the highlighted slot range's fill and outline.
const GRID_SELECTION_FILL_ALPHA: f64 = 0.14;
const GRID_SELECTION_STROKE_ALPHA: f64 = 0.3;
/// Pixels of slack the text baseline sits above a chip's vertical centre.
const CHIP_TEXT_BASELINE_OFFSET: f64 = 0.36;
/// Pointer travel (pixels) before a press on an event chip becomes a drag
/// rather than a click - the same jitter tolerance the slot selection uses.
const DRAG_THRESHOLD: f64 = 8.0;
/// How long a drag-free single click on a month chip waits before it is
/// treated as a peek rather than the first half of a double-click (UX-56).
const DOUBLE_CLICK_DELAY_MS: u64 = 250;
/// Bottom-edge strip of a timed chip (pixels) that grabs a resize instead of
/// a move.
const RESIZE_GRAB_HEIGHT: f64 = 8.0;
/// The snap grid for dragged timed chips: half hours, like slot selection.
const DRAG_SNAP_MINUTES: i64 = 30;

/// A callback fired when the user activates (clicks) an event in any view, so
/// the caller can open its editor. Multiple views share one subscription: the
/// `CalendarMain`-level `connect_event_activated` hands the callback down to
/// every sub-view's render pass.
type ActivateEvent = Rc<dyn Fn(EventOccurrence)>;

/// One entry in a widget's pending activation callback: set by the current
/// render pass from `CalendarMain::connect_event_activated`, consumed by the
/// widget's click handling.
type PendingActivate = Rc<RefCell<Option<ActivateEvent>>>;

/// A callback fired when the user single-clicks an event chip in the Month
/// grid (UX-56), receiving the clicked occurrence and the chip widget it was
/// clicked on (so the caller can anchor a peek popover to it). A double-click
/// instead fires [`ActivateEvent`] and opens the full editor.
type PeekEvent = Rc<dyn Fn(EventOccurrence, gtk::Widget)>;

/// One entry in a month grid's pending peek callback: set by each
/// `set_month_occurrences` render pass from `CalendarMain::connect_event_peeked`.
type PendingPeek = Rc<RefCell<Option<PeekEvent>>>;

/// A callback fired when the user activates a highlighted slot range in a
/// Day/Week time grid (a click inside the selection), so the caller can open
/// a new-event editor spanning that exact time. Receives the normalized range
/// as `(start_date, start_minutes, end_date, end_minutes)` - minutes since
/// local midnight, snapped to half hours, end inclusive (an end of 11:00
/// means the highlight runs through the 11:00-11:30 slot).
type SlotActivate = Rc<dyn Fn(NaiveDate, i64, NaiveDate, i64)>;

/// One entry in a widget's pending slot-activation callback: set by the
/// current render pass from `CalendarMain::connect_slot_activated`, consumed
/// by the canvas's click handling.
type PendingSlotActivate = Rc<RefCell<Option<SlotActivate>>>;

/// A callback fired when the user clicks an already-selected day cell in a
/// month grid, so the caller can open a new-event editor for that day.
type DayActivate = Rc<dyn Fn(NaiveDate)>;

/// One entry in a month grid's pending day-activation callback, set once at
/// registration time from `CalendarMain::connect_main_day_activated`.
type PendingDayActivate = Rc<RefCell<Option<DayActivate>>>;

/// A callback fired when the user drags an event chip to a new time in a
/// Day/Week/Work week or Month grid, receiving the dragged occurrence, its
/// resolved new start/end (UTC), and whether the drag was a copy (`true` when
/// Ctrl was held at the press, so the caller clones the event instead of
/// moving it). The caller is responsible for persisting the change (route it
/// through the event editor's save path).
type EventDrag = Rc<dyn Fn(EventOccurrence, DateTime<Utc>, DateTime<Utc>, bool)>;

/// One entry in a widget's pending drag callback: set by the current render
/// pass from `CalendarMain::connect_event_dragged`.
type PendingEventDrag = Rc<RefCell<Option<EventDrag>>>;

/// A callback fired when the user right-clicks an event (item 3.12 / CAL-31),
/// receiving the clicked occurrence, the widget to anchor the menu to, and -
/// for a Cairo-drawn surface (the time grids and the Schedule view) - the
/// click point in that widget's coordinates, so the menu pops up at the
/// pointer. `None` points at the anchor widget as a whole (a chip or row).
type EventContext = Rc<dyn Fn(EventOccurrence, gtk::Widget, Option<(f64, f64)>)>;

/// One entry in a widget's pending context-menu callback: shared by every
/// sub-view (set once from `CalendarMain::connect_event_context_menu`).
type PendingContext = Rc<RefCell<Option<EventContext>>>;

/// How a chip drag changes the event: move shifts both ends together, resize
/// keeps the start and follows the pointer with the end.
#[derive(Clone, Copy, Debug, PartialEq)]
enum DragMode {
    Move,
    ResizeEnd,
}

/// A chip drag in flight on a time-grid canvas. Positions are stored as
/// *absolute grid minutes*: minutes since local midnight of the grid's first
/// day column (`column * 1440 + minutes`), so a drag across day columns is a
/// plain integer shift and the half-hour snap stays aligned across midnight.
/// All-day chips occupy whole 1440-minute days.
#[derive(Clone, Copy, Debug)]
struct TimeGridDrag {
    /// Index into `chips` of the dragged chip.
    chip: usize,
    mode: DragMode,
    /// The dragged occurrence's original span, in absolute grid minutes.
    original_start: i64,
    original_end: i64,
    /// The live span, in absolute grid minutes (snapped, clamped to the grid).
    live_start: i64,
    live_end: i64,
    /// The pointer's offset from the chip's start at grab time (absolute grid
    /// minutes), preserved across a move so the chip doesn't jump to the
    /// pointer. Meaningless for a resize (the end follows the pointer).
    grab_offset: i64,
    all_day: bool,
}

/// A pending press on a time-grid event chip: the chip index, the grab mode
/// (move vs bottom-edge resize, decided at press time), and the press point.
type ChipPress = Option<(usize, DragMode, f64, f64)>;

/// A chip drag in flight on a month grid: the occurrence being dragged, the
/// chip button itself (re-parented between cells as the pointer moves), and
/// the day cell it started in plus the live target date under the pointer.
struct MonthDrag {
    occ: EventOccurrence,
    /// The chip button being dragged.
    button: gtk::Widget,
    /// The date of the day cell the chip started in (unchanged by reparenting
    /// the chip between cells mid-drag).
    from_date: NaiveDate,
    /// The date of the day cell currently under the pointer.
    to_date: NaiveDate,
    px: f64,
    py: f64,
    /// True once the pointer crossed the drag threshold (the press became a
    /// drag rather than a click).
    dragging: bool,
    /// The `n_press` of the press that armed this drag (1 for a single click,
    /// 2 for the second click of a double-click). Drives the peek-vs-editor
    /// decision on a drag-free release (UX-56).
    n_press: i32,
    /// Whether Ctrl was held at the press: the drop clones the event instead
    /// of moving it (CAL-30 / item 3.12).
    copy: bool,
}

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
        .calendar-selected-cell {
            border: 2px solid alpha(currentColor, 0.35);
        }
        .calendar-drag-target {
            border: 2px solid alpha(currentColor, 0.45);
        }
        /* The same translucent fill as every other pane (`.folder-pane`), so
           the window background image shows through every calendar view. */
        .calendar-main-background {
            background-color: @lookout-pane-bg;
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
        /* The Calendar sidebar's mini month grid keeps its normal font and
           width, but sheds the theme's default button chrome so the cells
           aren't bloated to ~36px each (16px min-width + 10px padding per
           side, the same Adwaita default `.mini-calendar-compact` exists to
           fight) - without a floor, seven cells would push the grid well past
           the sidebar's 240px width request. */
        .mini-calendar-sidebar button {
            min-width: 0;
            min-height: 0;
            padding: 4px 6px;
        }
        .mini-calendar-today {
            color: @accent_bg_color;
        }
        .calendar-account-header {
            font-weight: bold;
        }
        .calendar-toggle-label {
            font-weight: normal;
        }
        /* The colour picker's swatches (the calendar row menu's Change
           colour submenu). The background comes from a per-colour
           `.calendar-swatch-<hex>` rule the checklist writes into
           `check_colors`; this is the shared chrome. */
        .calendar-color-swatch {
            min-width: 18px;
            min-height: 18px;
            padding: 0;
            border-radius: 4px;
            border: 1px solid alpha(currentColor, 0.25);
        }
        .calendar-color-swatch:hover {
            border-color: @accent_bg_color;
        }
        /* Month-grid event chips are `Gtk.Button`s so they can open an editor;
           strip the button chrome so they render as the plain colored chips the
           `.calendar-event-<hex>` rule draws. */
        .calendar-event-chip-button {
            padding: 0;
            min-width: 0;
            min-height: 0;
        }",
    );
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(&display, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    }
}

/// A single 6-week, Sunday-first month grid. Deliberately dumb and read-only:
/// no drag/resize/creation, no per-event click handling, and no header row -
/// the header (Today/prev/next + current-month dropdown) is owned by
/// [`CalendarMain`], which
/// shares it across every view. Kept as plain data-in/widget-state-out
/// functions (`set_month`/`set_month_occurrences`), mirroring
/// `folder_tree.rs`'s `build_multi_account_tree_model` precedent, so the
/// date-bucketing logic stays testable independent of a running GTK main loop.
pub struct MonthGrid {
    pub root: gtk::Widget,
    day_cells: Vec<DayCell>,
    anchor_month: Rc<RefCell<NaiveDate>>,
    on_activate: PendingActivate,
    /// The single-click peek callback, set by each `set_month_occurrences`
    /// render pass (see [`connect_event_peeked`]); `on_activate` is the
    /// double-click full-editor callback.
    on_peek: PendingPeek,
    /// The drag-reschedule callback, set by each `set_month_occurrences`
    /// render pass (see [`connect_event_dragged`]).
    on_drag: PendingEventDrag,
    /// A chip drag in flight, if any (see [`MonthDrag`]).
    month_drag: Rc<RefCell<Option<MonthDrag>>>,
    /// The `lookout-occ` data the chips normally carry. Rebuilt by every
    /// `set_month_occurrences` render pass alongside the chips themselves.
    chip_events: Rc<RefCell<HashMap<gtk::Widget, EventOccurrence>>>,
    /// Day-selection callbacks, fired when a day cell is clicked (see
    /// [`connect_main_day_selected`]). Registered once at build time via
    /// `CalendarMain::connect_main_day_selected`, which forwards to the main
    /// and Split-view grids alike.
    on_day_selected: DaySelectedCallbacks,
    /// The day cell currently highlighted by a first click - a second click
    /// on it fires `on_day_activate` instead of re-selecting (see
    /// [`connect_main_day_activated`]).
    selected_day: Rc<RefCell<Option<NaiveDate>>>,
    on_day_activate: PendingDayActivate,
    /// First day of the week for this grid (the `calendar-week-start`
    /// preference), used by [`first_grid_day`] to place the 6-week window.
    /// Shared with the click/drag closures so [`set_month_week_start`]
    /// applies a change live (item 3.1).
    week_start: Rc<Cell<chrono::Weekday>>,
    /// The weekday header labels, relabelled on a week-start change.
    weekday_labels: Vec<gtk::Label>,
}

fn build_month_grid(week_start: chrono::Weekday, on_context: &PendingContext) -> MonthGrid {
    let week_start = Rc::new(Cell::new(week_start));
    let header_row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(1).homogeneous(true).build();
    let mut weekday_labels = Vec::with_capacity(7);
    for label in ordered_weekday_labels(week_start.get()) {
        let weekday_label = gtk::Label::builder().label(label).css_classes(["dim-label", "caption-heading"]).build();
        let cell = gtk::Box::builder().css_classes(["calendar-day-cell"]).hexpand(true).build();
        cell.append(&weekday_label);
        header_row.append(&cell);
        weekday_labels.push(weekday_label);
    }

    let grid = gtk::Grid::builder()
        .row_homogeneous(true)
        .column_homogeneous(true)
        .row_spacing(1)
        .column_spacing(1)
        .vexpand(true)
        .hexpand(true)
        .build();

    let mut day_cells = Vec::with_capacity(42);
    let anchor_month = Rc::new(RefCell::new(first_of_month(today())));
    let on_day_selected: DaySelectedCallbacks = Rc::new(RefCell::new(Vec::new()));
    let selected_day: Rc<RefCell<Option<NaiveDate>>> = Rc::new(RefCell::new(None));
    let on_day_activate: PendingDayActivate = Rc::new(RefCell::new(None));
    // Chips register themselves here as they render (see
    // `clickable_event_label`); the day-cell gesture below reads it to tell a
    // chip click from a bare-cell click, so it's created before the loop.
    let chip_events: Rc<RefCell<HashMap<gtk::Widget, EventOccurrence>>> = Rc::new(RefCell::new(HashMap::new()));
    for index in 0..42usize {
        let row = index / 7;
        let col = index % 7;
        let date_label = gtk::Label::builder().xalign(0.0).margin_start(4).margin_top(2).css_classes(["caption"]).build();
        let events_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(1).vexpand(true).build();
        let container = gtk::Box::builder().orientation(gtk::Orientation::Vertical).css_classes(["calendar-day-cell"]).build();
        container.append(&date_label);
        container.append(&events_box);
        grid.attach(&container, col as i32, row as i32, 1, 1);
        // Clicking a day cell selects it and re-anchors every view to that
        // date (the same action the sidebar mini-calendar's day buttons take);
        // clicking the already-selected cell again opens a new-event editor
        // for that day. Clicks landing on an event chip are ignored here - the
        // month-grid gesture (see `attach_month_drag`) owns chip clicks and
        // opens the peek/editor, so a chip click never also jumps the anchor.
        {
            let click_target = container.clone();
            let anchor_month = anchor_month.clone();
            let week_start = week_start.clone();
            let on_day_selected = on_day_selected.clone();
            let selected_day = selected_day.clone();
            let on_day_activate = on_day_activate.clone();
            let chip_events = chip_events.clone();
            let gesture = gtk::GestureClick::new();
            // Primary only, so a right-click on a chip reaches
            // `attach_month_context` rather than selecting the day cell.
            gesture.set_button(gtk::gdk::BUTTON_PRIMARY);
            gesture.connect_pressed(move |_, _, x, y| {
                let mut widget = click_target.pick(x, y, gtk::PickFlags::DEFAULT);
                while let Some(current) = widget {
                    if chip_events.borrow().contains_key(&current) {
                        return;
                    }
                    widget = current.parent();
                }
                let date = first_grid_day(*anchor_month.borrow(), week_start.get()) + chrono::Duration::days(index as i64);
                if *selected_day.borrow() == Some(date) {
                    *selected_day.borrow_mut() = None;
                    click_target.remove_css_class("calendar-selected-cell");
                    if let Some(callback) = on_day_activate.borrow().as_ref() {
                        callback(date);
                    }
                } else {
                    *selected_day.borrow_mut() = Some(date);
                    for callback in on_day_selected.borrow().iter() {
                        callback(date);
                    }
                }
            });
            container.add_controller(gesture);
        }
        day_cells.push(DayCell {
            container,
            date_label,
            events_box,
        });
    }

    let root_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(1).vexpand(true).hexpand(true).build();
    root_box.append(&header_row);
    root_box.append(&grid);

    let on_activate: PendingActivate = Rc::new(RefCell::new(None));
    let month_drag: Rc<RefCell<Option<MonthDrag>>> = Rc::new(RefCell::new(None));
    let on_drag: PendingEventDrag = Rc::new(RefCell::new(None));
    let on_peek: PendingPeek = Rc::new(RefCell::new(None));
    attach_month_drag(&grid, &day_cells, &anchor_month, &month_drag, &chip_events, &on_activate, &on_drag, &on_peek, &week_start);
    attach_month_context(&grid, &chip_events, on_context);

    MonthGrid {
        root: root_box.upcast(),
        day_cells,
        anchor_month,
        on_activate,
        on_peek,
        on_drag,
        month_drag,
        chip_events,
        on_day_selected,
        selected_day,
        on_day_activate,
        week_start,
        weekday_labels,
    }
}

/// A right-click on an event chip opens the event context menu (item 3.12 /
/// CAL-31), anchored to the chip. Kept separate from the primary-button drag
/// gesture so a secondary click never arms a drag or the queued peek.
fn attach_month_context(grid: &gtk::Grid, chip_events: &Rc<RefCell<HashMap<gtk::Widget, EventOccurrence>>>, on_context: &PendingContext) {
    let gesture = gtk::GestureClick::new();
    gesture.set_button(gtk::gdk::BUTTON_SECONDARY);
    {
        let grid = grid.clone();
        let chip_events = chip_events.clone();
        let on_context = on_context.clone();
        gesture.connect_pressed(move |_, _, x, y| {
            let mut current = grid.pick(x, y, gtk::PickFlags::DEFAULT);
            let mut occ = None;
            while let Some(w) = current.as_ref() {
                if let Some(found) = chip_events.borrow().get(w) {
                    occ = Some(found.clone());
                    break;
                }
                current = w.parent();
            }
            let (Some(occ), Some(chip)) = (occ, current) else { return };
            if let Some(callback) = on_context.borrow().as_ref() {
                callback(occ, chip, None);
            }
        });
    }
    grid.add_controller(gesture);
}

/// Changes the grid's first weekday (item 3.1): relabels the weekday header
/// and re-lays the current month's 6-week window. The caller re-renders the
/// month's events afterwards.
pub fn set_month_week_start(mg: &MonthGrid, week_start: chrono::Weekday) {
    mg.week_start.set(week_start);
    for (label, text) in mg.weekday_labels.iter().zip(ordered_weekday_labels(week_start)) {
        label.set_label(text);
    }
    let month = *mg.anchor_month.borrow();
    set_month(mg, month);
}

/// Rebuilds the grid's date labels/highlighting for the month containing
/// `month` and clears every cell's event list (a subsequent
/// `set_month_occurrences` call repopulates them).
pub fn set_month(mg: &MonthGrid, month: NaiveDate) {
    let month = first_of_month(month);
    *mg.anchor_month.borrow_mut() = month;

    let today = today();
    let selected_day = *mg.selected_day.borrow();
    let grid_start = first_grid_day(month, mg.week_start.get());

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
        if selected_day == Some(date) {
            cell.container.add_css_class("calendar-selected-cell");
        } else {
            cell.container.remove_css_class("calendar-selected-cell");
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
pub fn set_month_occurrences(
    mg: &MonthGrid,
    occurrences: &[EventOccurrence],
    colors: &HashMap<CalendarId, String>,
    on_activate: Option<ActivateEvent>,
    on_drag: Option<EventDrag>,
    on_peek: Option<PeekEvent>,
) {
    *mg.on_activate.borrow_mut() = on_activate;
    *mg.on_drag.borrow_mut() = on_drag;
    *mg.on_peek.borrow_mut() = on_peek;
    // A re-render rebuilds every chip, so any in-flight drag is stale.
    *mg.month_drag.borrow_mut() = None;
    mg.chip_events.borrow_mut().clear();
    for cell in &mg.day_cells {
        cell.container.remove_css_class("calendar-drag-target");
    }
    let grid_start = first_grid_day(*mg.anchor_month.borrow(), mg.week_start.get());

    for cell in &mg.day_cells {
        clear_children(&cell.events_box);
    }

    let by_date = bucket_by_date(occurrences, grid_start, mg.day_cells.len());

    for (i, cell) in mg.day_cells.iter().enumerate() {
        let date = grid_start + chrono::Duration::days(i as i64);
        let Some(day_occurrences) = by_date.get(&date) else { continue };
        for occ in &day_occurrences[..day_occurrences.len().min(MAX_VISIBLE_EVENTS_PER_DAY)] {
            let label = event_label(occ, colors);
            let interactive = mg.on_activate.borrow().is_some() || mg.on_peek.borrow().is_some() || mg.on_drag.borrow().is_some();
            if interactive {
                cell.events_box.append(&clickable_event_label(label, occ, mg));
            } else {
                cell.events_box.append(&label);
            }
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
    let (start_local, end_local) = local_span(occ);
    let start_date = start_local.date();
    let end_date = end_local.date();

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

/// The 0-based index into `day_cells` (row-major, Sunday-first) of the month
/// grid cell under the grid-relative point `(x, y)`, or `None` for points
/// outside the grid. The grid's seven columns and six week rows are all
/// homogeneous. Pure - the caller passes the grid's allocated size.
fn cell_index_at_point(width: f64, height: f64, x: f64, y: f64) -> Option<usize> {
    if width <= 0.0 || height <= 0.0 || x < 0.0 || y < 0.0 || x >= width || y >= height {
        return None;
    }
    let col = (x / (width / 7.0)) as usize;
    let row = (y / (height / 6.0)) as usize;
    Some(row * 7 + col)
}

/// Attaches the month grid's chip gesture: a press landing on a chip
/// (identified by its entry in `chip_events`) arms a potential drag, motion
/// beyond [`DRAG_THRESHOLD`] turns it into a drag across day cells (the chip
/// is live re-parented between cells as it moves, with the target cell
/// highlighted), and a drag-free release either peeks (single click, UX-56) or
/// opens the editor (double click). Only the grabbed chip follows the pointer
/// live - the sibling chips of a multi-day event in other cells catch up when
/// the resync after the drop lands. Coordinates stay grid-relative for the
/// gesture's whole lifetime, so reparenting the chip never skews them.
///
/// Single vs double click: the first drag-free release cannot know whether a
/// second click is coming, so it schedules the peek after
/// [`DOUBLE_CLICK_DELAY_MS`]; a second press cancels the timer and the second
/// release opens the editor. A drag cancels the peek outright.
#[allow(clippy::too_many_arguments)]
fn attach_month_drag(
    grid: &gtk::Grid,
    day_cells: &[DayCell],
    anchor_month: &Rc<RefCell<NaiveDate>>,
    month_drag: &Rc<RefCell<Option<MonthDrag>>>,
    chip_events: &Rc<RefCell<HashMap<gtk::Widget, EventOccurrence>>>,
    on_activate: &PendingActivate,
    on_drag: &PendingEventDrag,
    on_peek: &PendingPeek,
    week_start: &Rc<Cell<chrono::Weekday>>,
) {
    let gesture = gtk::GestureClick::new();
    // Primary only: a secondary click is the context menu's (see
    // `attach_month_context`), and must not arm a drag or the queued peek.
    gesture.set_button(gtk::gdk::BUTTON_PRIMARY);
    let events_boxes: Vec<gtk::Box> = day_cells.iter().map(|c| c.events_box.clone()).collect();
    let containers: Vec<gtk::Box> = day_cells.iter().map(|c| c.container.clone()).collect();
    // The queued single-click peek, if any (cancelled by a second press).
    let pending_peek: Rc<RefCell<Option<gtk::glib::SourceId>>> = Rc::new(RefCell::new(None));
    {
        let grid = grid.clone();
        let anchor_month = anchor_month.clone();
        let month_drag = month_drag.clone();
        let chip_events = chip_events.clone();
        let pending_peek = pending_peek.clone();
        let week_start = week_start.clone();
        gesture.connect_pressed(move |gesture, n_press, x, y| {
            // A second press means this is a double-click: cancel the peek the
            // first release queued so the editor wins.
            if n_press >= 2 {
                if let Some(id) = pending_peek.borrow_mut().take() {
                    id.remove();
                }
            }
            // Ctrl at press turns the drop into a copy (CAL-30).
            let copy = gesture.current_event_state().contains(gtk::gdk::ModifierType::CONTROL_MASK);
            // Only presses that land on a chip arm the drag; the day cells'
            // own gestures handle everything else.
            let mut current = grid.pick(x, y, gtk::PickFlags::DEFAULT);
            let mut occ = None;
            while let Some(w) = current.as_ref() {
                if let Some(found) = chip_events.borrow().get(w) {
                    occ = Some(found.clone());
                    break;
                }
                current = w.parent();
            }
            let Some(occ) = occ else { return };
            let Some(chip) = current else { return };
            let Some(cell) = cell_index_at_point(grid.width() as f64, grid.height() as f64, x, y) else {
                return;
            };
            let date = first_grid_day(*anchor_month.borrow(), week_start.get()) + chrono::Duration::days(cell as i64);
            *month_drag.borrow_mut() = Some(MonthDrag {
                occ,
                button: chip,
                from_date: date,
                to_date: date,
                px: x,
                py: y,
                dragging: false,
                n_press,
                copy,
            });
        });
    }
    {
        let grid = grid.clone();
        let anchor_month = anchor_month.clone();
        let month_drag = month_drag.clone();
        let events_boxes = events_boxes.clone();
        let containers = containers.clone();
        let week_start = week_start.clone();
        gesture.connect_update(move |gesture, sequence| {
            let Some((x, y)) = gesture.point(sequence) else { return };
            let mut guard = month_drag.borrow_mut();
            let Some(d) = guard.as_mut() else { return };
            let dx = x - d.px;
            let dy = y - d.py;
            if !d.dragging {
                if dx * dx + dy * dy < DRAG_THRESHOLD * DRAG_THRESHOLD {
                    return;
                }
                d.dragging = true;
            }
            let Some(cell) = cell_index_at_point(grid.width() as f64, grid.height() as f64, x, y) else {
                return;
            };
            let date = first_grid_day(*anchor_month.borrow(), week_start.get()) + chrono::Duration::days(cell as i64);
            if d.to_date != date {
                d.to_date = date;
                // Live feedback: reparent the chip into the target cell and
                // highlight it.
                let target = &events_boxes[cell];
                if d.button.parent().is_none_or(|parent| parent != *target.upcast_ref::<gtk::Widget>()) {
                    d.button.unparent();
                    target.append(&d.button);
                }
                for (i, container) in containers.iter().enumerate() {
                    if i == cell {
                        container.add_css_class("calendar-drag-target");
                    } else {
                        container.remove_css_class("calendar-drag-target");
                    }
                }
            }
        });
    }
    {
        let month_drag = month_drag.clone();
        let on_activate = on_activate.clone();
        let on_drag = on_drag.clone();
        let on_peek = on_peek.clone();
        let pending_peek = pending_peek.clone();
        gesture.connect_released(move |_, _, _, _| {
            let drag = month_drag.borrow_mut().take();
            for container in &containers {
                container.remove_css_class("calendar-drag-target");
            }
            let Some(d) = drag else { return };
            if !d.dragging {
                // A double-click opens the editor now; a single click is
                // queued as a peek in case a second click follows.
                if d.n_press >= 2 {
                    if let Some(callback) = on_activate.borrow().as_ref() {
                        callback(d.occ);
                    }
                    return;
                }
                let occ = d.occ.clone();
                let anchor = d.button;
                let on_peek = on_peek.clone();
                let pending_peek_clear = pending_peek.clone();
                let id = gtk::glib::timeout_add_local_once(std::time::Duration::from_millis(DOUBLE_CLICK_DELAY_MS), move || {
                    *pending_peek_clear.borrow_mut() = None;
                    // A re-render (or a new month) may have dropped the chip
                    // while the timer was pending; there's nothing to anchor to
                    // then, so skip the peek.
                    if anchor.parent().is_none() {
                        return;
                    }
                    if let Some(callback) = on_peek.borrow().as_ref() {
                        callback(occ, anchor);
                    }
                });
                *pending_peek.borrow_mut() = Some(id);
                return;
            }
            // A drop: shift the event by the day-cell delta (only the grabbed
            // chip has been reparented live; the resync repaints everything).
            let shift = (d.to_date - d.from_date).num_days();
            let new_start = d.occ.start + chrono::Duration::days(shift);
            let new_end = d.occ.end + chrono::Duration::days(shift);
            if let Some(callback) = on_drag.borrow().as_ref() {
                callback(d.occ, new_start, new_end, d.copy);
            }
        });
    }
    grid.add_controller(gesture);
}

/// Wraps a month-grid event chip's label in a plain clickable box and
/// registers it (with its occurrence) in `chip_events` - the lookup the
/// month-grid gesture uses to find what the pointer hit. The grid gesture
/// (see `attach_month_drag`) owns press/click handling: a drag-free single
/// click peeks, a double-click opens the editor, and a drag reschedules.
///
/// A `gtk::Box` rather than a `gtk::Button` on purpose: a button's own click
/// gesture claims the pointer sequence, which starves the ancestor grid's
/// gesture of the press/release events it needs (so nothing happened at all).
/// A box has no competing gesture, so the grid gesture sees every click. The
/// label itself paints the coloured chip; `.calendar-event-chip-button` keeps
/// the old chrome-strip rule harmless.
fn clickable_event_label(label: gtk::Label, occ: &EventOccurrence, mg: &MonthGrid) -> gtk::Widget {
    let chip = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .css_classes(["calendar-event-chip-button", "calendar-event-chip"])
        .halign(gtk::Align::Fill)
        .build();
    chip.set_cursor_from_name(Some("pointer"));
    // The label's own background is the coloured chip, so it must fill the
    // cell-wide chip box (a `gtk::Button` did this implicitly; a plain
    // child of the horizontal box would otherwise stop at its text width).
    label.set_hexpand(true);
    label.set_halign(gtk::Align::Fill);
    chip.append(&label);
    mg.chip_events.borrow_mut().insert(chip.clone().upcast(), occ.clone());
    chip.upcast()
}

/// The one-line "when" text for the event peek: "All day · Mon 12 Aug" for an
/// all-day event, "Mon 12 Aug 2025 · 9:00am – 10:00am" within a day, or a
/// cross-day range.
fn peek_when_text(occ: &EventOccurrence) -> String {
    let (start, end) = local_span(occ);
    let start_date = start.date();
    if occ.all_day {
        let last = (end - chrono::Duration::days(1)).date().max(start_date);
        return if last > start_date {
            format!("All day · {} – {}", start_date.format("%a %d %b"), last.format("%a %d %b"))
        } else {
            format!("All day · {}", start_date.format("%a %d %b %Y"))
        };
    }
    if start.date() == end.date() {
        format!("{} · {} – {}", start_date.format("%a %d %b %Y"), format_event_time(&start), format_event_time(&end))
    } else {
        format!(
            "{} {} – {} {}",
            start.format("%a %d %b"),
            format_event_time(&start),
            end.format("%a %d %b"),
            format_event_time(&end)
        )
    }
}

/// A wrapped, left-aligned detail line for the event peek.
fn peek_detail(text: &str) -> gtk::Label {
    gtk::Label::builder().label(text).xalign(0.0).wrap(true).css_classes(["caption"]).build()
}

/// Builds the Month/Split event peek popover (UX-56): a compact read-only card
/// shown on a single chip click, with Edit/Delete/Join actions supplied by the
/// caller. The caller parents it to the clicked chip and pops it up; every
/// action pops it back down first.
pub fn build_event_peek(occ: &EventOccurrence, colors: &HashMap<CalendarId, String>, on_edit: Rc<dyn Fn()>, on_delete: Option<Rc<dyn Fn()>>) -> gtk::Popover {
    let popover = gtk::Popover::builder().autohide(true).has_arrow(true).build();
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .width_request(320)
        .build();

    // Title, led by a dot in the owning calendar's colour.
    let title_row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).build();
    let color = colors.get(&occ.calendar_id).map(String::as_str).unwrap_or("#808080");
    let swatch = gtk::Label::builder()
        .label(format!("<span foreground=\"{}\" size=\"large\">●</span>", gtk::glib::markup_escape_text(color)))
        .use_markup(true)
        .valign(gtk::Align::Start)
        .build();
    let title = gtk::Label::builder()
        .label(occ.summary.as_deref().unwrap_or("(untitled)"))
        .css_classes(["title-4"])
        .xalign(0.0)
        .wrap(true)
        .hexpand(true)
        .build();
    title_row.append(&swatch);
    title_row.append(&title);
    content.append(&title_row);

    content.append(&peek_detail(&peek_when_text(occ)));
    if occ.rrule.is_some() {
        content.append(&peek_detail("Recurring event"));
    }
    if let Some(location) = occ.location.as_deref().filter(|l| !l.trim().is_empty()) {
        content.append(&peek_detail(&format!("Where: {location}")));
    }
    if let Some(organizer) = &occ.organizer {
        // The organizer's name/address is the sensitive part of this line.
        let organizer_row = peek_detail(&format!("Organizer: {}", organizer.display_label()));
        crate::privacy::mark(&organizer_row);
        content.append(&organizer_row);
    }
    let attendee_count = occ.attendees.len();
    if attendee_count > 0 {
        content.append(&peek_detail(&format!("{attendee_count} attendee{}", if attendee_count == 1 { "" } else { "s" })));
    }

    let buttons = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .halign(gtk::Align::End)
        .margin_top(6)
        .build();
    let edit_button = gtk::Button::builder().label("Edit").css_classes(["suggested-action"]).build();
    {
        let popover = popover.clone();
        edit_button.connect_clicked(move |_| {
            popover.popdown();
            on_edit();
        });
    }
    buttons.append(&edit_button);
    // CAL-20 / 3.17: a Join button for an online meeting, labelled by provider.
    if let Some(meeting) = lookout_core::interop::onlinemeeting::detect(occ.conference_url.as_deref(), occ.location.as_deref(), occ.description.as_deref()) {
        let label = format!("Join {}", meeting.provider.display_name());
        let join_button = gtk::LinkButton::builder().uri(&meeting.url).label(&label).css_classes(["pill"]).build();
        buttons.append(&join_button);
    }
    if let Some(on_delete) = on_delete {
        let delete_button = gtk::Button::builder().label("Delete").css_classes(["destructive-action"]).build();
        {
            let popover = popover.clone();
            delete_button.connect_clicked(move |_| {
                popover.popdown();
                on_delete();
            });
        }
        buttons.append(&delete_button);
    }
    content.append(&buttons);

    popover.set_child(Some(&content));
    popover
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
    /// The `canvas`'s scroller, retained so callers outside this module (the
    /// event editor's reused Day-view preview) can scroll it to an arbitrary
    /// time via [`scroll_time_grid_to_minutes`], not just "now".
    scroller: gtk::ScrolledWindow,
    /// Weekday/date header labels above the day columns (Week/Work week only;
    /// the Day view's date lives in [`CalendarMain`]'s shared header).
    /// Rebuilt by [`TimeGrid::set_weekdays`], since a Work week change can
    /// change the column count.
    headers: Rc<RefCell<Vec<gtk::Label>>>,
    /// The header row the `headers` live in (Week/Work week only).
    header_row: Option<gtk::Box>,
    /// The blank header cell above the hour gutter (Week/Work week only),
    /// resized with the gutter by [`TimeGrid::set_zones`].
    gutter_spacer: Option<gtk::Label>,
    anchor: Rc<RefCell<NaiveDate>>,
    /// Which weekdays make up the columns. The Day view is the single-column
    /// special case (`day_view`), which always shows the anchor date itself.
    weekdays: Rc<RefCell<Vec<chrono::Weekday>>>,
    day_view: bool,
    data: TimeGridData,
    /// The click-to-edit callback, set by each `set_time_grid` render pass.
    on_activate: PendingActivate,
    /// The click-a-time-slot-to-create callback, set by each `set_time_grid`
    /// render pass (see [`SlotActivate`]).
    on_slot_activate: PendingSlotActivate,
    /// The drag-reschedule callback, set by each `set_time_grid` render pass
    /// (see [`connect_event_dragged`]).
    on_drag: PendingEventDrag,
}

/// The per-grid render state shared with the draw/hover closures.
/// A selected span of time-grid slots: from `start` to `end` inclusive, with
/// `minutes` being the snapped half-hour start of each boundary slot (an end
/// of `11:00` means the highlight runs through the 11:00-11:30 slot). Always
/// normalized so `(start_date, start_minutes) <= (end_date, end_minutes)`,
/// so multi-column ranges paint left-to-right regardless of drag direction.
#[derive(Clone, Copy, PartialEq, Debug)]
struct SlotRange {
    start_date: NaiveDate,
    start_minutes: i64,
    end_date: NaiveDate,
    end_minutes: i64,
}

impl SlotRange {
    /// The normalized range covering the single slot `(date, minutes)`.
    fn single(date: NaiveDate, minutes: i64) -> Self {
        SlotRange {
            start_date: date,
            start_minutes: minutes,
            end_date: date,
            end_minutes: minutes,
        }
    }

    /// The normalized range spanning the slots `(a_date, a_minutes)` through
    /// `(b_date, b_minutes)`, in either drag direction.
    fn spanning(a_date: NaiveDate, a_minutes: i64, b_date: NaiveDate, b_minutes: i64) -> Self {
        let a = (a_date, a_minutes);
        let b = (b_date, b_minutes);
        let (start, end) = if a <= b { (a, b) } else { (b, a) };
        SlotRange {
            start_date: start.0,
            start_minutes: start.1,
            end_date: end.0,
            end_minutes: end.1,
        }
    }

    /// Whether the snapped slot `(date, minutes)` falls inside the range.
    fn contains(&self, date: NaiveDate, minutes: i64) -> bool {
        (self.start_date, self.start_minutes) <= (date, minutes) && (date, minutes) <= (self.end_date, self.end_minutes)
    }
}

#[derive(Clone)]
struct TimeGridData {
    occurrences: Rc<RefCell<Vec<EventOccurrence>>>,
    colors: Rc<RefCell<HashMap<CalendarId, String>>>,
    /// The consecutive local dates currently displayed, one per column.
    dates: Rc<RefCell<Vec<NaiveDate>>>,
    chips: Rc<RefCell<Vec<TimeChip>>>,
    /// The slot range currently highlighted (by a click, or by a click-and-
    /// drag). A click inside the highlighted range fires the slot-activation
    /// callback; cleared by every `set_time_grid` render pass.
    selection: Rc<RefCell<Option<SlotRange>>>,
    /// True while the pointer button is held after pressing on an empty slot.
    drag_active: Rc<Cell<bool>>,
    /// The slot the drag started from, used to distinguish a jittery click
    /// from a real drag and to anchor the range while dragging.
    drag_anchor: Rc<RefCell<Option<(NaiveDate, i64)>>>,
    /// Whether Ctrl was held when the armed chip press began: the drop clones
    /// the event instead of moving it (CAL-30 / item 3.12).
    drag_copy: Rc<Cell<bool>>,
    /// A pending press on an event chip: `(chip index, grab mode, press x,
    /// press y)`. Armed by `attach_click`'s pressed handler; a release without
    /// further movement activates the event, movement beyond the drag
    /// threshold starts a chip drag (see `drag`).
    chip_press: Rc<RefCell<ChipPress>>,
    /// The chip drag currently in flight, if any - set once a chip press
    /// crosses the drag threshold, cleared on release or re-render.
    drag: Rc<RefCell<Option<TimeGridDrag>>>,
    /// The business-hours band painted by [`paint_time_grid`], as local hours,
    /// half-open `start..end`. Defaults to 8..18; [`TimeGrid::set_work_hours`]
    /// sets it from the `CalendarPreferences`. Shared (`Rc`) with the paint
    /// closures - a plain `Cell` here was copied into each closure when the
    /// grid was built, so the configured hours never reached the painter.
    work_start: Rc<Cell<usize>>,
    work_end: Rc<Cell<usize>>,
    /// The weekdays that get the business-hours band (item 3.1): a weekend
    /// column in Week view, or the Day view on a day off, shows none.
    work_days: Rc<RefCell<Vec<chrono::Weekday>>>,
    /// Total width of the hour gutter left of the day columns: one
    /// [`HOUR_GUTTER_WIDTH`] column, or two when a second time zone is shown.
    gutter: Rc<Cell<f64>>,
    /// The second time zone (CAL-10) whose hours are labelled in the outer
    /// gutter column, if any. See [`TimeGrid::set_zones`].
    secondary_zone: Rc<Cell<Option<chrono_tz::Tz>>>,
}

pub(crate) fn build_time_grid(weekdays: &[chrono::Weekday], day_view: bool, on_context: &PendingContext) -> TimeGrid {
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
        selection: Rc::new(RefCell::new(None)),
        drag_active: Rc::new(Cell::new(false)),
        drag_anchor: Rc::new(RefCell::new(None)),
        drag_copy: Rc::new(Cell::new(false)),
        chip_press: Rc::new(RefCell::new(None)),
        drag: Rc::new(RefCell::new(None)),
        work_start: Rc::new(Cell::new(8)),
        work_end: Rc::new(Cell::new(18)),
        work_days: Rc::new(RefCell::new(crate::settings::CalendarPreferences::default().work_days)),
        gutter: Rc::new(Cell::new(HOUR_GUTTER_WIDTH)),
        secondary_zone: Rc::new(Cell::new(None)),
    };
    let hover: Rc<Cell<Option<usize>>> = Rc::new(Cell::new(None));
    let on_activate: PendingActivate = Rc::new(RefCell::new(None));
    let on_slot_activate: PendingSlotActivate = Rc::new(RefCell::new(None));
    let on_drag: PendingEventDrag = Rc::new(RefCell::new(None));

    {
        let data = data.clone();
        let hover = hover.clone();
        band.set_draw_func(move |band, cr, width, height| {
            paint_all_day_band(cr, width as f64, height as f64, &data, hover.get(), foreground_rgb(band));
        });
    }
    {
        let data = data.clone();
        let hover = hover.clone();
        canvas.set_draw_func(move |canvas, cr, width, height| {
            paint_time_grid(cr, width as f64, height as f64, &data, hover.get(), foreground_rgb(canvas));
        });
    }

    attach_hover(&band, true, &data, &hover);
    attach_hover(&canvas, false, &data, &hover);
    attach_click(&band, true, &data, &on_activate, &on_slot_activate, &on_drag);
    attach_click(&canvas, false, &data, &on_activate, &on_slot_activate, &on_drag);
    attach_time_grid_context(&band, true, &data, on_context);
    attach_time_grid_context(&canvas, false, &data, on_context);

    let mut headers = Vec::new();
    let mut header_row_slot = None;
    let mut gutter_spacer = None;
    let root_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).vexpand(true).hexpand(true).build();
    if !day_view {
        let header_row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(1).homogeneous(true).build();
        let spacer = gtk::Label::builder().width_request(HOUR_GUTTER_WIDTH as i32).build();
        header_row.append(&spacer);
        gutter_spacer = Some(spacer);
        for _ in weekdays {
            let label = time_grid_header_label();
            header_row.append(&label);
            headers.push(label);
        }
        root_box.append(&header_row);
        header_row_slot = Some(header_row);
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
        scroller,
        headers: Rc::new(RefCell::new(headers)),
        header_row: header_row_slot,
        gutter_spacer,
        anchor: Rc::new(RefCell::new(today())),
        weekdays: Rc::new(RefCell::new(weekdays.to_vec())),
        day_view,
        data,
        on_activate,
        on_slot_activate,
        on_drag,
    }
}

impl TimeGrid {
    /// Sets the business-hours band (local hours, half-open `start..end`)
    /// painted on the timeline and repaints it. Called by [`build_main`] from
    /// the `CalendarPreferences`; the event editor's preview grid keeps the
    /// default 8..18.
    pub(crate) fn set_work_hours(&self, start: usize, end: usize) {
        self.data.work_start.set(start.min(23));
        self.data.work_end.set(end.clamp(1, 24));
        self.canvas.queue_draw();
    }

    /// Changes the weekdays that make up the columns (item 3.1: a new week
    /// start, or a new Work week), rebuilding the header labels to match.
    /// The caller re-renders the grid afterwards (`set_time_grid`).
    pub(crate) fn set_weekdays(&self, weekdays: &[chrono::Weekday]) {
        if weekdays.is_empty() {
            return;
        }
        *self.weekdays.borrow_mut() = weekdays.to_vec();
        if let Some(header_row) = &self.header_row {
            let mut headers = self.headers.borrow_mut();
            for label in headers.drain(..) {
                header_row.remove(&label);
            }
            for _ in weekdays {
                let label = time_grid_header_label();
                header_row.append(&label);
                headers.push(label);
            }
        }
    }

    /// Sets which weekdays get the business-hours band and repaints.
    pub(crate) fn set_work_days(&self, days: &[chrono::Weekday]) {
        *self.data.work_days.borrow_mut() = days.to_vec();
        self.canvas.queue_draw();
    }

    /// Shows (or, with `None`, hides) the second time-zone gutter (item 3.1 /
    /// CAL-10): the gutter doubles in width, the outer column labels each
    /// hour row in `secondary`, and the all-day band's gutter names both
    /// zones. The primary zone is the calendar's display zone.
    pub(crate) fn set_zones(&self, secondary: Option<chrono_tz::Tz>) {
        self.data.secondary_zone.set(secondary);
        self.data.gutter.set(if secondary.is_some() { 2.0 * HOUR_GUTTER_WIDTH } else { HOUR_GUTTER_WIDTH });
        if let Some(spacer) = &self.gutter_spacer {
            spacer.set_width_request(self.data.gutter.get() as i32);
        }
        self.band.queue_draw();
        self.canvas.queue_draw();
    }
}

/// One weekday/date header label above a Week/Work week column.
fn time_grid_header_label() -> gtk::Label {
    gtk::Label::builder().css_classes(["dim-label", "caption-heading"]).hexpand(true).build()
}

/// The second-zone label for the primary-zone hour row `hour` on `date`: the
/// same instant in `secondary`, in the gutter's compact 12-hour style - "5pm"
/// on whole-hour offsets, "2:30pm" on half-hour ones (Asia/Kolkata).
fn secondary_gutter_label(hour: u32, date: NaiveDate, primary: crate::calendar_tz::DisplayZone, secondary: chrono_tz::Tz) -> String {
    let Some(naive) = date.and_hms_opt(hour, 0, 0) else { return String::new() };
    let local = primary.resolve_local(naive).with_timezone(&secondary);
    if local.minute() == 0 {
        hour_gutter_text(local.hour() as usize)
    } else {
        format!("{}:{:02}{}", hour_12(local.hour()), local.minute(), meridiem(local.hour()))
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
            // While a drag is in flight the ghost chip owns the pointer: no
            // hover ring or tooltip for the chip under it.
            if data.drag.borrow().is_some() {
                if hover.take().is_some() {
                    canvas_widget.queue_draw();
                }
                canvas_widget.set_tooltip_text(None);
                return;
            }
            let dates_guard = data.dates.borrow();
            let chips_guard = data.chips.borrow();
            let hit = hovered_chip(data.gutter.get(), &chips_guard, canvas_widget.width() as f64, dates_guard.len(), x, y, band);
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

/// Wires a click handler onto a time-grid canvas so clicking a chip opens its
/// editor (or drags it to a new time), and empty time slots on the timeline
/// support select-then-edit: a click highlights the slot, a click-and-drag
/// extends the highlight over a whole range (across hours and day columns,
/// drawn by `paint_time_grid`), and a click inside the highlighted range fires
/// `on_slot_activate` with the snapped start/end slots (a new-event entry
/// point for the caller). Dragging tracks pointer motion via the gesture's
/// `update` signal, so a jittery click on a selected range still just
/// activates it.
///
/// A press on an event chip arms a potential drag: the event still activates
/// on a release without movement, but once the pointer moves beyond
/// [`DRAG_THRESHOLD`] the press becomes a drag instead (a move for a whole-
/// chip grab, a resize for a bottom-edge grab). The drag's live position is
/// rendered as a translucent ghost by the paint passes; release fires
/// `on_drag` with the resolved new start/end. `band` selects the split half
/// (all-day chips vs timed chips), the same convention as [`attach_hover`];
/// the hit-test is the shared [`hovered_chip`]. The callbacks come from
/// `on_activate`/`on_slot_activate`/`on_drag`, set by the latest
/// `set_time_grid` render pass.
fn attach_click(canvas: &gtk::DrawingArea, band: bool, data: &TimeGridData, on_activate: &PendingActivate, on_slot_activate: &PendingSlotActivate, on_drag: &PendingEventDrag) {
    let gesture = gtk::GestureClick::new();
    // Primary only: a secondary click is the context menu's (see
    // `attach_time_grid_context`).
    gesture.set_button(gtk::gdk::BUTTON_PRIMARY);
    {
        let canvas_widget = canvas.clone();
        let data = data.clone();
        let on_activate = on_activate.clone();
        let on_slot_activate = on_slot_activate.clone();
        let on_drag = on_drag.clone();
        // The snapped slot under `(x, y)` (timeline only - the band has no
        // slots), or `None` for gutter/band/chip hits.
        let slot_at = {
            let data = data.clone();
            let canvas_widget = canvas_widget.clone();
            move |x: f64, y: f64| -> Option<(NaiveDate, i64)> {
                let dates_guard = data.dates.borrow();
                let chips_guard = data.chips.borrow();
                if band || hovered_chip(data.gutter.get(), &chips_guard, canvas_widget.width() as f64, dates_guard.len(), x, y, band).is_some() {
                    return None;
                }
                slot_from_point(data.gutter.get(), x, y, canvas_widget.width() as f64, dates_guard.len()).map(|(col, minutes)| (dates_guard[col], minutes))
            }
        };
        // The index of the chip under `(x, y)` (timeline only; band chips are
        // activated by the same gesture through `band`).
        let chip_at = {
            let data = data.clone();
            let canvas_widget = canvas_widget.clone();
            move |x: f64, y: f64| -> Option<usize> {
                let dates_guard = data.dates.borrow();
                let chips_guard = data.chips.borrow();
                hovered_chip(data.gutter.get(), &chips_guard, canvas_widget.width() as f64, dates_guard.len(), x, y, band)
            }
        };
        let slot_at_update = slot_at.clone();
        let slot_at_release = slot_at.clone();
        let pressed_data = data.clone();
        let pressed_canvas = canvas_widget.clone();
        let update_data = data.clone();
        let update_canvas = canvas_widget.clone();
        let release_data = data.clone();
        let release_canvas = canvas_widget.clone();
        gesture.connect_pressed(move |gesture, _, x, y| {
            // Ctrl at press turns a chip drop into a copy (CAL-30).
            pressed_data.drag_copy.set(gesture.current_event_state().contains(gtk::gdk::ModifierType::CONTROL_MASK));
            if let Some(hit) = chip_at(x, y) {
                // Arm a potential chip drag. The event still activates if the
                // pointer is released without moving (see `connect_released`).
                let mode = {
                    let chips_guard = pressed_data.chips.borrow();
                    let dates_guard = pressed_data.dates.borrow();
                    let chip = &chips_guard[hit];
                    if chip.all_day {
                        DragMode::Move
                    } else {
                        let gutter = pressed_data.gutter.get();
                        let col_width = col_width_for(gutter, pressed_canvas.width() as f64, dates_guard.len());
                        let (_, cy, _, ch) = chip_geometry(gutter, chip, col_width);
                        if y >= cy + ch - RESIZE_GRAB_HEIGHT {
                            DragMode::ResizeEnd
                        } else {
                            DragMode::Move
                        }
                    }
                };
                *pressed_data.selection.borrow_mut() = None;
                *pressed_data.chip_press.borrow_mut() = Some((hit, mode, x, y));
                pressed_canvas.queue_draw();
                return;
            }
            let Some(slot) = slot_at(x, y) else { return };
            // A press inside the highlighted range is a "click the selection
            // to activate" candidate; it only opens the editor if the pointer
            // is released without dragging it into a new range.
            if pressed_data.selection.borrow().is_some_and(|range| range.contains(slot.0, slot.1)) {
                *pressed_data.drag_anchor.borrow_mut() = Some(slot);
                return;
            }
            // Otherwise begin a new selection at the pressed slot; dragging
            // extends it (see `connect_update`), releasing keeps it.
            *pressed_data.selection.borrow_mut() = Some(SlotRange::single(slot.0, slot.1));
            *pressed_data.drag_anchor.borrow_mut() = Some(slot);
            pressed_data.drag_active.set(true);
            pressed_canvas.queue_draw();
        });
        gesture.connect_update(move |gesture, sequence| {
            let Some((x, y)) = gesture.point(sequence) else { return };
            // A press that started on a chip: once the pointer moves beyond
            // the threshold the press becomes a drag (a release before that
            // still activates the event). The drag starts from the *press*
            // position, so the grab offset is preserved exactly. The pending
            // press is copied out first - an `if let` scrutinee would hold
            // the `Ref` alive for the whole body, panicking on the
            // `borrow_mut` below.
            let chip_press = *update_data.chip_press.borrow();
            if let Some((chip_idx, mode, px, py)) = chip_press {
                let dx = x - px;
                let dy = y - py;
                if dx * dx + dy * dy < DRAG_THRESHOLD * DRAG_THRESHOLD {
                    return;
                }
                *update_data.chip_press.borrow_mut() = None;
                let chip_span = {
                    let chips_guard = update_data.chips.borrow();
                    let occurrences_guard = update_data.occurrences.borrow();
                    let chip = &chips_guard[chip_idx];
                    let Some(_) = occurrences_guard.get(chip.occurrence) else { return };
                    (chip.column as i64 * 1440 + chip.start_minutes, chip.column as i64 * 1440 + chip.end_minutes, chip.all_day)
                };
                let dates_len = update_data.dates.borrow().len();
                let Some((col, minutes)) = col_minutes_at(update_data.gutter.get(), x, y, update_canvas.width() as f64, dates_len) else {
                    return;
                };
                // The grab offset comes from the *press* point, so the chip
                // keeps its grab position exactly as the drag starts.
                let grab_offset = col_minutes_at(update_data.gutter.get(), px, py, update_canvas.width() as f64, dates_len)
                    .map(|(c, m)| c as i64 * 1440 + m)
                    .unwrap_or(chip_span.0)
                    - chip_span.0;
                let mut drag = TimeGridDrag {
                    chip: chip_idx,
                    mode,
                    original_start: chip_span.0,
                    original_end: chip_span.1,
                    live_start: chip_span.0,
                    live_end: chip_span.1,
                    grab_offset,
                    all_day: chip_span.2,
                };
                drag_update(&mut drag, col as i64, minutes, dates_len as i64 * 1440);
                *update_data.drag.borrow_mut() = Some(drag);
                update_canvas.set_cursor_from_name(Some("grabbing"));
                update_canvas.queue_draw();
                return;
            }
            // An in-flight chip drag: track the pointer and repaint the ghost.
            // The drag is copied out first (same `if let` scrutinee-lifetime
            // reason as the chip press above).
            let drag_state = *update_data.drag.borrow();
            if let Some(mut drag) = drag_state {
                let dates_len = update_data.dates.borrow().len();
                let Some((col, minutes)) = col_minutes_at(update_data.gutter.get(), x, y, update_canvas.width() as f64, dates_len) else {
                    return;
                };
                drag_update(&mut drag, col as i64, minutes, dates_len as i64 * 1440);
                *update_data.drag.borrow_mut() = Some(drag);
                update_canvas.queue_draw();
                return;
            }
            let Some(slot) = slot_at_update(x, y) else { return };
            let Some(anchor) = *update_data.drag_anchor.borrow() else { return };
            if (slot.0, slot.1) == anchor {
                return;
            }
            // First movement after pressing inside an existing selection
            // turns the gesture into a drag of a brand-new range from that
            // press point (a click with no movement still activates on
            // release); a press on an empty slot already set this flag.
            if !update_data.drag_active.get() {
                update_data.drag_active.set(true);
            }
            *update_data.selection.borrow_mut() = Some(SlotRange::spanning(anchor.0, anchor.1, slot.0, slot.1));
            update_canvas.queue_draw();
        });
        gesture.connect_released(move |_, _, x, y| {
            // A chip press released without ever crossing the drag threshold:
            // a plain click - activate the event. The press is copied out
            // first, as in the update handler above.
            let chip_press = *release_data.chip_press.borrow();
            if let Some((chip_idx, _, _, _)) = chip_press {
                *release_data.chip_press.borrow_mut() = None;
                let occ = {
                    let chips_guard = release_data.chips.borrow();
                    let occurrence_index = chips_guard[chip_idx].occurrence;
                    release_data.occurrences.borrow().get(occurrence_index).cloned()
                };
                if let Some(occ) = occ {
                    if let Some(callback) = on_activate.borrow().as_ref() {
                        callback(occ);
                    }
                }
                return;
            }
            // A chip drag ended: report the resolved new start/end so the
            // caller can persist the move/resize.
            let ended_drag = release_data.drag.borrow_mut().take();
            if let Some(drag) = ended_drag {
                release_canvas.set_cursor_from_name(None);
                let occ = {
                    let chips_guard = release_data.chips.borrow();
                    let occurrence_index = chips_guard[drag.chip].occurrence;
                    release_data.occurrences.borrow().get(occurrence_index).cloned()
                };
                let Some(occ) = occ else { return };
                let (new_start, new_end) = drag_times(&drag, &occ);
                if let Some(callback) = on_drag.borrow().as_ref() {
                    callback(occ, new_start, new_end, release_data.drag_copy.get());
                }
                release_canvas.queue_draw();
                return;
            }
            let was_dragging = release_data.drag_active.replace(false);
            let Some(anchor) = *release_data.drag_anchor.borrow() else { return };
            let Some(release_slot) = slot_at_release(x, y) else { return };
            if was_dragging {
                // A real drag: keep the range highlighted for a later click.
                if (release_slot.0, release_slot.1) != anchor {
                    *release_data.selection.borrow_mut() = Some(SlotRange::spanning(anchor.0, anchor.1, release_slot.0, release_slot.1));
                    release_canvas.queue_draw();
                }
                return;
            }
            // A plain click that started inside the selection: activate it.
            if (release_slot.0, release_slot.1) == anchor {
                let range = release_data.selection.borrow_mut().take();
                release_canvas.queue_draw();
                if let Some(range) = range {
                    if let Some(callback) = on_slot_activate.borrow().as_ref() {
                        callback(range.start_date, range.start_minutes, range.end_date, range.end_minutes);
                    }
                }
            }
        });
    }
    canvas.add_controller(gesture);
}

/// A right-click on a time-grid chip opens the event context menu (item 3.12
/// / CAL-31), anchored to the canvas at the click point. Separate from
/// `attach_click`, whose primary-button gesture owns drag/select/activate.
fn attach_time_grid_context(canvas: &gtk::DrawingArea, band: bool, data: &TimeGridData, on_context: &PendingContext) {
    let gesture = gtk::GestureClick::new();
    gesture.set_button(gtk::gdk::BUTTON_SECONDARY);
    {
        let data = data.clone();
        let canvas_widget = canvas.clone();
        let on_context = on_context.clone();
        gesture.connect_pressed(move |_, _, x, y| {
            // Resolve the chip first (its own borrows end with the block) so
            // the callback can re-render without a borrow conflict.
            let occ = {
                let dates_guard = data.dates.borrow();
                let chips_guard = data.chips.borrow();
                hovered_chip(data.gutter.get(), &chips_guard, canvas_widget.width() as f64, dates_guard.len(), x, y, band)
                    .and_then(|idx| chips_guard.get(idx))
                    .and_then(|chip| data.occurrences.borrow().get(chip.occurrence).cloned())
            };
            let Some(occ) = occ else { return };
            if let Some(callback) = on_context.borrow().as_ref() {
                callback(occ, canvas_widget.clone().upcast(), Some((x, y)));
            }
        });
    }
    canvas.add_controller(gesture);
}

/// The day column and snapped start-minute of the time slot under `(x, y)` on
/// a grid `width` wide with `n_cols` day columns, or `None` for clicks in the
/// hour gutter or outside the columns. Minutes are rounded to the nearest half
/// hour and clamped so the slot never starts in the final 30 minutes of the
/// day (a one-hour default span must stay within the day).
fn slot_from_point(gutter: f64, x: f64, y: f64, width: f64, n_cols: usize) -> Option<(usize, i64)> {
    if n_cols == 0 || x < gutter {
        return None;
    }
    let col_width = ((width - gutter).max(0.0)) / n_cols as f64;
    let col = (((x - gutter) / col_width) as usize).min(n_cols - 1);
    let minutes = (y / TIME_SLOT_HEIGHT * 60.0) as i64;
    let snapped = (((minutes + 15) / 30) * 30).clamp(0, 1440 - 30);
    Some((col, snapped))
}

/// The day-column width for a grid `width` wide with `n_cols` columns - the
/// shared geometry behind chip hit-testing, geometry, and drag positions.
fn col_width_for(gutter: f64, width: f64, n_cols: usize) -> f64 {
    if n_cols > 0 {
        ((width - gutter).max(0.0)) / n_cols as f64
    } else {
        0.0
    }
}

/// The `(column, unsnapped minutes)` under `(x, y)` on a grid `width` wide
/// with `n_cols` day columns, or `None` for the hour gutter/outside. Like
/// [`slot_from_point`] but without the half-hour snapping (drag live positions
/// snap separately in [`drag_update`]) and with the full 0..=1440 minute range
/// so a drag can end exactly at midnight.
fn col_minutes_at(gutter: f64, x: f64, y: f64, width: f64, n_cols: usize) -> Option<(usize, i64)> {
    if n_cols == 0 || x < gutter {
        return None;
    }
    let col_width = col_width_for(gutter, width, n_cols);
    let col = (((x - gutter) / col_width) as usize).min(n_cols - 1);
    let minutes = ((y / TIME_SLOT_HEIGHT * 60.0) as i64).clamp(0, 1440);
    Some((col, minutes))
}

/// Recomputes a chip drag's live span from a pointer at `(col, minutes)`
/// (minutes unsnapped, clamped to 0..=1440 by the caller). `total_minutes` is
/// the grid window's span (`n_cols * 1440`). Pure and unit-testable.
///
/// A move preserves the pointer's grab offset against the chip's original
/// start and shifts both ends together (duration unchanged); a resize keeps
/// the original start and follows the pointer with the end, never letting the
/// span flip or fall below one snap slot. All-day chips move by whole days.
/// Timed live positions snap to [`DRAG_SNAP_MINUTES`] and everything is
/// clamped inside the visible grid window.
fn drag_update(drag: &mut TimeGridDrag, col: i64, minutes: i64, total_minutes: i64) {
    if drag.all_day {
        let span = drag.original_end - drag.original_start;
        drag.live_start = (col * 1440 - drag.grab_offset).clamp(0, (total_minutes - span).max(0));
        drag.live_end = drag.live_start + span;
        return;
    }
    let pointer = (col * 1440 + minutes).clamp(0, total_minutes);
    let snap = |v: i64| ((v + DRAG_SNAP_MINUTES / 2) / DRAG_SNAP_MINUTES) * DRAG_SNAP_MINUTES;
    match drag.mode {
        DragMode::Move => {
            let duration = drag.original_end - drag.original_start;
            let start = snap(pointer - drag.grab_offset).clamp(0, (total_minutes - duration).max(0));
            drag.live_start = start;
            drag.live_end = start + duration;
        }
        DragMode::ResizeEnd => {
            drag.live_start = drag.original_start;
            drag.live_end = snap(pointer).clamp(0, total_minutes).max(drag.live_start + DRAG_SNAP_MINUTES);
        }
    }
}

/// The UTC start/end a dropped drag resolves to: a move shifts both ends by
/// the live-span delta, a resize shifts only the end. Deltas are applied to
/// the occurrence's original UTC instants, so the wall-clock times in the
/// user's timezone shift by exactly the drag's grid minutes.
fn drag_times(drag: &TimeGridDrag, occ: &EventOccurrence) -> (DateTime<Utc>, DateTime<Utc>) {
    let start = occ.start + Duration::minutes(drag.live_start - drag.original_start);
    let end = occ.end + Duration::minutes(drag.live_end - drag.original_end);
    (start, end)
}

/// The [`TimeChip`] the drag ghost renders as: the live span laid out with the
/// same column/start/end conventions as [`compute_time_grid_chips`] (an end at
/// exactly midnight of the day after the chip's start renders as a full
/// `1440`-minute last column). Full-width (`lane 0` of `lanes 1`) so the
/// dragged chip always reads clearly.
fn drag_ghost_chip(drag: &TimeGridDrag, occurrence: usize) -> TimeChip {
    let start_col = drag.live_start / 1440;
    let end_col = ((drag.live_end - 1).max(drag.live_start)) / 1440;
    let end_minutes = if drag.live_end % 1440 == 0 && drag.live_end > drag.live_start {
        1440
    } else {
        drag.live_end % 1440
    };
    TimeChip {
        column: start_col as usize,
        span: (end_col - start_col + 1) as usize,
        lane: 0,
        lanes: 1,
        all_day: drag.all_day,
        start_minutes: drag.live_start % 1440,
        end_minutes,
        occurrence,
    }
}

/// The index of the chip under `(x, y)` among the chips of one split half
/// (`band` = all-day chips, otherwise timed chips), or `None`. Uses the same
/// geometry maths as the paint code, so hover hits exactly what is drawn.
fn hovered_chip(gutter: f64, chips: &[TimeChip], canvas_width: f64, n_cols: usize, x: f64, y: f64, band: bool) -> Option<usize> {
    let col_width = col_width_for(gutter, canvas_width, n_cols);
    chips
        .iter()
        .enumerate()
        .find(|(_, chip)| {
            chip.all_day == band && {
                let (cx, cy, cw, ch) = chip_geometry(gutter, chip, col_width);
                x >= cx && x <= cx + cw && y >= cy && y <= cy + ch
            }
        })
        .map(|(i, _)| i)
}

/// Re-points the grid at `anchor` (the week containing it for Week/Work week,
/// the date itself for Day) and re-renders it with the given occurrences,
/// replacing the old `set_week`/`set_week_occurrences`/`set_day`/
/// `set_day_occurrences` calls. `on_activate` fires when an event chip is
/// clicked; `on_slot_activate` when an already-selected time slot is clicked
/// again (see [`SlotActivate`]); `on_drag` carries the drag-reschedule path
/// (see [`connect_event_dragged`]).
#[allow(clippy::too_many_arguments)]
pub fn set_time_grid(
    t: &TimeGrid,
    anchor: NaiveDate,
    occurrences: &[EventOccurrence],
    colors: &HashMap<CalendarId, String>,
    on_activate: Option<ActivateEvent>,
    on_slot_activate: Option<SlotActivate>,
    on_drag: Option<EventDrag>,
) {
    *t.on_activate.borrow_mut() = on_activate;
    *t.on_slot_activate.borrow_mut() = on_slot_activate;
    *t.on_drag.borrow_mut() = on_drag;
    *t.anchor.borrow_mut() = anchor;
    *t.data.occurrences.borrow_mut() = occurrences.to_vec();
    *t.data.colors.borrow_mut() = colors.clone();

    // A new render pass repaints the whole grid: the previously highlighted
    // range no longer corresponds to visible positions, and any armed chip
    // press/drag is stale too.
    *t.data.selection.borrow_mut() = None;
    t.data.drag_active.set(false);
    *t.data.drag_anchor.borrow_mut() = None;
    *t.data.chip_press.borrow_mut() = None;
    *t.data.drag.borrow_mut() = None;
    t.data.drag_copy.set(false);

    let dates = grid_dates(t);
    let chips = compute_time_grid_chips(&dates, occurrences);
    *t.data.dates.borrow_mut() = dates.clone();
    *t.data.chips.borrow_mut() = chips;

    let today = today();
    for (i, label) in t.headers.borrow().iter().enumerate() {
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
    let now = crate::calendar_tz::display_zone().now();
    scroll_scroller_to_minutes(scroller, now.hour() as i64 * 60 + now.minute() as i64);
}

/// Scrolls `t`'s timeline so `minutes` (minutes since local midnight) sits
/// ~100px from the top - the same placement [`scroll_time_grid_to_now`] uses
/// for "now". Lets a caller outside this module (the event editor's reused
/// Day-view preview) follow an arbitrary picked time rather than only ever
/// jumping to the current moment.
pub(crate) fn scroll_time_grid_to_minutes(t: &TimeGrid, minutes: i64) {
    scroll_scroller_to_minutes(&t.scroller, minutes);
}

fn scroll_scroller_to_minutes(scroller: &gtk::ScrolledWindow, minutes: i64) {
    let scroller = scroller.clone();
    gtk::glib::idle_add_local_once(move || {
        // The all-day band is fixed above the scroller, so only the hour
        // timeline scrolls: place the target time ~100px from the top of
        // that timeline.
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
        let weekdays = t.weekdays.borrow();
        let start = week_start(anchor, weekdays[0]);
        (0..weekdays.len()).map(|i| start + chrono::Duration::days(i as i64)).collect()
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
        let (start_local, end_local) = local_span(occ);
        let start_date = start_local.date();
        let end_date = end_local.date();

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
fn chip_geometry(gutter: f64, chip: &TimeChip, col_width: f64) -> (f64, f64, f64, f64) {
    let lane_width = col_width / chip.lanes.max(1) as f64;
    let x = gutter + chip.column as f64 * col_width + chip.lane as f64 * lane_width + 1.0;
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
        let time = format_event_time(&local_span(occ).0);
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
        let start = format_event_time(&local_span(occ).0);
        let end = format_event_time(&local_span(occ).1);
        format!("{start} – {end}  {}", occ.summary.as_deref().unwrap_or("(untitled)"))
    }
}

/// Paints the whole time grid: the all-day band and 24-hour timeline with
/// business-hours shading and hairlines, the hour-gutter labels, and every
/// positioned event chip. Fully custom-drawn so chips sit at their exact
/// start/end times and multi-day events can span columns; text uses cairo's
/// toy font API since themed widget labels can't be placed inside a canvas.
/// Paints the fixed all-day band: its column separators, the "All day"
/// gutter label, and every all-day chip. It sits
/// above the scroller so these chips stay visible while the timeline scrolls.
fn paint_all_day_band(cr: &gtk::cairo::Context, width: f64, height: f64, data: &TimeGridData, hover: Option<usize>, fg: (f64, f64, f64)) {
    let dates = data.dates.borrow();
    let chips = data.chips.borrow();
    let occurrences = data.occurrences.borrow();
    let colors = data.colors.borrow();
    let n = dates.len();
    let gutter = data.gutter.get();
    let columns_width = (width - gutter).max(0.0);
    let col_width = if n > 0 { columns_width / n as f64 } else { 0.0 };

    // No background fill: the band is the main panel's own (translucent)
    // background, see `GRID_BUSINESS_ALPHA`. Column separators, aligned with
    // the timeline's below.
    cr.set_source_rgba(fg.0, fg.1, fg.2, GRID_LINE_ALPHA);
    cr.set_line_width(1.0);
    for col in 0..=n {
        let x = gutter + col as f64 * col_width;
        cr.move_to(x, 0.0);
        cr.line_to(x, height);
    }
    let _ = cr.stroke();

    // The gutter reads "All day" - or, with a second time zone shown, names
    // the zone above each gutter column (second zone outside, the display
    // zone next to the grid), as Outlook's two-zone gutter does.
    let dim_text = (fg.0, fg.1, fg.2, GRID_DIM_TEXT_ALPHA);
    match data.secondary_zone.get() {
        Some(secondary) => {
            let at = dates
                .first()
                .and_then(|d| d.and_hms_opt(12, 0, 0))
                .map(|naive| crate::calendar_tz::display_zone().resolve_local(naive))
                .unwrap_or_else(Utc::now);
            let primary_label = crate::calendar_tz::display_zone().label(at);
            let secondary_label = crate::calendar_tz::DisplayZone::Named(secondary).label(at);
            paint_right_text(cr, &secondary_label, HOUR_GUTTER_WIDTH - 5.0, 3.0, 10.0, dim_text, FontWeight::Normal);
            paint_right_text(cr, &primary_label, gutter - 5.0, 3.0, 10.0, dim_text, FontWeight::Normal);
        }
        None => {
            paint_right_text(cr, "All day", gutter - 5.0, 3.0, 10.0, dim_text, FontWeight::Normal);
        }
    }

    let dragged = data.drag.borrow().map(|d| d.chip);
    for (i, chip) in chips.iter().enumerate() {
        if chip.all_day {
            if dragged == Some(i) {
                continue;
            }
            paint_chip(cr, gutter, chip, &occurrences[chip.occurrence], &colors, col_width, Some(i) == hover, 0.92, fg);
        }
    }
    if let Some(drag) = *data.drag.borrow() {
        if drag.all_day {
            let occurrence = chips[drag.chip].occurrence;
            paint_chip(
                cr,
                gutter,
                &drag_ghost_chip(&drag, occurrence),
                &occurrences[occurrence],
                &colors,
                col_width,
                false,
                0.45,
                fg,
            );
        }
    }
}

/// Paints the scrollable 24-hour timeline: the business-hours stripe and
/// hairlines over the main panel's own translucent background, the hour-gutter
/// labels, and every timed event chip (single-day and multi-day spans)
/// positioned at its exact start/end times. Fully custom-drawn so chips sit by
/// the clock and multi-day events can span columns; text uses cairo's toy font
/// API since themed widget labels can't be placed inside a canvas.
fn paint_time_grid(cr: &gtk::cairo::Context, width: f64, height: f64, data: &TimeGridData, hover: Option<usize>, fg: (f64, f64, f64)) {
    let dates = data.dates.borrow();
    let chips = data.chips.borrow();
    let occurrences = data.occurrences.borrow();
    let colors = data.colors.borrow();
    let n = dates.len();
    let gutter = data.gutter.get();
    let columns_width = (width - gutter).max(0.0);
    let col_width = if n > 0 { columns_width / n as f64 } else { 0.0 };

    // No background fill (see `GRID_BUSINESS_ALPHA`): off-hours are the main
    // panel's own translucent background, with a foreground tint marking the
    // business-hours stripe. The band comes from the `calendar-work-start`/
    // `-end` preferences, on the `calendar-work-days` columns only.
    let work_start = data.work_start.get();
    let work_end = data.work_end.get();
    let business_top = work_start as f64 * TIME_SLOT_HEIGHT;
    let business_height = (work_end.saturating_sub(work_start)) as f64 * TIME_SLOT_HEIGHT;
    for col in business_columns(&dates, &data.work_days.borrow()) {
        cr.rectangle(gutter + col as f64 * col_width, business_top, col_width, business_height);
    }
    cr.set_source_rgba(fg.0, fg.1, fg.2, GRID_BUSINESS_ALPHA);
    let _ = cr.fill();

    // Hairline gridlines: hour rows across the columns, plus column separators.
    cr.set_source_rgba(fg.0, fg.1, fg.2, GRID_LINE_ALPHA);
    cr.set_line_width(1.0);
    for hh in 0..=HOURS_PER_DAY {
        let y = hh as f64 * TIME_SLOT_HEIGHT;
        if y > height {
            break;
        }
        cr.move_to(gutter, y);
        cr.line_to(width, y);
    }
    for col in 0..=n {
        let x = gutter + col as f64 * col_width;
        cr.move_to(x, 0.0);
        cr.line_to(x, height);
    }
    let _ = cr.stroke();

    // Gutter labels: right-aligned hour markers beside their rows, plus the
    // second time zone's matching times in the outer column (CAL-10). Those
    // are converted on the first displayed date, so a DST change inside a
    // shown week shifts by an hour from that day on - as in Outlook.
    let dim_text = (fg.0, fg.1, fg.2, GRID_DIM_TEXT_ALPHA);
    let secondary = data.secondary_zone.get();
    let label_date = dates.first().copied().unwrap_or_else(today);
    for hh in 0..HOURS_PER_DAY {
        let y = hh as f64 * TIME_SLOT_HEIGHT + 3.0;
        paint_right_text(cr, &hour_gutter_text(hh), gutter - 5.0, y, 10.0, dim_text, FontWeight::Normal);
        if let Some(secondary) = secondary {
            let label = secondary_gutter_label(hh as u32, label_date, crate::calendar_tz::display_zone(), secondary);
            paint_right_text(cr, &label, HOUR_GUTTER_WIDTH - 5.0, y, 10.0, dim_text, FontWeight::Normal);
        }
    }

    // The highlighted slot range (from a click, or a click-and-drag): a
    // translucent band across every covered column - full-day on the interior
    // columns, and the partial start/end segments on the boundary ones.
    if let Some(range) = *data.selection.borrow() {
        for (col, date) in dates.iter().enumerate() {
            if *date < range.start_date || *date > range.end_date {
                continue;
            }
            let top = if *date == range.start_date {
                range.start_minutes as f64 * TIME_SLOT_HEIGHT / 60.0
            } else {
                0.0
            };
            let bottom = if *date == range.end_date {
                (range.end_minutes + 30) as f64 * TIME_SLOT_HEIGHT / 60.0
            } else {
                HOURS_PER_DAY as f64 * TIME_SLOT_HEIGHT
            };
            let height = bottom - top;
            if height <= 0.0 {
                continue;
            }
            cr.rectangle(gutter + col as f64 * col_width + 1.0, top + 1.0, col_width - 2.0, height - 2.0);
            cr.set_source_rgba(fg.0, fg.1, fg.2, GRID_SELECTION_FILL_ALPHA);
            let _ = cr.fill();
            cr.rectangle(gutter + col as f64 * col_width + 0.5, top + 0.5, col_width - 1.0, height - 1.0);
            cr.set_source_rgba(fg.0, fg.1, fg.2, GRID_SELECTION_STROKE_ALPHA);
            cr.set_line_width(1.0);
            let _ = cr.stroke();
        }
    }

    let dragged = data.drag.borrow().map(|d| d.chip);
    for (i, chip) in chips.iter().enumerate() {
        if !chip.all_day {
            if dragged == Some(i) {
                continue;
            }
            paint_chip(cr, gutter, chip, &occurrences[chip.occurrence], &colors, col_width, Some(i) == hover, 0.92, fg);
        }
    }
    if let Some(drag) = *data.drag.borrow() {
        if !drag.all_day {
            let occurrence = chips[drag.chip].occurrence;
            paint_chip(
                cr,
                gutter,
                &drag_ghost_chip(&drag, occurrence),
                &occurrences[occurrence],
                &colors,
                col_width,
                false,
                0.45,
                fg,
            );
        }
    }
}

/// The column indexes among `dates` that fall on a work day - the columns
/// [`paint_time_grid`] gives the business-hours band.
fn business_columns(dates: &[NaiveDate], work_days: &[chrono::Weekday]) -> Vec<usize> {
    dates
        .iter()
        .enumerate()
        .filter(|(_, date)| work_days.contains(&date.weekday()))
        .map(|(col, _)| col)
        .collect()
}

/// The time-grid canvas's current theme foreground colour, which every
/// grid tone is an alpha of (see `GRID_BUSINESS_ALPHA`).
fn foreground_rgb(widget: &impl IsA<gtk::Widget>) -> (f64, f64, f64) {
    let color = widget.as_ref().color();
    (color.red() as f64, color.green() as f64, color.blue() as f64)
}

/// How `paint_chip` fills and outlines an event chip for one "Show as" state
/// (CAL-05, UX-57): Outlook reserves a distinct look for every state so the
/// grid reads at a glance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChipStyle {
    /// Busy: the pre-3.3 look - the calendar colour, solid.
    Solid,
    /// Free: outline only, no fill.
    Hollow,
    /// Tentative: a light fill under diagonal hatching.
    Hatched,
    /// Away / out of office: solid, tinted Outlook's OOF purple.
    Oof,
    /// Working elsewhere: a translucent fill with a dotted outline.
    Dotted,
}

/// The chip style for one show-as state.
pub fn chip_style(show_as: BusyStatus) -> ChipStyle {
    match show_as {
        BusyStatus::Busy => ChipStyle::Solid,
        BusyStatus::Free => ChipStyle::Hollow,
        BusyStatus::Tentative => ChipStyle::Hatched,
        BusyStatus::Oof => ChipStyle::Oof,
        BusyStatus::WorkingElsewhere => ChipStyle::Dotted,
    }
}

/// Outlook's out-of-office purple (`#7B2CBF`), the tint an Away chip shifts
/// towards so it stays recognisable whatever the calendar's own colour is.
const OOF_PURPLE: (f64, f64, f64) = (0.482, 0.173, 0.749);

/// The fill colour an Away chip paints with: 55% of Outlook's OOF purple over
/// the calendar's own colour, keeping some of the calendar's identity.
fn oof_fill(r: f64, g: f64, b: f64) -> (f64, f64, f64) {
    (0.45 * r + 0.55 * OOF_PURPLE.0, 0.45 * g + 0.55 * OOF_PURPLE.1, 0.45 * b + 0.55 * OOF_PURPLE.2)
}

fn fill_rect(cr: &gtk::cairo::Context, rect: (f64, f64, f64, f64), color: (f64, f64, f64), alpha: f64) {
    let (x, y, w, h) = rect;
    cr.rectangle(x, y, w, h);
    cr.set_source_rgba(color.0, color.1, color.2, alpha);
    let _ = cr.fill();
}

fn stroke_rect(cr: &gtk::cairo::Context, rect: (f64, f64, f64, f64), color: (f64, f64, f64), alpha: f64, width: f64) {
    let (x, y, w, h) = rect;
    cr.rectangle(x, y, w, h);
    cr.set_source_rgba(color.0, color.1, color.2, alpha);
    cr.set_line_width(width);
    let _ = cr.stroke();
}

/// Diagonal hatching across the chip: the tentative fill, clipped to the
/// chip's rectangle so no line escapes it.
fn hatch_rect(cr: &gtk::cairo::Context, rect: (f64, f64, f64, f64), color: (f64, f64, f64), alpha: f64) {
    let (x, y, w, h) = rect;
    let _ = cr.save();
    cr.rectangle(x, y, w, h);
    cr.clip();
    cr.set_source_rgba(color.0, color.1, color.2, alpha);
    cr.set_line_width(1.0);
    let step = 6.0;
    let mut offset = -h;
    while offset < w + h {
        cr.move_to(x + offset, y + h);
        cr.line_to(x + offset + h, y);
        offset += step;
    }
    let _ = cr.stroke();
    let _ = cr.restore();
}

/// Paints one event chip (fill, outline, hover ring, and its label). `alpha`
/// scales the fill opacity - the drag ghost paints at a lower alpha so the
/// grid stays readable under it. `fg` is the canvas's theme foreground, used
/// as the text colour on the light-filled styles where a white/dark-on-colour
/// choice doesn't apply.
#[allow(clippy::too_many_arguments)]
fn paint_chip(
    cr: &gtk::cairo::Context,
    gutter: f64,
    chip: &TimeChip,
    occ: &EventOccurrence,
    colors: &HashMap<CalendarId, String>,
    col_width: f64,
    hovered: bool,
    alpha: f64,
    fg: (f64, f64, f64),
) {
    let color = colors.get(&occ.calendar_id).map(String::as_str).unwrap_or(calendar_colors::DEFAULT_CHECK_COLOR);
    let (cx, cy, cw, ch) = chip_geometry(gutter, chip, col_width);
    paint_event_block(cr, (cx, cy, cw, ch), color, occ.show_as, &chip_text(occ, chip, ch), hovered, alpha, fg);
}

/// Paints an event block into `rect`: the calendar-colour fill/outline, the
/// hover ring and the label, styled per show-as state (CAL-05, UX-57). Shared
/// by the Day/Week time-grid chips ([`paint_chip`]) and the Schedule view's
/// lanes, so both read identically. `alpha` scales the fill opacity; `fg` is
/// the canvas's theme foreground, the text colour on the light-filled styles.
#[allow(clippy::too_many_arguments)]
fn paint_event_block(cr: &gtk::cairo::Context, rect: (f64, f64, f64, f64), color: &str, show_as: BusyStatus, label: &str, hovered: bool, alpha: f64, fg: (f64, f64, f64)) {
    let (cx, cy, cw, ch) = rect;
    if cw < 2.0 || ch < 2.0 {
        return;
    }
    let (r, g, b) = css_color_rgb(color);
    let style = chip_style(show_as);
    let chip_color = (r, g, b);
    let inset = (cx + 0.5, cy + 0.5, cw - 1.0, ch - 1.0);
    match style {
        // Busy: the original solid fill with its dark hairline border.
        ChipStyle::Solid => {
            fill_rect(cr, rect, chip_color, 0.92 * alpha);
            stroke_rect(cr, inset, (0.0, 0.0, 0.0), 0.35, 1.0);
        }
        // Away: solid, purple-tinted.
        ChipStyle::Oof => {
            fill_rect(cr, rect, oof_fill(r, g, b), 0.92 * alpha);
            stroke_rect(cr, inset, (0.0, 0.0, 0.0), 0.35, 1.0);
        }
        // Tentative: a light wash under diagonal hatching.
        ChipStyle::Hatched => {
            fill_rect(cr, rect, chip_color, 0.30 * alpha);
            hatch_rect(cr, rect, chip_color, 0.85 * alpha);
            stroke_rect(cr, inset, (0.0, 0.0, 0.0), 0.35, 1.0);
        }
        // Free: outline only, in the calendar's own colour.
        ChipStyle::Hollow => {
            stroke_rect(cr, (cx + 0.75, cy + 0.75, cw - 1.5, ch - 1.5), chip_color, 0.9 * alpha, 1.5);
        }
        // Working elsewhere: a translucent fill with a dotted outline.
        ChipStyle::Dotted => {
            fill_rect(cr, rect, chip_color, 0.55 * alpha);
            let _ = cr.save();
            cr.set_dash(&[3.0, 3.0], 0.0);
            stroke_rect(cr, (cx + 0.75, cy + 0.75, cw - 1.5, ch - 1.5), chip_color, 0.95 * alpha, 1.2);
            cr.set_dash(&[], 0.0);
            let _ = cr.restore();
        }
    }
    if hovered {
        cr.rectangle(cx + 0.5, cy + 0.5, cw - 1.0, ch - 1.0);
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.55);
        let _ = cr.stroke();
    }
    // On the solid styles the text contrast is decided by the fill colour; on
    // the light-filled/hollow styles it follows the theme's foreground.
    let text_color = match style {
        ChipStyle::Solid => readable_chip_color(color),
        ChipStyle::Oof => readable_chip_color(&css_color_to_hex(oof_fill(r, g, b))),
        ChipStyle::Dotted => readable_chip_color(color),
        ChipStyle::Hatched | ChipStyle::Hollow => (fg.0, fg.1, fg.2, 0.95),
    };
    let text = ellipsize_text(cr, label, (cw - 8.0).max(4.0));
    if !text.is_empty() {
        cr.set_source_rgba(text_color.0, text_color.1, text_color.2, text_color.3);
        cr.select_font_face("Sans", FontSlant::Normal, FontWeight::Normal);
        cr.set_font_size(10.0);
        cr.move_to(cx + 4.0, cy + ch / 2.0 + 10.0 * CHIP_TEXT_BASELINE_OFFSET);
        let _ = cr.show_text(&text);
    }
}

/// White or near-black text for a chip fill, by luminance (the old inline
/// choice, now shared by the tinted Away fill).
fn readable_chip_color(hex: &str) -> (f64, f64, f64, f64) {
    if calendar_colors::readable_foreground(hex) == "white" {
        (1.0, 1.0, 1.0, 0.95)
    } else {
        (0.0, 0.0, 0.0, 0.85)
    }
}

fn css_color_to_hex((r, g, b): (f64, f64, f64)) -> String {
    format!("#{:02x}{:02x}{:02x}", (r * 255.0).round() as u8, (g * 255.0).round() as u8, (b * 255.0).round() as u8)
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
    on_activate: PendingActivate,
    /// The right-click context-menu callback (item 3.12), shared with the
    /// other views (see [`connect_event_context_menu`]).
    on_context: PendingContext,
}

fn build_agenda_view(on_context: &PendingContext) -> AgendaView {
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
        anchor: Rc::new(RefCell::new(today())),
        on_activate: Rc::new(RefCell::new(None)),
        on_context: on_context.clone(),
    }
}

/// Rebuilds the agenda's rows for the inclusive `anchor..=window_end` range,
/// grouped by day: a day header per local date ("Today"/"Tomorrow"/"Wed 12
/// Aug") with its events under it, each row a time column plus "5:00pm –
/// 6:00pm summary" (or "All day summary"). The Split view passes its month's
/// end (a month agenda); the standalone List view passes a rolling window
/// (see [`list_window_end`]).
pub fn set_agenda(
    a: &AgendaView,
    anchor: NaiveDate,
    window_end: NaiveDate,
    occurrences: &[EventOccurrence],
    colors: &HashMap<CalendarId, String>,
    on_activate: Option<ActivateEvent>,
) {
    *a.on_activate.borrow_mut() = on_activate;
    *a.anchor.borrow_mut() = anchor;
    clear_children(&a.events_box);

    let mut upcoming: Vec<&EventOccurrence> = occurrences
        .iter()
        .filter(|occ| {
            let date = local_span(occ).0.date();
            date >= anchor && date <= window_end
        })
        .collect();
    upcoming.sort_by_key(|occ| occ.start);

    if upcoming.is_empty() {
        let placeholder = gtk::Label::builder().label("No events").css_classes(["dim-label", "caption"]).xalign(0.0).build();
        a.events_box.append(&placeholder);
        return;
    }

    let today = today();
    let mut day_groups: Vec<(NaiveDate, Vec<&EventOccurrence>)> = Vec::new();
    for occ in upcoming {
        let date = local_span(occ).0.date();
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
                .label(if occ.all_day { "All day".to_string() } else { format_event_time(&local_span(occ).0) })
                .css_classes(["dim-label", "caption"])
                .width_request(96)
                .xalign(0.0)
                .build();

            let text = if occ.all_day {
                occ.summary.clone().unwrap_or_else(|| "(untitled)".to_string())
            } else {
                let start = format_event_time(&local_span(occ).0);
                let end = format_event_time(&local_span(occ).1);
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

            // A right-click on the row opens the event context menu, anchored
            // to the row (item 3.12 / CAL-31).
            let attach_context = |widget: &gtk::Widget| {
                let Some(callback) = a.on_context.borrow().clone() else { return };
                let occ = occ.clone();
                let gesture = gtk::GestureClick::new();
                gesture.set_button(gtk::gdk::BUTTON_SECONDARY);
                gesture.connect_pressed(move |gesture, _, _, _| {
                    if let Some(anchor) = gesture.widget() {
                        callback(occ.clone(), anchor, None);
                    }
                });
                widget.add_controller(gesture);
            };

            // The row is a clickable button when an editor is wired up, so the
            // agenda can open an event too. A plain box otherwise.
            if let Some(callback) = a.on_activate.borrow().as_ref() {
                let button = gtk::Button::builder().child(&row).css_classes(["flat"]).build();
                let occ = occ.clone();
                let callback = callback.clone();
                button.connect_clicked(move |_| callback(occ.clone()));
                attach_context(button.upcast_ref());
                a.events_box.append(&button);
            } else {
                attach_context(row.upcast_ref());
                a.events_box.append(&row);
            }
        }
    }
}

/// The agenda's day-group header: "Today"/"Tomorrow" when relevant, otherwise
/// the weekday + day + month (e.g. "Wed 12 Aug").
pub(crate) fn agenda_day_header(date: NaiveDate, today: NaiveDate) -> String {
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

fn build_split_view(week_start: chrono::Weekday, on_context: &PendingContext) -> SplitView {
    let month = build_month_grid(week_start, on_context);
    month.root.set_width_request(300);
    let agenda = build_agenda_view(on_context);

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

/// Width of the calendar-name gutter on the left of the Schedule view's
/// timeline - wide enough for a calendar's name without crowding the axis.
const SCHEDULE_LABEL_WIDTH: f64 = 168.0;
/// Height of one calendar's lane in the Schedule view.
const SCHEDULE_ROW_HEIGHT: f64 = 44.0;
/// Height of the hour-axis header above the lanes.
const SCHEDULE_HEADER_HEIGHT: f64 = 22.0;
/// Vertical inset of an event block within its lane.
const SCHEDULE_BLOCK_INSET: f64 = 3.0;

/// One calendar's lane in the Schedule view: its display name, its colour and
/// the event blocks laid out across the shared horizontal timeline.
struct ScheduleLane {
    label: String,
    color: String,
    blocks: Vec<ScheduleBlock>,
}

/// One event block on a Schedule lane. `start_minutes`/`end_minutes` are
/// display-zone minutes since local midnight of the shown day, clamped to
/// `0..=1440`; all-day events occupy the whole span.
struct ScheduleBlock {
    occ: EventOccurrence,
    start_minutes: i64,
    end_minutes: i64,
}

/// The Schedule view's mutable render state, shared with its draw/hit-test
/// closures.
struct ScheduleState {
    lanes: Vec<ScheduleLane>,
    date: NaiveDate,
    work_start: u32,
    work_end: u32,
    work_days: Vec<chrono::Weekday>,
}

/// Outlook's Schedule view (CAL-03): a scrollable horizontal timeline with one
/// lane per calendar, events drawn as blocks at their exact times over a
/// shared hour axis (all-day events span their lane). Read-only except for
/// click-to-open, like the other calendar grids; the widget is
/// data-in/callback-out like the rest of this module.
pub struct ScheduleView {
    pub root: gtk::Widget,
    canvas: gtk::DrawingArea,
    state: Rc<RefCell<ScheduleState>>,
    /// The click-to-edit callback, set by each `set_schedule` render pass.
    on_activate: PendingActivate,
}

fn build_schedule_view(prefs: &crate::settings::CalendarPreferences, on_context: &PendingContext) -> ScheduleView {
    let canvas = gtk::DrawingArea::new();
    canvas.set_hexpand(true);
    canvas.set_content_height((SCHEDULE_HEADER_HEIGHT + SCHEDULE_ROW_HEIGHT) as i32);

    let state = Rc::new(RefCell::new(ScheduleState {
        lanes: Vec::new(),
        date: today(),
        work_start: prefs.work_start,
        work_end: prefs.work_end,
        work_days: prefs.work_days.clone(),
    }));
    let on_activate: PendingActivate = Rc::new(RefCell::new(None));
    let hover: Rc<Cell<Option<(usize, usize)>>> = Rc::new(Cell::new(None));

    {
        let state = state.clone();
        let hover = hover.clone();
        canvas.set_draw_func(move |canvas, cr, width, height| {
            paint_schedule(cr, width as f64, height as f64, &state.borrow(), hover.get(), foreground_rgb(canvas));
        });
    }

    // Hover: ring the block under the pointer and show its time range.
    {
        let state = state.clone();
        let hover = hover.clone();
        let canvas_widget = canvas.clone();
        let motion = gtk::EventControllerMotion::new();
        motion.connect_motion(move |_, x, y| {
            let width = canvas_widget.width() as f64;
            let hit = schedule_block_at(&state.borrow(), width, x, y);
            if hover.get() != hit {
                hover.set(hit);
                canvas_widget.queue_draw();
                match hit {
                    Some((lane, block)) => {
                        let tooltip = schedule_block_tooltip(&state.borrow().lanes[lane].blocks[block].occ);
                        canvas_widget.set_tooltip_text(Some(&tooltip));
                    }
                    None => canvas_widget.set_tooltip_text(None),
                }
            }
        });
        canvas.add_controller(motion);
    }

    // Click: open the clicked event (the caller's editor).
    {
        let state = state.clone();
        let on_activate = on_activate.clone();
        let canvas_widget = canvas.clone();
        let click = gtk::GestureClick::new();
        click.set_button(1);
        click.connect_released(move |_, _n, x, y| {
            let width = canvas_widget.width() as f64;
            // Bind the hit first (its own statement), so the `state` borrow is
            // released before we clone the occurrence and call back out.
            let hit = schedule_block_at(&state.borrow(), width, x, y);
            if let Some((lane, block)) = hit {
                let occ = state.borrow().lanes[lane].blocks[block].occ.clone();
                if let Some(callback) = on_activate.borrow().as_ref() {
                    callback(occ);
                }
            }
        });
        canvas.add_controller(click);
    }

    // Right-click: open the event context menu (item 3.12 / CAL-31) at the
    // click point.
    {
        let state = state.clone();
        let on_context = on_context.clone();
        let canvas_widget = canvas.clone();
        let click = gtk::GestureClick::new();
        click.set_button(gtk::gdk::BUTTON_SECONDARY);
        click.connect_pressed(move |_, _, x, y| {
            let width = canvas_widget.width() as f64;
            let hit = schedule_block_at(&state.borrow(), width, x, y);
            let Some((lane, block)) = hit else { return };
            let occ = state.borrow().lanes[lane].blocks[block].occ.clone();
            if let Some(callback) = on_context.borrow().as_ref() {
                callback(occ, canvas_widget.clone().upcast(), Some((x, y)));
            }
        });
        canvas.add_controller(click);
    }

    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .vexpand(true)
        .hexpand(true)
        .child(&canvas)
        .build();

    let root = gtk::Box::builder().orientation(gtk::Orientation::Vertical).vexpand(true).hexpand(true).build();
    root.append(&scroller);

    ScheduleView {
        root: root.upcast(),
        canvas,
        state,
        on_activate,
    }
}

impl ScheduleView {
    /// Applies the Calendar preferences (item 3.1): the business-hours band and
    /// work days. The axis itself is the process-wide display zone, so it is
    /// picked up on the next render. The caller re-renders afterwards.
    fn apply_preferences(&self, prefs: &crate::settings::CalendarPreferences) {
        {
            let mut st = self.state.borrow_mut();
            st.work_start = prefs.work_start;
            st.work_end = prefs.work_end;
            st.work_days = prefs.work_days.clone();
        }
        self.canvas.queue_draw();
    }
}

/// Rebuilds the Schedule view's lanes for `date`: one per `(id, display_name)`
/// in `calendars` (sidebar order, so an empty calendar still gets a lane),
/// each holding that calendar's occurrences for the day with their clamped
/// time spans. Sets the canvas height to fit the lanes and repaints.
fn set_schedule(
    view: &ScheduleView,
    date: NaiveDate,
    occurrences: &[EventOccurrence],
    colors: &HashMap<CalendarId, String>,
    calendars: &[(CalendarId, String)],
    on_activate: Option<ActivateEvent>,
) {
    *view.on_activate.borrow_mut() = on_activate;
    let mut lanes = Vec::with_capacity(calendars.len());
    for (id, label) in calendars {
        let mut blocks: Vec<ScheduleBlock> = occurrences
            .iter()
            .filter(|occ| &occ.calendar_id == id && !covered_local_dates(occ, date, date).is_empty())
            .filter_map(|occ| {
                schedule_block_span(date, occ).map(|(start_minutes, end_minutes)| ScheduleBlock {
                    occ: occ.clone(),
                    start_minutes,
                    end_minutes,
                })
            })
            .collect();
        blocks.sort_by_key(|b| (b.start_minutes, b.end_minutes));
        lanes.push(ScheduleLane {
            label: label.clone(),
            color: colors.get(id).cloned().unwrap_or_else(|| calendar_colors::DEFAULT_CHECK_COLOR.to_string()),
            blocks,
        });
    }
    {
        let mut st = view.state.borrow_mut();
        st.lanes = lanes;
        st.date = date;
    }
    let rows = view.state.borrow().lanes.len().max(1);
    view.canvas.set_content_height((SCHEDULE_HEADER_HEIGHT + rows as f64 * SCHEDULE_ROW_HEIGHT).ceil() as i32);
    view.canvas.queue_draw();
}

/// An occurrence's span within the shown `date`, as display-zone minutes since
/// local midnight clamped to `0..=1440` (all-day events occupy the whole day).
/// `None` when the event does not actually cover `date` (shouldn't happen
/// after the caller's `covered_local_dates` filter, but kept honest).
fn schedule_block_span(date: NaiveDate, occ: &EventOccurrence) -> Option<(i64, i64)> {
    if occ.all_day {
        return Some((0, 1440));
    }
    let (start, end) = local_span(occ);
    let minutes_on = |local: NaiveDateTime| (local.date() - date).num_days() * 1440 + i64::from(local.hour()) * 60 + i64::from(local.minute());
    let start_minutes = minutes_on(start);
    let end_minutes = minutes_on(end);
    if end_minutes <= 0 || start_minutes >= 1440 {
        return None;
    }
    Some((start_minutes.max(0), end_minutes.min(1440)))
}

/// The x position for a minutes-since-midnight value on the Schedule axis; the
/// time plot runs from `SCHEDULE_LABEL_WIDTH` to the widget edge.
fn schedule_x_for_minutes(minutes: f64, width: f64) -> f64 {
    let plot = (width - SCHEDULE_LABEL_WIDTH).max(1.0);
    SCHEDULE_LABEL_WIDTH + (minutes / 1440.0).clamp(0.0, 1.0) * plot
}

/// The pixel rectangle a block occupies in its lane, used for both painting
/// and hit-testing.
fn schedule_block_rect(lane: usize, block: &ScheduleBlock, width: f64) -> (f64, f64, f64, f64) {
    let x = schedule_x_for_minutes(block.start_minutes as f64, width) + 1.0;
    let x_end = schedule_x_for_minutes(block.end_minutes as f64, width) - 1.0;
    let y = SCHEDULE_HEADER_HEIGHT + lane as f64 * SCHEDULE_ROW_HEIGHT + SCHEDULE_BLOCK_INSET;
    let h = SCHEDULE_ROW_HEIGHT - 2.0 * SCHEDULE_BLOCK_INSET;
    (x, y, (x_end - x).max(3.0), h)
}

/// The lane and block index under the pointer, if any.
fn schedule_block_at(state: &ScheduleState, width: f64, x: f64, y: f64) -> Option<(usize, usize)> {
    if y < SCHEDULE_HEADER_HEIGHT {
        return None;
    }
    let lane = ((y - SCHEDULE_HEADER_HEIGHT) / SCHEDULE_ROW_HEIGHT) as usize;
    if lane >= state.lanes.len() {
        return None;
    }
    state.lanes[lane]
        .blocks
        .iter()
        .enumerate()
        .find(|(_, block)| {
            let (bx, by, bw, bh) = schedule_block_rect(lane, block, width);
            x >= bx && x <= bx + bw && y >= by && y <= by + bh
        })
        .map(|(index, _)| (lane, index))
}

/// The tooltip for a Schedule block: the summary plus its full time range.
fn schedule_block_tooltip(occ: &EventOccurrence) -> String {
    if occ.all_day {
        occ.summary.clone().unwrap_or_else(|| "(untitled)".to_string())
    } else {
        let start = format_event_time(&local_span(occ).0);
        let end = format_event_time(&local_span(occ).1);
        format!("{start} – {end}  {}", occ.summary.as_deref().unwrap_or("(untitled)"))
    }
}

/// Paints the Schedule view: the hour axis, one lane per calendar (name,
/// working-hours shading, event blocks) and the current-time line.
fn paint_schedule(cr: &gtk::cairo::Context, width: f64, height: f64, state: &ScheduleState, hover: Option<(usize, usize)>, fg: (f64, f64, f64)) {
    let _ = height;
    cr.select_font_face("Sans", FontSlant::Normal, FontWeight::Normal);

    if state.lanes.is_empty() {
        cr.set_font_size(12.0);
        cr.set_source_rgba(fg.0, fg.1, fg.2, GRID_DIM_TEXT_ALPHA);
        cr.move_to(12.0, SCHEDULE_HEADER_HEIGHT + 20.0);
        let _ = cr.show_text("No calendars selected");
        return;
    }

    let lanes_height = state.lanes.len() as f64 * SCHEDULE_ROW_HEIGHT;

    // Hour axis labels and vertical grid lines.
    for hour in (0..24).step_by(2) {
        let x = schedule_x_for_minutes(hour as f64 * 60.0, width);
        cr.set_source_rgba(fg.0, fg.1, fg.2, GRID_LINE_ALPHA);
        cr.set_line_width(0.5);
        cr.move_to(x, SCHEDULE_HEADER_HEIGHT);
        cr.line_to(x, SCHEDULE_HEADER_HEIGHT + lanes_height);
        let _ = cr.stroke();
        cr.set_font_size(10.0);
        cr.set_source_rgba(fg.0, fg.1, fg.2, GRID_DIM_TEXT_ALPHA);
        cr.move_to(x + 2.0, SCHEDULE_HEADER_HEIGHT - 7.0);
        let _ = cr.show_text(&hour_gutter_text(hour));
    }

    for (lane_index, lane) in state.lanes.iter().enumerate() {
        let y = SCHEDULE_HEADER_HEIGHT + lane_index as f64 * SCHEDULE_ROW_HEIGHT;

        // Business-hours shading on work days.
        if state.work_days.contains(&state.date.weekday()) {
            let start = schedule_x_for_minutes(state.work_start as f64 * 60.0, width);
            let end = schedule_x_for_minutes(state.work_end as f64 * 60.0, width);
            cr.set_source_rgba(fg.0, fg.1, fg.2, GRID_BUSINESS_ALPHA);
            cr.rectangle(start, y, (end - start).max(0.0), SCHEDULE_ROW_HEIGHT);
            let _ = cr.fill();
        }

        // The calendar's name in the left gutter.
        cr.set_font_size(12.0);
        cr.set_source_rgba(fg.0, fg.1, fg.2, 0.9);
        cr.move_to(8.0, y + SCHEDULE_ROW_HEIGHT / 2.0 + 4.0);
        let _ = cr.show_text(&ellipsize_text(cr, &lane.label, SCHEDULE_LABEL_WIDTH - 16.0));

        // The lane's event blocks.
        for (block_index, block) in lane.blocks.iter().enumerate() {
            let rect = schedule_block_rect(lane_index, block, width);
            let label = block.occ.summary.as_deref().unwrap_or("(untitled)");
            paint_event_block(cr, rect, &lane.color, block.occ.show_as, label, hover == Some((lane_index, block_index)), 0.95, fg);
        }

        // Row separator.
        cr.set_source_rgba(fg.0, fg.1, fg.2, GRID_LINE_ALPHA);
        cr.set_line_width(0.5);
        cr.move_to(0.0, y + SCHEDULE_ROW_HEIGHT);
        cr.line_to(width, y + SCHEDULE_ROW_HEIGHT);
        let _ = cr.stroke();
    }

    // Label column separator.
    cr.set_source_rgba(fg.0, fg.1, fg.2, 0.3);
    cr.set_line_width(0.5);
    cr.move_to(SCHEDULE_LABEL_WIDTH, 0.0);
    cr.line_to(SCHEDULE_LABEL_WIDTH, SCHEDULE_HEADER_HEIGHT + lanes_height);
    let _ = cr.stroke();

    // Current-time line.
    if state.date == today() {
        let now = crate::calendar_tz::display_zone().now();
        let minutes = i64::from(now.hour()) * 60 + i64::from(now.minute());
        let x = schedule_x_for_minutes(minutes as f64, width);
        cr.set_source_rgba(0.85, 0.2, 0.2, 0.8);
        cr.set_line_width(1.0);
        cr.move_to(x, SCHEDULE_HEADER_HEIGHT);
        cr.line_to(x, SCHEDULE_HEADER_HEIGHT + lanes_height);
        let _ = cr.stroke();
    }
}

/// The calendar view's main panel: a shared header row (Today + prev/next +
/// a current-month dropdown whose popover holds a mini month grid) above a
/// stack of the views - Month, Work week, Week, Day, Split, List and the
/// horizontal Schedule view - all anchored to a single date so they always
/// agree on what's displayed. `window.rs` drives it the same way it drove the old standalone
/// [`MonthGrid`]: `set_anchor`/`set_occurrences` in, read the anchor back
/// with `anchor()` to decide which month to resync.
pub struct CalendarMain {
    pub root: gtk::Widget,
    pub header_label: gtk::Label,
    pub prev_button: gtk::Button,
    pub next_button: gtk::Button,
    pub today_button: gtk::Button,
    /// The mini month grid living inside the header's current-month dropdown
    /// (a `MenuButton` whose popover holds it). Clicking a day in it re-anchors
    /// the whole panel, so it doubles as the month selector the user asked
    /// the header dropdown to be. Synced to the anchor by the caller's
    /// `show_anchor` alongside the sidebar mini-calendar.
    pub month_mini: MiniCalendar,
    stack: gtk::Stack,
    month: MonthGrid,
    workweek: TimeGrid,
    week: TimeGrid,
    day: TimeGrid,
    agenda: AgendaView,
    split: SplitView,
    /// The horizontal calendars-as-rows Schedule view (CAL-03).
    schedule: ScheduleView,
    /// The checked calendars, in sidebar order, as `(id, display_name)` - the
    /// rows the Schedule view draws (CAL-03). Refreshed by window.rs from the
    /// sidebar checklist via [`set_schedule_calendars`].
    schedule_calendars: Rc<RefCell<Vec<(CalendarId, String)>>>,
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
    /// The click-to-edit callback, handed to every sub-view by each render
    /// pass (see [`connect_event_activated`]).
    on_activate: PendingActivate,
    /// The single-click peek callback (UX-56), handed to the Month/Split
    /// grids by each render pass (see [`connect_event_peeked`]).
    on_peek: PendingPeek,
    /// The click-a-time-slot-to-create callback, handed to every time grid by
    /// each render pass (see [`connect_slot_activated`]).
    on_slot_activate: PendingSlotActivate,
    /// The drag-reschedule callback, handed to every drag-capable sub-view by
    /// each render pass (see [`connect_event_dragged`]).
    on_drag: PendingEventDrag,
    /// The right-click context-menu callback (item 3.12), shared (the same
    /// cell) with every sub-view at build time; `connect_event_context_menu`
    /// fills it in after `build_main`.
    on_context: PendingContext,
}

pub fn build_main(prefs: &crate::settings::CalendarPreferences) -> CalendarMain {
    let week_start = prefs.week_start;
    install_calendar_css();

    let header_label = gtk::Label::builder().css_classes(["title-2"]).hexpand(true).xalign(0.0).build();
    let prev_button = gtk::Button::from_icon_name("go-previous-symbolic");
    let next_button = gtk::Button::from_icon_name("go-next-symbolic");
    let today_button = gtk::Button::builder().label("Today").build();

    // The header's current-month title is a dropdown: clicking it opens a
    // popover holding the same mini month grid the sidebar uses, so the user
    // can browse months with its prev/next buttons and click a day to jump the
    // whole panel there. The caller (`window.rs`) registers that navigation via
    // `connect_day_selected`; the popover closes itself on a day pick here.
    let month_mini = build_mini(week_start);
    let month_popover = gtk::Popover::builder().child(&month_mini.root).build();
    {
        let month_popover = month_popover.clone();
        connect_day_selected(&month_mini, move |_| month_popover.popdown());
    }
    let month_button = gtk::MenuButton::builder()
        .popover(&month_popover)
        .child(&header_label)
        .css_classes(["calendar-toggle"])
        .build();

    let header_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .margin_start(6)
        .margin_end(6)
        .margin_top(6)
        .margin_bottom(6)
        .build();
    header_row.append(&today_button);
    header_row.append(&prev_button);
    header_row.append(&next_button);
    header_row.append(&month_button);

    // The event context-menu callback (item 3.12) is registered after
    // `build_main` returns, so the shared cell is created up-front and cloned
    // into every sub-view; `connect_event_context_menu` fills it in once.
    let on_context: PendingContext = Rc::new(RefCell::new(None));
    let month = build_month_grid(week_start, &on_context);
    let workweek = build_time_grid(&prefs.work_days, false, &on_context);
    let week = build_time_grid(&week_days(week_start), false, &on_context);
    let day = build_time_grid(&[week_start], true, &on_context);
    // The Day/Week/Work-week grids share the configured business-hours band
    // and second time zone.
    for grid in [&workweek, &week, &day] {
        apply_time_grid_preferences(grid, prefs);
    }
    let agenda = build_agenda_view(&on_context);
    let split = build_split_view(week_start, &on_context);
    let schedule = build_schedule_view(prefs, &on_context);

    let stack = gtk::Stack::new();
    stack.add_named(&workweek.root, Some("workweek"));
    stack.add_named(&week.root, Some("week"));
    stack.add_named(&day.root, Some("day"));
    stack.add_named(&split.root, Some("split"));
    stack.add_named(&agenda.root, Some("list"));
    stack.add_named(&schedule.root, Some("schedule"));
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

    let anchor = Rc::new(RefCell::new(today()));
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
        month_mini,
        stack,
        month,
        workweek,
        week,
        day,
        agenda,
        split,
        schedule,
        schedule_calendars: Rc::new(RefCell::new(Vec::new())),
        anchor: anchor.clone(),
        occurrences: Rc::new(RefCell::new(Vec::new())),
        colors: Rc::new(RefCell::new(HashMap::new())),
        check_colors,
        on_activate: Rc::new(RefCell::new(None)),
        on_peek: Rc::new(RefCell::new(None)),
        on_slot_activate: Rc::new(RefCell::new(None)),
        on_drag: Rc::new(RefCell::new(None)),
        on_context,
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
/// `"month"`, `"split"`, `"list"`, or `"schedule"`) and re-renders the shared header title for it.
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
        Some("list") => "list",
        Some("schedule") => "schedule",
        _ => "month",
    }
}

/// The per-grid part of the Calendar preferences: business hours, work days
/// and the second time-zone gutter. Shared by the main panel's grids and the
/// event editor's Day-view preview.
pub(crate) fn apply_time_grid_preferences(grid: &TimeGrid, prefs: &crate::settings::CalendarPreferences) {
    grid.set_work_hours(prefs.work_start as usize, prefs.work_end as usize);
    grid.set_work_days(&prefs.work_days);
    grid.set_zones(prefs.secondary_tz);
}

/// Applies changed Calendar preferences to every view of the main panel and
/// re-renders it (item 3.1): the week start (month grids, the header's mini
/// month, Week view), the Work week's days, business hours and work days,
/// and the second time zone. The display zone itself is process-wide
/// (`calendar_tz::set_display_zone`) and is picked up by the re-render.
pub fn apply_preferences(c: &CalendarMain, prefs: &crate::settings::CalendarPreferences) {
    set_month_week_start(&c.month, prefs.week_start);
    set_month_week_start(&c.split.month, prefs.week_start);
    set_mini_week_start(&c.month_mini, prefs.week_start);
    c.workweek.set_weekdays(&prefs.work_days);
    c.week.set_weekdays(&week_days(prefs.week_start));
    for grid in [&c.workweek, &c.week, &c.day] {
        apply_time_grid_preferences(grid, prefs);
    }
    c.schedule.apply_preferences(prefs);
    refresh(c);
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

/// Sets the calendars the Schedule view draws as rows (CAL-03), in sidebar
/// order as `(id, display_name)`. The caller is expected to re-render (the
/// checklist refresh does, right after); the next [`refresh`] rebuilds the
/// lanes.
pub fn set_schedule_calendars(c: &CalendarMain, calendars: &[(CalendarId, String)]) {
    *c.schedule_calendars.borrow_mut() = calendars.to_vec();
}

/// Registers `f` to run when the user activates (clicks) an event in any of
/// the main panel's views - the month grid, the time grids, or the agenda -
/// receiving the clicked occurrence so the caller can open its editor. In the
/// Month/Split grids a single click peeks instead (see
/// [`connect_event_peeked`]); this fires on their double-click.
/// One subscriber is expected (the window); a later registration replaces it.
pub fn connect_event_activated(c: &CalendarMain, f: impl Fn(EventOccurrence) + 'static) {
    *c.on_activate.borrow_mut() = Some(Rc::new(f));
}

/// Registers `f` to run when the user single-clicks an event chip in the
/// Month/Split grids (UX-56), receiving the clicked occurrence and the chip
/// widget to anchor a peek popover to. A double-click fires
/// [`connect_event_activated`] (the full editor) instead.
/// One subscriber is expected (the window); a later registration replaces it.
pub fn connect_event_peeked(c: &CalendarMain, f: impl Fn(EventOccurrence, gtk::Widget) + 'static) {
    *c.on_peek.borrow_mut() = Some(Rc::new(f));
}

/// Registers `f` to run when the user drags an event chip to a new time in
/// any drag-capable view (the Day/Week/Work week grids and the Month/Split
/// grids), receiving the dragged occurrence, its resolved new start/end (UTC)
/// and whether the drag was a copy (Ctrl held at the press, CAL-30) so the
/// caller can persist the move/resize - or clone - through the session.
/// One subscriber is expected (the window); a later registration replaces it.
pub fn connect_event_dragged(c: &CalendarMain, f: impl Fn(EventOccurrence, DateTime<Utc>, DateTime<Utc>, bool) + 'static) {
    *c.on_drag.borrow_mut() = Some(Rc::new(f));
}

/// Registers `f` to run when the user right-clicks an event in any view (item
/// 3.12 / CAL-31), receiving the clicked occurrence, the widget to anchor the
/// menu to, and - on a Cairo-drawn surface - the click point within it. One
/// subscriber is expected (the window); a later registration replaces it.
pub fn connect_event_context_menu(c: &CalendarMain, f: impl Fn(EventOccurrence, gtk::Widget, Option<(f64, f64)>) + 'static) {
    *c.on_context.borrow_mut() = Some(Rc::new(f));
}

/// Registers `f` to run when the user clicks a highlighted slot range in the
/// Day or Week grids (selected by a click, or a click-and-drag), receiving
/// the range as `(start_date, start_minutes, end_date, end_minutes)` so the
/// caller can open a new-event editor spanning that exact time. One
/// subscriber is expected (the window); a later registration replaces it.
pub fn connect_slot_activated(c: &CalendarMain, f: impl Fn(NaiveDate, i64, NaiveDate, i64) + 'static) {
    *c.on_slot_activate.borrow_mut() = Some(Rc::new(f));
}

/// Registers `f` to run when the user clicks a day cell in the Month grid
/// (both the Month and Split views), receiving the clicked local date so the
/// caller can re-anchor the whole panel to it - the large grid's equivalent
/// of the sidebar mini-calendar's day buttons (whose own registration keeps
/// the name `connect_day_selected`, hence this one's `_main` suffix).
pub fn connect_main_day_selected(c: &CalendarMain, f: impl Fn(NaiveDate) + 'static) {
    let callback = Rc::new(f);
    c.month.on_day_selected.borrow_mut().push(callback.clone());
    c.split.month.on_day_selected.borrow_mut().push(callback);
}

/// Registers `f` to run when the user clicks an already-selected (highlighted)
/// day cell in the Month or Split grid, receiving the clicked local date so
/// the caller can open a new-event editor for that day - the second click of
/// the grid's select-then-edit interaction.
pub fn connect_main_day_activated(c: &CalendarMain, f: impl Fn(NaiveDate) + 'static) {
    let callback = Rc::new(f);
    *c.month.on_day_activate.borrow_mut() = Some(callback.clone());
    *c.split.month.on_day_activate.borrow_mut() = Some(callback);
}

/// Moves the anchor by `by` steps in the active view's natural unit: a day
/// for Day/Agenda, a week for Week/Work week, a month for Month/Split.
pub fn step(c: &CalendarMain, by: i64) {
    let anchor = *c.anchor.borrow();
    let new_anchor = match active_view(c) {
        "day" | "list" => anchor + chrono::Duration::days(by),
        "workweek" | "week" => anchor + chrono::Duration::days(by * 7),
        _ if by > 0 => anchor.checked_add_months(chrono::Months::new(1)).unwrap_or(anchor),
        _ => anchor.checked_sub_months(chrono::Months::new(1)).unwrap_or(anchor),
    };
    set_anchor(c, new_anchor);
}

/// Re-anchors every view on today (Month shows today's month, Week this week,
/// Day today, etc.).
pub fn go_today(c: &CalendarMain) {
    set_anchor(c, today());
}

fn refresh(c: &CalendarMain) {
    let anchor = *c.anchor.borrow();
    let occurrences = c.occurrences.borrow();
    let colors = c.colors.borrow();
    let on_activate = c.on_activate.borrow().clone();
    let on_peek = c.on_peek.borrow().clone();
    let on_slot_activate = c.on_slot_activate.borrow().clone();
    let on_drag = c.on_drag.borrow().clone();

    set_month(&c.month, anchor);
    set_month_occurrences(&c.month, &occurrences, &colors, on_activate.clone(), on_drag.clone(), on_peek.clone());
    set_month(&c.split.month, anchor);
    set_month_occurrences(&c.split.month, &occurrences, &colors, on_activate.clone(), on_drag.clone(), on_peek);

    set_time_grid(&c.workweek, anchor, &occurrences, &colors, on_activate.clone(), on_slot_activate.clone(), on_drag.clone());
    set_time_grid(&c.week, anchor, &occurrences, &colors, on_activate.clone(), on_slot_activate.clone(), on_drag.clone());
    set_time_grid(&c.day, anchor, &occurrences, &colors, on_activate.clone(), on_slot_activate, on_drag.clone());

    set_agenda(&c.agenda, anchor, list_window_end(anchor), &occurrences, &colors, on_activate.clone());
    set_agenda(&c.split.agenda, anchor, last_of_month(anchor), &occurrences, &colors, on_activate.clone());
    set_schedule(&c.schedule, anchor, &occurrences, &colors, &c.schedule_calendars.borrow(), on_activate);

    c.header_label.set_label(&header_text(c));
}

fn header_text(c: &CalendarMain) -> String {
    let anchor = *c.anchor.borrow();
    match active_view(c) {
        "day" => anchor.format("%A %d %B %Y").to_string(),
        "workweek" | "week" => {
            let weekdays = if active_view(c) == "workweek" {
                c.workweek.weekdays.borrow().clone()
            } else {
                c.week.weekdays.borrow().clone()
            };
            let start = week_start(anchor, weekdays[0]);
            let end = start + chrono::Duration::days(weekdays.len() as i64 - 1);
            if start.year() == end.year() {
                format!("{} – {}", start.format("%d %b"), end.format("%d %b %Y"))
            } else {
                format!("{} – {}", start.format("%d %b %Y"), end.format("%d %b %Y"))
            }
        }
        "split" => format!("Split · {}", anchor.format("%d %b %Y")),
        "list" => format!("List · {}", anchor.format("%d %b %Y")),
        "schedule" => format!("Schedule · {}", anchor.format("%d %b %Y")),
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
        let time = format_event_time(&local_span(occ).0);
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

/// A display-zone wall-clock time as a compact 12-hour timestamp with
/// minutes: `9:30am`, `5:00pm`.
fn format_event_time(local: &NaiveDateTime) -> String {
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
    /// First day of the week (the `calendar-week-start` preference) for the
    /// grid's column order and its 6-week window. Shared with the button
    /// closures so [`set_mini_week_start`] applies a change live (item 3.1).
    week_start: Rc<Cell<chrono::Weekday>>,
    /// The weekday header labels, relabelled on a week-start change.
    weekday_labels: Vec<gtk::Label>,
}

pub fn build_mini(week_start: chrono::Weekday) -> MiniCalendar {
    let week_start = Rc::new(Cell::new(week_start));
    let anchor_month = Rc::new(RefCell::new(first_of_month(today())));
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
    let mut weekday_labels = Vec::with_capacity(7);
    for (col, label) in ordered_weekday_labels(week_start.get()).into_iter().enumerate() {
        let weekday_label = gtk::Label::builder().label(label).css_classes(["dim-label", "caption"]).build();
        grid.attach(&weekday_label, col as i32, 0, 1, 1);
        weekday_labels.push(weekday_label);
    }

    let mut day_buttons = Vec::with_capacity(42);
    for row in 0..6 {
        for col in 0..7 {
            let index = (row * 7 + col) as i64;
            let button = gtk::Button::builder().css_classes(["flat"]).build();
            {
                let anchor_month = anchor_month.clone();
                let on_day_selected = on_day_selected.clone();
                let week_start = week_start.clone();
                button.connect_clicked(move |_| {
                    let month = *anchor_month.borrow();
                    let date = first_grid_day(month, week_start.get()) + chrono::Duration::days(index);
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
        week_start: week_start.clone(),
        weekday_labels,
    };

    {
        let mini_month = mini.anchor_month.clone();
        let mini_header = mini.header_label.clone();
        let mini_buttons = mini.day_buttons.clone();
        let mini_days = mini.event_days.clone();
        let week_start = week_start.clone();
        prev_button.connect_clicked(move |_| {
            let current = *mini_month.borrow();
            let event_days = mini_days.borrow();
            let new_month = current.checked_sub_months(chrono::Months::new(1)).unwrap_or(current);
            relabel_mini(&mini_month, &mini_header, &mini_buttons, new_month, &event_days, week_start.get());
        });
    }
    {
        let mini_month = mini.anchor_month.clone();
        let mini_header = mini.header_label.clone();
        let mini_buttons = mini.day_buttons.clone();
        let mini_days = mini.event_days.clone();
        let week_start = week_start.clone();
        next_button.connect_clicked(move |_| {
            let current = *mini_month.borrow();
            let event_days = mini_days.borrow();
            let new_month = current.checked_add_months(chrono::Months::new(1)).unwrap_or(current);
            relabel_mini(&mini_month, &mini_header, &mini_buttons, new_month, &event_days, week_start.get());
        });
    }

    let initial_month = *mini.anchor_month.borrow();
    let initial_days = mini.event_days.borrow().clone();
    relabel_mini(&mini.anchor_month, &mini.header_label, &mini.day_buttons, initial_month, &initial_days, week_start.get());
    mini
}

/// Changes the mini grid's first weekday (item 3.1): relabels the weekday
/// header and re-lays the displayed month.
pub fn set_mini_week_start(mc: &MiniCalendar, week_start: chrono::Weekday) {
    mc.week_start.set(week_start);
    for (label, text) in mc.weekday_labels.iter().zip(ordered_weekday_labels(week_start)) {
        label.set_label(text);
    }
    let month = *mc.anchor_month.borrow();
    let event_days = mc.event_days.borrow().clone();
    relabel_mini(&mc.anchor_month, &mc.header_label, &mc.day_buttons, month, &event_days, week_start);
}

/// Re-points the mini grid at `month`'s month and records which local dates
/// within it have events (`event_days`) so their day buttons render bold.
pub fn set_mini_month(mc: &MiniCalendar, month: NaiveDate, event_days: &HashSet<NaiveDate>) {
    *mc.event_days.borrow_mut() = event_days.clone();
    relabel_mini(&mc.anchor_month, &mc.header_label, &mc.day_buttons, month, event_days, mc.week_start.get());
}

/// Updates which dates in the currently-displayed month have events, without
/// moving the month - used when a `SyncMonth` fetch lands for the month the
/// mini grid is already showing.
pub fn set_mini_event_days(mc: &MiniCalendar, event_days: &HashSet<NaiveDate>) {
    *mc.event_days.borrow_mut() = event_days.clone();
    let month = *mc.anchor_month.borrow();
    relabel_mini(&mc.anchor_month, &mc.header_label, &mc.day_buttons, month, event_days, mc.week_start.get());
}

/// The first-of-month date the mini grid is currently showing (see
/// [`set_mini_month`]).
pub fn mini_month(mc: &MiniCalendar) -> NaiveDate {
    *mc.anchor_month.borrow()
}

fn relabel_mini(
    anchor_month: &Rc<RefCell<NaiveDate>>,
    header_label: &gtk::Label,
    day_buttons: &[gtk::Button],
    month: NaiveDate,
    event_days: &HashSet<NaiveDate>,
    week_start: chrono::Weekday,
) {
    let month = first_of_month(month);
    *anchor_month.borrow_mut() = month;
    header_label.set_label(&month.format("%B %Y").to_string());

    let today = today();
    let grid_start = first_grid_day(month, week_start);
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

/// One section of the sidebar's "My calendars" checklist: a header row (the
/// account's display name, or a user-defined group's name), its calendars as
/// the checkable rows below it, and an optional status line rendered while an
/// account section hasn't delivered any calendars yet (so a connected-but-silent
/// account is never just blank).
pub struct CalendarSection {
    /// `Some(group id)` for a user-defined group (item 3.13 / CAL-33): its
    /// header carries a ⋯ menu with Rename/Delete. `None` for an account
    /// fallback section, whose header has no menu.
    pub id: Option<String>,
    pub title: String,
    pub calendars: Vec<CalendarInfo>,
    /// Short status line for the sidebar, or `None` once the section has
    /// calendars to list (see [`calendar_account_status_text`]).
    pub status: Option<String>,
}

/// A calendar section before user grouping has been applied: what an account
/// (or a synthetic source) contributes to the checklist.
pub struct CalendarFallbackSection {
    pub title: String,
    pub calendars: Vec<CalendarInfo>,
    pub status: Option<String>,
}

/// A user-defined calendar group as the sidebar needs it (item 3.13 /
/// CAL-33): its id, display name, and member calendar ids in order. The row
/// menu's "Move to group" only reads `id`/`name`; the section builder also
/// reads `calendar_ids`.
pub struct CalendarGroupChoice {
    pub id: String,
    pub name: String,
    pub calendar_ids: Vec<CalendarId>,
}

/// What a checklist row or group header asked for (item 3.13 / CAL-32,
/// CAL-33). The widget layer only names the intent; the window owns the effect
/// (persisting a colour, opening a dialog, routing a rename to the session).
pub enum CalendarSidebarAction {
    /// A palette swatch was picked (`#rrggbb`).
    SetColor(CalendarId, String),
    /// "Custom colour…" - the caller opens a `gtk::ColorDialog`.
    CustomColor(CalendarId),
    /// "Automatic colour" - the caller re-derives the palette/server colour.
    AutomaticColor(CalendarId),
    Rename(CalendarId),
    Share(CalendarId),
    /// Remove from the sidebar (webcal unsubscribes; everything else hides).
    Remove(CalendarId),
    /// Move the calendar into a group (`None` takes it out of every group).
    MoveToGroup(CalendarId, Option<String>),
    NewGroup,
    RenameGroup(String),
    DeleteGroup(String),
    /// Reveal every hidden calendar.
    ShowAll,
}

/// The checklist's action callback, stored by the caller (the window) and
/// handed to every rebuild through `CalendarUiState`.
pub type CalendarSidebarActionHandler = Rc<dyn Fn(CalendarSidebarAction) + 'static>;

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

/// Splits the per-account (and synthetic) fallback sections into the checklist
/// the sidebar renders (item 3.13 / CAL-33): the user-defined groups first, in
/// their own order, then whatever calendars those groups didn't claim, still
/// under their account's header. Pure so the ordering and "an account section
/// disappears once all its calendars are grouped" rules are unit-tested
/// without GTK.
///
/// A user group is rendered even when empty (a group the user is about to fill,
/// or has emptied); an account section whose calendars all moved into groups is
/// dropped, while a still-calendarless account section is kept for its status
/// line.
pub fn build_calendar_sections(fallbacks: &[CalendarFallbackSection], groups: &[CalendarGroupChoice]) -> Vec<CalendarSection> {
    let by_id: HashMap<&CalendarId, &CalendarInfo> = fallbacks.iter().flat_map(|f| f.calendars.iter()).map(|c| (&c.id, c)).collect();
    let mut claimed: HashSet<CalendarId> = HashSet::new();
    let mut sections = Vec::new();
    for group in groups {
        let calendars: Vec<CalendarInfo> = group.calendar_ids.iter().filter_map(|id| by_id.get(id).map(|c| (*c).clone())).collect();
        for calendar in &calendars {
            claimed.insert(calendar.id.clone());
        }
        sections.push(CalendarSection {
            id: Some(group.id.clone()),
            title: group.name.clone(),
            calendars,
            status: None,
        });
    }
    for fallback in fallbacks {
        let remaining: Vec<CalendarInfo> = fallback.calendars.iter().filter(|c| !claimed.contains(&c.id)).cloned().collect();
        if remaining.is_empty() && !fallback.calendars.is_empty() {
            // Every calendar here moved into a user group, so the account
            // header would be an empty stub.
            continue;
        }
        sections.push(CalendarSection {
            id: None,
            title: fallback.title.clone(),
            calendars: remaining,
            status: fallback.status.clone(),
        });
    }
    sections
}

/// Clears `container` and re-renders the "My calendars" checklist from the
/// given sections: one header per section (dim caption, deliberately not the
/// bold `caption-heading`), its calendars as custom coloured toggle rows
/// (name + hand-drawn radio indicator in the calendar's colour, active =
/// currently checked), or a dim status line when an account has none yet. A
/// user-defined group's header also carries a ⋯ menu (Rename/Delete group),
/// and every calendar row a ⋯ menu offering Outlook's per-calendar actions -
/// Change colour, Rename, Share, Remove and Move to group (item 3.13 /
/// CAL-32, CAL-33). The trailing "＋ New group" row creates a group, and a
/// "Show all" row appears while calendars are hidden. Each row's `toggled`
/// handler fires `on_toggle`; every menu item fires `on_action`. A plain
/// rebuildable-list function rather than a stateful struct, matching this
/// file's existing data-in/widget-state-out convention - callers own the
/// actual checked/colour/group state and just ask for a fresh render of it.
///
/// Every calendar's row is tinted with its assigned colour from `colors` (the
/// colour normally lives in `calendar_colors::load`, but this function only
/// needs the map - the caller resolves/persists assignments). The same pass
/// writes the matching `.calendar-event-<hex>` chip rules and the colour
/// picker's swatch rules into `check_colors`, a display-level provider owned
/// by the caller.
#[allow(clippy::too_many_arguments)]
pub fn rebuild_calendar_checklist(
    container: &gtk::Box,
    sections: &[CalendarSection],
    checked: &HashSet<CalendarId>,
    colors: &HashMap<CalendarId, String>,
    check_colors: &gtk::CssProvider,
    on_toggle: impl Fn(CalendarId, bool) + 'static + Clone,
    on_action: CalendarSidebarActionHandler,
    groups: &[CalendarGroupChoice],
    hidden_count: usize,
) {
    clear_children(container);
    let mut css = String::new();
    if sections.is_empty() {
        let placeholder = gtk::Label::builder()
            .label("No calendars connected")
            .css_classes(["dim-label", "caption"])
            .xalign(0.0)
            .build();
        container.append(&placeholder);
    }
    for section in sections {
        let header_row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(2).build();
        let header = gtk::Label::builder()
            .label(&section.title)
            .css_classes(["dim-label", "caption", "calendar-account-header"])
            .xalign(0.0)
            .hexpand(true)
            .build();
        // A group header names a group, not an account, so it isn't private
        // data; an account header is the account's name/address.
        if section.id.is_none() {
            crate::privacy::mark(&header);
        }
        header_row.append(&header);
        if let Some(group_id) = &section.id {
            header_row.append(&build_group_menu(group_id, &on_action));
        }
        container.append(&header_row);
        if section.calendars.is_empty() {
            if let Some(status) = &section.status {
                let label = gtk::Label::builder().label(status).css_classes(["dim-label", "caption"]).xalign(0.0).wrap(true).build();
                container.append(&label);
            }
            continue;
        }
        for calendar in &section.calendars {
            let color = colors.get(&calendar.id).map(String::as_str).unwrap_or(calendar_colors::DEFAULT_CHECK_COLOR);
            css.push_str(&calendar_color_css(color));
            let id = calendar.id.clone();
            let on_toggle = on_toggle.clone();
            let toggle = calendar_toggle_row(&calendar.display_name, color, checked.contains(&calendar.id), move |is_checked| {
                on_toggle(id.clone(), is_checked)
            });
            let row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(2).build();
            toggle.set_hexpand(true);
            row.append(&toggle);
            row.append(&build_calendar_row_menu(calendar, groups, &on_action));
            container.append(&row);
        }
    }
    // The colour picker's swatch grid: one background rule per palette colour.
    // Written into the same provider the chip rules use, so it is (re)parsed
    // once per checklist rebuild.
    for hex in calendar_colors::CALENDAR_PALETTE {
        css.push_str(&format!(".calendar-swatch-{} {{ background-color: {hex}; }}\n", hex.trim_start_matches('#')));
    }
    append_checklist_footer(container, &on_action, hidden_count);
    check_colors.load_from_string(&css);
}

/// The checklist's trailing controls: "＋ New group" always, and a "Show all"
/// row while any calendar is hidden (item 3.13 / CAL-33, CAL-32).
fn append_checklist_footer(container: &gtk::Box, on_action: &CalendarSidebarActionHandler, hidden_count: usize) {
    let new_group = gtk::Button::builder()
        .label("+ New group")
        .css_classes(["flat", "dim-label"])
        .halign(gtk::Align::Start)
        .tooltip_text("Create a calendar group")
        .build();
    {
        let on_action = on_action.clone();
        new_group.connect_clicked(move |_| on_action(CalendarSidebarAction::NewGroup));
    }
    container.append(&new_group);
    if hidden_count > 0 {
        let show_all = gtk::Button::builder()
            .label("Show all")
            .css_classes(["flat", "dim-label"])
            .halign(gtk::Align::Start)
            .tooltip_text("Show calendars removed from the list")
            .build();
        {
            let on_action = on_action.clone();
            show_all.connect_clicked(move |_| on_action(CalendarSidebarAction::ShowAll));
        }
        container.append(&show_all);
    }
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

/// One calendar row's ⋯ menu (UX-52, item 3.13 / CAL-32, CAL-33): Outlook's
/// per-calendar actions - Change colour (palette swatches + Automatic + Custom),
/// Rename, Share, Remove, and Move to group. Built through the shared
/// [`ContextMenu`] builder (item 0.5), like the folder row menu, so the
/// swatches and action wiring match the rest of the app. The menu is rebuilt
/// with its row on every checklist refresh, so it captures its calendar
/// directly rather than sharing a mutable cursor slot.
fn build_calendar_row_menu(calendar: &CalendarInfo, groups: &[CalendarGroupChoice], on_action: &CalendarSidebarActionHandler) -> gtk::MenuButton {
    let button = gtk::MenuButton::builder()
        .icon_name("view-more-symbolic")
        .tooltip_text("More options")
        .css_classes(["flat"])
        .valign(gtk::Align::Center)
        .build();
    let dispatch: Dispatch = {
        let on_action = on_action.clone();
        let id = calendar.id.clone();
        Rc::new(move |name, param| {
            let text = param.and_then(|p| p.get::<String>());
            let action = match name {
                "color" => CalendarSidebarAction::SetColor(id.clone(), text.unwrap_or_default()),
                "color-auto" => CalendarSidebarAction::AutomaticColor(id.clone()),
                "color-custom" => CalendarSidebarAction::CustomColor(id.clone()),
                "rename" => CalendarSidebarAction::Rename(id.clone()),
                "share" => CalendarSidebarAction::Share(id.clone()),
                "remove" => CalendarSidebarAction::Remove(id.clone()),
                "move-group" => CalendarSidebarAction::MoveToGroup(id.clone(), text.filter(|group| !group.is_empty())),
                "new-group" => CalendarSidebarAction::NewGroup,
                _ => return,
            };
            on_action(action);
        })
    };

    let mut move_menu = MenuDescription::new().section().action_with_target("No group", "calendar.move-group", String::new());
    for group in groups {
        move_menu = move_menu.action_with_target(group.name.clone(), "calendar.move-group", group.id.clone());
    }
    move_menu = move_menu.section().action("New group…", "calendar.new-group");

    let description = MenuDescription::new()
        .section()
        .submenu(
            "Change colour",
            MenuDescription::new()
                .section()
                .custom("colors")
                .section()
                .action("Automatic colour", "calendar.color-auto")
                .action("Custom colour…", "calendar.color-custom"),
        )
        .section()
        .action("Rename…", "calendar.rename")
        .action("Share…", "calendar.share")
        .action("Remove", "calendar.remove")
        .section()
        .submenu("Move to group", move_menu);

    let specs = [
        ActionSpec::parameterised("color", gtk::glib::VariantTy::STRING),
        ActionSpec::simple("color-auto"),
        ActionSpec::simple("color-custom"),
        ActionSpec::simple("rename"),
        ActionSpec::simple("share"),
        ActionSpec::simple("remove"),
        ActionSpec::parameterised("move-group", gtk::glib::VariantTy::STRING),
        ActionSpec::simple("new-group"),
    ];
    let menu = ContextMenu::new(&description, &specs, dispatch);
    menu.attach_button(&button, "calendar");

    // The palette swatches are the "Change colour" submenu's custom child (the
    // folder menu's pattern); clicking one applies the colour and closes the
    // menu. Their background rules are written into `check_colors` by
    // [`rebuild_calendar_checklist`].
    let swatch_grid = gtk::Grid::builder()
        .column_spacing(6)
        .row_spacing(6)
        .margin_start(12)
        .margin_end(12)
        .margin_top(6)
        .margin_bottom(6)
        .build();
    for (index, hex) in calendar_colors::CALENDAR_PALETTE.iter().enumerate() {
        let swatch = gtk::Button::builder()
            .css_classes(["calendar-color-swatch", &format!("calendar-swatch-{}", hex.trim_start_matches('#'))])
            .tooltip_text(*hex)
            .build();
        {
            let menu = menu.clone();
            let on_action = on_action.clone();
            let id = calendar.id.clone();
            let hex = hex.to_string();
            swatch.connect_clicked(move |_| {
                menu.popdown();
                on_action(CalendarSidebarAction::SetColor(id.clone(), hex.clone()));
            });
        }
        swatch_grid.attach(&swatch, (index % 5) as i32, (index / 5) as i32, 1, 1);
    }
    menu.add_child(&swatch_grid, "colors");
    button
}

/// A user-defined group header's ⋯ menu (item 3.13 / CAL-33): Rename and
/// Delete. Deleting a group never deletes its calendars - they return to their
/// account's fallback section - so it is safe without a confirmation dialog
/// (the caller offers an Undo).
fn build_group_menu(group_id: &str, on_action: &CalendarSidebarActionHandler) -> gtk::MenuButton {
    let button = gtk::MenuButton::builder()
        .icon_name("view-more-symbolic")
        .tooltip_text("More options")
        .css_classes(["flat"])
        .valign(gtk::Align::Center)
        .build();
    let dispatch: Dispatch = {
        let on_action = on_action.clone();
        let group_id = group_id.to_string();
        Rc::new(move |name, _| match name {
            "group-rename" => on_action(CalendarSidebarAction::RenameGroup(group_id.clone())),
            "group-delete" => on_action(CalendarSidebarAction::DeleteGroup(group_id.clone())),
            _ => {}
        })
    };
    let description = MenuDescription::new()
        .section()
        .action("Rename group…", "calendar.group-rename")
        .action("Delete group", "calendar.group-delete");
    let specs = [ActionSpec::simple("group-rename"), ActionSpec::simple("group-delete")];
    let menu = ContextMenu::new(&description, &specs, dispatch);
    menu.attach_button(&button, "calendar");
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

/// The calendar view's left sidebar: a mini month-picker, an "Add calendar"
/// button (opening the subscribe/import/manage dialog - the caller wires it),
/// and a "My calendars" checklist (populated later by the caller via
/// `rebuild_calendar_checklist`, once accounts have actually reported which
/// calendars exist).
pub struct CalendarSidebar {
    pub root: gtk::Widget,
    pub mini_calendar: MiniCalendar,
    pub calendar_list_box: gtk::Box,
    /// The sidebar's "Add calendar" entry point; `build_sidebar` creates it
    /// enabled but unwired - the caller connects `clicked` to the subscribe/
    /// import/manage dialog, which needs the session plumbing this module
    /// deliberately doesn't have.
    pub add_calendar_button: gtk::Button,
}

pub fn build_sidebar(week_start: chrono::Weekday) -> CalendarSidebar {
    let mini_calendar = build_mini(week_start);
    mini_calendar.root.add_css_class("mini-calendar-sidebar");

    let add_calendar_button = gtk::Button::builder().label("Add calendar").css_classes(["flat"]).halign(gtk::Align::Start).build();

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
        add_calendar_button,
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

/// How many days forward the standalone List view looks from its anchor: a
/// rolling window (Outlook's List is an upcoming-events list), rather than the
/// Split view's "rest of this month".
const LIST_WINDOW_DAYS: i64 = 30;

/// The last day the standalone List view shows for `anchor`.
fn list_window_end(anchor: NaiveDate) -> NaiveDate {
    anchor + chrono::Duration::days(LIST_WINDOW_DAYS)
}

/// The first day of the grid's 6-week window: on or before the 1st of `month`,
/// on the configured `week_start` weekday.
fn first_grid_day(month: NaiveDate, week_start: chrono::Weekday) -> NaiveDate {
    let offset = (crate::settings::weekday_sun_index(month.weekday()) - crate::settings::weekday_sun_index(week_start)).rem_euclid(7);
    month - chrono::Duration::days(offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The second-zone gutter (CAL-10) labels each primary hour row with the
    /// same instant elsewhere, on the first shown date: whole-hour offsets in
    /// the compact style, half-hour ones with minutes, and DST on either side
    /// respected for that date.
    #[test]
    fn secondary_gutter_labels_convert_each_hour_row() {
        use crate::calendar_tz::DisplayZone;
        let sydney = DisplayZone::Named(chrono_tz::Australia::Sydney);
        // 12 Oct 2026: Sydney is on AEDT (+11), so 9am there is 22:00 UTC the
        // day before.
        let day = NaiveDate::from_ymd_opt(2026, 10, 12).unwrap();
        assert_eq!(secondary_gutter_label(9, day, sydney, chrono_tz::UTC), "10pm");
        // ...and 3:30am in Kolkata (+5:30).
        assert_eq!(secondary_gutter_label(9, day, sydney, chrono_tz::Asia::Kolkata), "3:30am");
        // In July Sydney is on AEST (+10): 9am is 23:00 UTC.
        let winter = NaiveDate::from_ymd_opt(2026, 7, 1).unwrap();
        assert_eq!(secondary_gutter_label(9, winter, sydney, chrono_tz::UTC), "11pm");
    }

    /// The business-hours band only goes on work-day columns.
    #[test]
    fn business_band_covers_only_work_days() {
        let week = test_week(); // Sun 2 Aug .. Sat 8 Aug 2026
        let weekdays = crate::settings::CalendarPreferences::default().work_days;
        assert_eq!(business_columns(&week, &weekdays), vec![1, 2, 3, 4, 5]);
        assert_eq!(business_columns(&week[..1], &weekdays), Vec::<usize>::new(), "a Sunday Day view gets no band");
        assert_eq!(business_columns(&week, &[chrono::Weekday::Sun]), vec![0]);
    }

    /// CAL-05/UX-57: every show-as state gets its own chip look.
    #[test]
    fn chip_styles_map_every_show_as_state() {
        assert_eq!(chip_style(BusyStatus::Busy), ChipStyle::Solid);
        assert_eq!(chip_style(BusyStatus::Free), ChipStyle::Hollow);
        assert_eq!(chip_style(BusyStatus::Tentative), ChipStyle::Hatched);
        assert_eq!(chip_style(BusyStatus::Oof), ChipStyle::Oof);
        assert_eq!(chip_style(BusyStatus::WorkingElsewhere), ChipStyle::Dotted);
    }

    /// An Away chip is 55% Outlook's OOF purple over the calendar's colour:
    /// black shifts to the purple tint, and a coloured calendar still shows
    /// through.
    #[test]
    fn away_chips_shift_towards_outlooks_oof_purple() {
        let (r, g, b) = oof_fill(0.0, 0.0, 0.0);
        assert!((r - 0.265).abs() < 0.01, "{r}");
        assert!((g - 0.095).abs() < 0.01, "{g}");
        assert!((b - 0.412).abs() < 0.01, "{b}");
        let (red_r, _, red_b) = oof_fill(1.0, 0.0, 0.0);
        assert!(red_r > r, "a red calendar keeps more red");
        assert!((red_b - b).abs() < 1e-9, "the purple's blue channel is fixed");
    }

    /// Live preference changes (item 3.1): a Work week change rebuilds the
    /// column headers to the new count, and a week-start change relabels the
    /// mini month's weekday row.
    #[test]
    fn grids_apply_week_preferences_live() {
        if !crate::gtk_test::gtk_ready() {
            return;
        }
        use chrono::Weekday::*;
        let grid = build_time_grid(&[Mon, Tue, Wed, Thu, Fri], false, &Rc::new(RefCell::new(None)));
        assert_eq!(grid.headers.borrow().len(), 5);
        grid.set_weekdays(&[Sun, Mon, Tue, Wed]);
        assert_eq!(grid.headers.borrow().len(), 4);
        assert_eq!(*grid.weekdays.borrow(), vec![Sun, Mon, Tue, Wed]);
        grid.set_zones(Some(chrono_tz::UTC));
        assert_eq!(grid.data.gutter.get(), 2.0 * HOUR_GUTTER_WIDTH);
        grid.set_zones(None);
        assert_eq!(grid.data.gutter.get(), HOUR_GUTTER_WIDTH);

        let mini = build_mini(Sun);
        assert_eq!(mini.weekday_labels[0].label(), weekday_short(Sun));
        set_mini_week_start(&mini, Mon);
        assert_eq!(mini.weekday_labels[0].label(), weekday_short(Mon));
        assert_eq!(mini.weekday_labels[6].label(), weekday_short(Sun));
    }
    use chrono::NaiveDateTime;
    use lookout_core::EventUid;

    #[test]
    fn first_grid_day_lands_on_sunday_on_or_before_the_1st() {
        // 2026-07-01 is a Wednesday; a Sunday-first grid starts 2026-06-28.
        let month = NaiveDate::from_ymd_opt(2026, 7, 1).unwrap();
        let grid_start = first_grid_day(month, chrono::Weekday::Sun);
        assert_eq!(grid_start, NaiveDate::from_ymd_opt(2026, 6, 28).unwrap());
        assert_eq!(grid_start.weekday(), chrono::Weekday::Sun);
        // The same month on a Monday-first grid starts 2026-06-29.
        assert_eq!(first_grid_day(month, chrono::Weekday::Mon), NaiveDate::from_ymd_opt(2026, 6, 29).unwrap());
    }

    #[test]
    fn first_grid_day_is_unchanged_when_month_already_starts_on_the_week_start() {
        // 2026-11-01 is itself a Sunday.
        let month = NaiveDate::from_ymd_opt(2026, 11, 1).unwrap();
        assert_eq!(first_grid_day(month, chrono::Weekday::Sun), month);
    }

    #[test]
    fn week_days_and_labels_start_at_the_configured_day() {
        assert_eq!(week_days(chrono::Weekday::Sun).first().copied(), Some(chrono::Weekday::Sun));
        assert_eq!(week_days(chrono::Weekday::Mon).first().copied(), Some(chrono::Weekday::Mon));
        // Starting at Saturday wraps through Sunday back to Friday.
        assert_eq!(
            week_days(chrono::Weekday::Sat),
            vec![
                chrono::Weekday::Sat,
                chrono::Weekday::Sun,
                chrono::Weekday::Mon,
                chrono::Weekday::Tue,
                chrono::Weekday::Wed,
                chrono::Weekday::Thu,
                chrono::Weekday::Fri
            ]
        );
        assert_eq!(ordered_weekday_labels(chrono::Weekday::Mon)[0], "Mon");
        assert_eq!(ordered_weekday_labels(chrono::Weekday::Sat), ["Sat", "Sun", "Mon", "Tue", "Wed", "Thu", "Fri"]);
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
        let at = |hour: u32, minute: u32| NaiveDate::from_ymd_opt(2026, 8, 6).unwrap().and_hms_opt(hour, minute, 0).unwrap();
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

    /// Builds an occurrence whose start/end land on the given naive times in
    /// the display zone regardless of the test host's timezone - timed spans
    /// round-trip through the (system) display zone on the way in and out of
    /// [`compute_time_grid_chips`], all-day spans are UTC-midnight dates.
    fn occ(summary: &str, start: NaiveDateTime, end: NaiveDateTime, all_day: bool) -> EventOccurrence {
        let zone = crate::calendar_tz::display_zone();
        EventOccurrence {
            uid: EventUid(summary.to_string()),
            calendar_id: CalendarId("test".to_string()),
            summary: Some(summary.to_string()),
            description: None,
            location: None,
            start: zone.occurrence_from_local(start, all_day),
            end: zone.occurrence_from_local(end, all_day),
            all_day,
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
            reminder_minutes_before: None,
            conference_url: None,
        }
    }

    /// UX-56: the peek's one-line "when" text - timed within a day, cross-day,
    /// and the all-day single- and multi-day forms (all-day ends are
    /// exclusive, so the displayed last day is one before `end`).
    #[test]
    fn peek_when_text_formats_timed_and_all_day_spans() {
        let day = NaiveDate::from_ymd_opt(2026, 8, 6).unwrap();
        let timed = occ("Standup", day.and_hms_opt(9, 0, 0).unwrap(), day.and_hms_opt(10, 0, 0).unwrap(), false);
        assert_eq!(peek_when_text(&timed), "Thu 06 Aug 2026 · 9:00am – 10:00am");

        let overnight = occ(
            "Red-eye",
            day.and_hms_opt(23, 0, 0).unwrap(),
            (day + chrono::Duration::days(1)).and_hms_opt(2, 0, 0).unwrap(),
            false,
        );
        assert_eq!(peek_when_text(&overnight), "Thu 06 Aug 11:00pm – Fri 07 Aug 2:00am");

        let all_day = occ(
            "Holiday",
            day.and_hms_opt(0, 0, 0).unwrap(),
            (day + chrono::Duration::days(1)).and_hms_opt(0, 0, 0).unwrap(),
            true,
        );
        assert_eq!(peek_when_text(&all_day), "All day · Thu 06 Aug 2026");

        let multi = occ(
            "Conference",
            day.and_hms_opt(0, 0, 0).unwrap(),
            (day + chrono::Duration::days(3)).and_hms_opt(0, 0, 0).unwrap(),
            true,
        );
        assert_eq!(peek_when_text(&multi), "All day · Thu 06 Aug – Sat 08 Aug");
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
            assert!(local_span(&occ).0.date() <= covered);
            assert!(covered < local_span(&occ).1.date());
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

        let (bx, by, bw, bh) = chip_geometry(HOUR_GUTTER_WIDTH, band_chip, 100.0);
        let band_centre = (bx + bw / 2.0, by + bh / 2.0);
        assert_eq!(
            hovered_chip(HOUR_GUTTER_WIDTH, &chips, canvas_width, 7, band_centre.0, band_centre.1, true).map(|i| chips[i].all_day),
            Some(true)
        );
        assert!(hovered_chip(HOUR_GUTTER_WIDTH, &chips, canvas_width, 7, band_centre.0, band_centre.1, false).is_none());

        let (tx, ty, tw, th) = chip_geometry(HOUR_GUTTER_WIDTH, timed_chip, 100.0);
        let timed_centre = (tx + tw / 2.0, ty + th / 2.0);
        assert_eq!(
            hovered_chip(HOUR_GUTTER_WIDTH, &chips, canvas_width, 7, timed_centre.0, timed_centre.1, false).map(|i| chips[i].all_day),
            Some(false)
        );
        assert!(hovered_chip(HOUR_GUTTER_WIDTH, &chips, canvas_width, 7, timed_centre.0, timed_centre.1, true).is_none());
    }

    #[test]
    fn slot_range_spanning_normalizes_the_drag_direction() {
        let mon = NaiveDate::from_ymd_opt(2026, 8, 3).unwrap();
        let tue = NaiveDate::from_ymd_opt(2026, 8, 4).unwrap();
        // Dragged downward in time (Tue 11:00 back to Mon 9:00) still yields
        // the same normalized range as dragging forward.
        let down = SlotRange::spanning(tue, 11 * 60, mon, 9 * 60);
        let up = SlotRange::spanning(mon, 9 * 60, tue, 11 * 60);
        assert_eq!(down, up);
        assert_eq!(down.start_date, mon);
        assert_eq!(down.start_minutes, 9 * 60);
        assert_eq!(down.end_date, tue);
        assert_eq!(down.end_minutes, 11 * 60);
    }

    #[test]
    fn slot_range_contains_includes_the_boundaries_and_skips_outside() {
        let mon = NaiveDate::from_ymd_opt(2026, 8, 3).unwrap();
        let tue = NaiveDate::from_ymd_opt(2026, 8, 4).unwrap();
        let range = SlotRange::spanning(mon, 9 * 60, tue, 11 * 60);
        assert!(range.contains(mon, 9 * 60));
        assert!(range.contains(mon, 13 * 60));
        assert!(range.contains(tue, 11 * 60));
        assert!(range.contains(tue, 0));
        assert!(!range.contains(mon, 8 * 60 + 30));
        assert!(!range.contains(tue, 11 * 60 + 30));
        assert!(!range.contains(NaiveDate::from_ymd_opt(2026, 8, 5).unwrap(), 0));
    }

    #[test]
    fn slot_range_single_covers_exactly_one_slot() {
        let day = NaiveDate::from_ymd_opt(2026, 8, 3).unwrap();
        let range = SlotRange::single(day, 9 * 60);
        assert_eq!(range.start_date, day);
        assert_eq!(range.end_date, day);
        assert_eq!(range.start_minutes, 9 * 60);
        assert_eq!(range.end_minutes, 9 * 60);
        assert!(range.contains(day, 9 * 60));
        assert!(!range.contains(day, 9 * 60 + 30));
        assert!(!range.contains(day, 8 * 60 + 30));
    }

    #[test]
    fn calendar_color_css_parses_as_valid_gtk_css() {
        // `load_from_string` swallows parse errors (logging them to GLib), so
        // assert on `to_str()` instead: a provider that failed to parse the
        // rules round-trips to an empty string. Skipped when the test host has
        // no display to initialise GTK against, or when another test thread
        // owns GTK (the shared `gtk_test::gtk_ready` lock serialises init).
        if !crate::gtk_test::gtk_ready() {
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
        // host has no display to initialise GTK against, or when another test
        // thread owns GTK (the shared `gtk_test::gtk_ready` lock serialises
        // init).
        if !crate::gtk_test::gtk_ready() {
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
        // display to initialise GTK against, or when another test thread owns
        // GTK (the shared `gtk_test::gtk_ready` lock serialises init).
        if !crate::gtk_test::gtk_ready() {
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

    #[test]
    fn slot_from_point_ignores_the_hour_gutter() {
        // Any y inside the gutter column maps to no slot at all.
        assert!(slot_from_point(HOUR_GUTTER_WIDTH, HOUR_GUTTER_WIDTH - 1.0, 300.0, 752.0, 7).is_none());
        assert!(slot_from_point(HOUR_GUTTER_WIDTH, 0.0, 0.0, 752.0, 7).is_none());
    }

    #[test]
    fn slot_from_point_maps_columns_across_the_canvas() {
        // 700px of columns over 7 days = 100px per column; column 0 starts at
        // the gutter edge, column 4 at x = 52 + 4*100.
        let (col, _) = slot_from_point(HOUR_GUTTER_WIDTH, HOUR_GUTTER_WIDTH + 1.0, 0.0, HOUR_GUTTER_WIDTH + 700.0, 7).unwrap();
        assert_eq!(col, 0);
        let (col, _) = slot_from_point(HOUR_GUTTER_WIDTH, HOUR_GUTTER_WIDTH + 100.0 * 4.0 + 50.0, 0.0, HOUR_GUTTER_WIDTH + 700.0, 7).unwrap();
        assert_eq!(col, 4);
        // A click past the last column's midpoint still lands in the last one.
        let (col, _) = slot_from_point(HOUR_GUTTER_WIDTH, HOUR_GUTTER_WIDTH + 700.0 - 1.0, 0.0, HOUR_GUTTER_WIDTH + 700.0, 7).unwrap();
        assert_eq!(col, 6);
    }

    #[test]
    fn slot_from_point_snaps_minutes_to_the_half_hour() {
        // 48px/hour means 0.8px per minute: 9:12am is 552 minutes in, snapped
        // to the nearest half hour → 9:00 (9:30 is further).
        let y_912 = (9 * 60 + 12) as f64 * TIME_SLOT_HEIGHT / 60.0;
        let (_, minutes) = slot_from_point(HOUR_GUTTER_WIDTH, HOUR_GUTTER_WIDTH + 10.0, y_912, 752.0, 7).unwrap();
        assert_eq!(minutes, 9 * 60);
        // 9:18am snaps up to 9:30.
        let y_918 = (9 * 60 + 18) as f64 * TIME_SLOT_HEIGHT / 60.0;
        let (_, minutes) = slot_from_point(HOUR_GUTTER_WIDTH, HOUR_GUTTER_WIDTH + 10.0, y_918, 752.0, 7).unwrap();
        assert_eq!(minutes, 9 * 60 + 30);
        // 9:00am on the dot snaps to itself.
        let y_9am = 9.0 * TIME_SLOT_HEIGHT;
        let (_, minutes) = slot_from_point(HOUR_GUTTER_WIDTH, HOUR_GUTTER_WIDTH + 10.0, y_9am, 752.0, 7).unwrap();
        assert_eq!(minutes, 9 * 60);
    }

    #[test]
    fn slot_from_point_clamps_to_the_last_startable_slot() {
        // The bottom edge is minute 1440 of the day; the final 30 minutes are
        // never offered as a start so the one-hour default span stays inside
        // the day.
        let (_, minutes) = slot_from_point(HOUR_GUTTER_WIDTH, HOUR_GUTTER_WIDTH + 10.0, 24.0 * TIME_SLOT_HEIGHT, 752.0, 7).unwrap();
        assert_eq!(minutes, 1440 - 30);
        let (_, minutes) = slot_from_point(HOUR_GUTTER_WIDTH, HOUR_GUTTER_WIDTH + 10.0, 23.9 * TIME_SLOT_HEIGHT, 752.0, 7).unwrap();
        assert_eq!(minutes, 1440 - 30);
    }

    // --- Drag-reschedule math.

    fn drag_at(original_start: i64, original_end: i64, mode: DragMode) -> TimeGridDrag {
        TimeGridDrag {
            chip: 0,
            mode,
            original_start,
            original_end,
            live_start: original_start,
            live_end: original_end,
            grab_offset: 0,
            all_day: false,
        }
    }

    #[test]
    fn drag_move_shifts_both_ends_preserving_duration_and_grab_offset() {
        // 9:00-10:00 on Monday (absolute grid minutes of a Sunday-first grid).
        let mut drag = drag_at(1440 + 9 * 60, 1440 + 10 * 60, DragMode::Move);
        drag.grab_offset = 30; // grabbed 30 minutes into the chip
                               // Pointer lands 9:30 Tuesday: the chip start should land 9:00 Tuesday.
        drag_update(&mut drag, 2, 9 * 60 + 30, 7 * 1440);
        assert_eq!(drag.live_start, 2 * 1440 + 9 * 60);
        assert_eq!(drag.live_end, 2 * 1440 + 10 * 60);
    }

    #[test]
    fn drag_move_snaps_to_half_hours() {
        let mut drag = drag_at(1440 + 9 * 60, 1440 + 10 * 60, DragMode::Move);
        // A pointer 13 minutes past the hour lands the start on the hour.
        drag_update(&mut drag, 2, 9 * 60 + 43, 7 * 1440);
        assert_eq!(drag.live_start, 2 * 1440 + 9 * 60 + 30);
        // ...and 7 minutes past lands it on the hour.
        let mut drag = drag_at(1440 + 9 * 60, 1440 + 10 * 60, DragMode::Move);
        drag_update(&mut drag, 2, 9 * 60 + 7, 7 * 1440);
        assert_eq!(drag.live_start, 2 * 1440 + 9 * 60);
    }

    #[test]
    fn drag_move_clamps_to_the_visible_grid_window() {
        // Dragging far past the right edge clamps the chip inside the grid.
        let mut drag = drag_at(0, 60, DragMode::Move);
        drag_update(&mut drag, 6, 23 * 60, 7 * 1440);
        assert_eq!(drag.live_end, 7 * 1440);
        // ...and before the left edge clamps it back to the start.
        let mut drag = drag_at(2 * 1440 + 9 * 60, 2 * 1440 + 10 * 60, DragMode::Move);
        drag_update(&mut drag, 0, 0, 7 * 1440);
        assert_eq!(drag.live_start, 0);
        assert_eq!(drag.live_end, 60);
    }

    #[test]
    fn drag_resize_follows_the_pointer_with_the_start_pinned() {
        let mut drag = drag_at(1440 + 9 * 60, 1440 + 10 * 60, DragMode::ResizeEnd);
        drag_update(&mut drag, 1, 11 * 60 + 40, 7 * 1440);
        assert_eq!(drag.live_start, 1440 + 9 * 60);
        assert_eq!(drag.live_end, 1440 + 11 * 60 + 30);
    }

    #[test]
    fn drag_resize_never_flips_or_underflows_the_minimum_duration() {
        // Dragging the end above the start floors it one snap slot later.
        let mut drag = drag_at(1440 + 9 * 60, 1440 + 10 * 60, DragMode::ResizeEnd);
        drag_update(&mut drag, 1, 8 * 60, 7 * 1440);
        assert_eq!(drag.live_start, 1440 + 9 * 60);
        assert_eq!(drag.live_end, 1440 + 9 * 60 + 30);
        // A resize can extend across midnight into the next day.
        let mut drag = drag_at(1440 + 22 * 60, 1440 + 23 * 60, DragMode::ResizeEnd);
        drag_update(&mut drag, 2, 6 * 60, 7 * 1440);
        assert_eq!(drag.live_start, 1440 + 22 * 60);
        assert_eq!(drag.live_end, 2 * 1440 + 6 * 60);
    }

    #[test]
    fn drag_all_day_moves_by_whole_days() {
        let mut drag = drag_at(2 * 1440, 4 * 1440, DragMode::Move);
        drag.all_day = true;
        drag.grab_offset = 1440; // grabbed on the second day of the span
                                 // Pointer over Tuesday (col 2): the span keeps its grab offset, so it
                                 // lands Monday-Wednesday.
        drag_update(&mut drag, 2, 720, 7 * 1440);
        assert_eq!(drag.live_start, 1440);
        assert_eq!(drag.live_end, 3 * 1440);
    }

    #[test]
    fn drag_times_move_shifts_both_ends_by_the_live_delta() {
        let e = occ("Move", NaiveDateTime::default(), NaiveDateTime::default(), false);
        let drag = TimeGridDrag {
            chip: 0,
            mode: DragMode::Move,
            original_start: 1440 + 9 * 60,
            original_end: 1440 + 10 * 60,
            live_start: 2 * 1440 + 9 * 60 + 30,
            live_end: 2 * 1440 + 10 * 60 + 30,
            grab_offset: 0,
            all_day: false,
        };
        let (start, end) = drag_times(&drag, &e);
        assert_eq!(start - e.start, chrono::Duration::minutes(24 * 60 + 30));
        assert_eq!(end - e.end, chrono::Duration::minutes(24 * 60 + 30));
    }

    #[test]
    fn drag_times_resize_only_shifts_the_end() {
        let e = occ("Resize", NaiveDateTime::default(), NaiveDateTime::default(), false);
        let drag = TimeGridDrag {
            chip: 0,
            mode: DragMode::ResizeEnd,
            original_start: 1440 + 9 * 60,
            original_end: 1440 + 10 * 60,
            live_start: 1440 + 9 * 60,
            live_end: 1440 + 12 * 60,
            grab_offset: 0,
            all_day: false,
        };
        let (start, end) = drag_times(&drag, &e);
        assert_eq!(start, e.start);
        assert_eq!(end - e.end, chrono::Duration::minutes(2 * 60));
    }

    #[test]
    fn drag_ghost_chip_spans_midnight_and_renders_full_last_columns() {
        // 22:00 Monday - 06:00 Tuesday.
        let drag = TimeGridDrag {
            chip: 0,
            mode: DragMode::Move,
            original_start: 1440 + 22 * 60,
            original_end: 1440 + 30 * 60,
            live_start: 1440 + 22 * 60,
            live_end: 2 * 1440 + 6 * 60,
            grab_offset: 0,
            all_day: false,
        };
        let ghost = drag_ghost_chip(&drag, 0);
        assert_eq!((ghost.column, ghost.span, ghost.start_minutes, ghost.end_minutes), (1, 2, 22 * 60, 6 * 60));

        // An end at exactly midnight renders as a full 1440-minute last column.
        let drag = TimeGridDrag {
            chip: 0,
            mode: DragMode::ResizeEnd,
            original_start: 1440 + 9 * 60,
            original_end: 1440 + 10 * 60,
            live_start: 1440 + 9 * 60,
            live_end: 2 * 1440,
            grab_offset: 0,
            all_day: false,
        };
        let ghost = drag_ghost_chip(&drag, 0);
        assert_eq!((ghost.column, ghost.span, ghost.start_minutes, ghost.end_minutes), (1, 1, 9 * 60, 1440));

        // An all-day span keeps whole-day columns.
        let drag = TimeGridDrag {
            chip: 0,
            mode: DragMode::Move,
            original_start: 2 * 1440,
            original_end: 4 * 1440,
            live_start: 3 * 1440,
            live_end: 5 * 1440,
            grab_offset: 0,
            all_day: true,
        };
        let ghost = drag_ghost_chip(&drag, 0);
        assert_eq!((ghost.column, ghost.span, ghost.start_minutes, ghost.end_minutes), (3, 2, 0, 1440));
    }

    #[test]
    fn cell_index_at_point_maps_grid_coordinates_to_day_cells() {
        // A 700x600 grid (7 homogeneous columns of 100px, 6 week rows of 100px).
        assert_eq!(cell_index_at_point(700.0, 600.0, 0.0, 50.0), Some(0));
        assert_eq!(cell_index_at_point(700.0, 600.0, 650.0, 50.0), Some(6));
        assert_eq!(cell_index_at_point(700.0, 600.0, 0.0, 550.0), Some(42 - 7));
        assert_eq!(cell_index_at_point(700.0, 600.0, 650.0, 550.0), Some(41));
        assert_eq!(cell_index_at_point(700.0, 600.0, 0.0, 100.0), Some(7));
        // Outside the grid.
        assert_eq!(cell_index_at_point(700.0, 600.0, -1.0, 100.0), None);
        assert_eq!(cell_index_at_point(700.0, 600.0, 700.0, 100.0), None);
        assert_eq!(cell_index_at_point(700.0, 600.0, 50.0, 600.0), None);
        // Degenerate sizes.
        assert_eq!(cell_index_at_point(0.0, 600.0, 0.0, 0.0), None);
    }

    /// CAL-03: a block's span is its display-zone minutes since local
    /// midnight, with each end clamped to the shown day; all-day events fill
    /// the lane and events wholly outside the day are dropped.
    #[test]
    fn schedule_block_span_clamps_each_end_to_the_day() {
        let day = NaiveDate::from_ymd_opt(2026, 8, 6).unwrap();
        let timed = occ("Standup", day.and_hms_opt(9, 0, 0).unwrap(), day.and_hms_opt(10, 0, 0).unwrap(), false);
        assert_eq!(schedule_block_span(day, &timed), Some((9 * 60, 10 * 60)));

        let all_day = occ("Holiday", day.and_hms_opt(0, 0, 0).unwrap(), (day + Duration::days(1)).and_hms_opt(0, 0, 0).unwrap(), true);
        assert_eq!(schedule_block_span(day, &all_day), Some((0, 1440)));

        // A cross-midnight event is clipped to the shown day.
        let overnight = occ(
            "Red-eye",
            (day - Duration::days(1)).and_hms_opt(22, 0, 0).unwrap(),
            day.and_hms_opt(2, 0, 0).unwrap(),
            false,
        );
        assert_eq!(schedule_block_span(day, &overnight), Some((0, 2 * 60)));

        // An end at exactly midnight renders as the full last minute.
        let to_midnight = occ("Late", day.and_hms_opt(23, 0, 0).unwrap(), (day + Duration::days(1)).and_hms_opt(0, 0, 0).unwrap(), false);
        assert_eq!(schedule_block_span(day, &to_midnight), Some((23 * 60, 1440)));

        // A next-day event does not belong to this day's lane.
        let outside = occ(
            "Later",
            (day + Duration::days(1)).and_hms_opt(1, 0, 0).unwrap(),
            (day + Duration::days(1)).and_hms_opt(2, 0, 0).unwrap(),
            false,
        );
        assert_eq!(schedule_block_span(day, &outside), None);
    }

    /// CAL-03: the axis maps midnight to the gutter edge and 24:00 to the
    /// widget edge, and a block's rectangle hit-tests back to its lane/block.
    #[test]
    fn schedule_axis_and_block_rect_agree_with_hit_testing() {
        let day = NaiveDate::from_ymd_opt(2026, 8, 6).unwrap();
        let width = SCHEDULE_LABEL_WIDTH + 720.0;
        assert_eq!(schedule_x_for_minutes(0.0, width), SCHEDULE_LABEL_WIDTH);
        assert_eq!(schedule_x_for_minutes(1440.0, width), width);

        let block = ScheduleBlock {
            occ: occ("Standup", day.and_hms_opt(9, 0, 0).unwrap(), day.and_hms_opt(10, 0, 0).unwrap(), false),
            start_minutes: 9 * 60,
            end_minutes: 10 * 60,
        };
        let (x, y, w, h) = schedule_block_rect(0, &block, width);
        assert_eq!(y, SCHEDULE_HEADER_HEIGHT + SCHEDULE_BLOCK_INSET);
        assert_eq!(h, SCHEDULE_ROW_HEIGHT - 2.0 * SCHEDULE_BLOCK_INSET);
        assert!(w > 0.0 && x > SCHEDULE_LABEL_WIDTH);

        let state = ScheduleState {
            lanes: vec![ScheduleLane {
                label: "Work".to_string(),
                color: "#3584e4".to_string(),
                blocks: vec![block],
            }],
            date: day,
            work_start: 8,
            work_end: 18,
            work_days: Vec::new(),
        };
        assert_eq!(schedule_block_at(&state, width, x + w / 2.0, y + h / 2.0), Some((0, 0)));
        // Nothing in the hour-axis header.
        assert_eq!(schedule_block_at(&state, width, x + w / 2.0, SCHEDULE_HEADER_HEIGHT - 1.0), None);
    }

    /// CAL-02: the standalone List view looks a rolling 30 days ahead.
    #[test]
    fn list_window_spans_thirty_days_forward() {
        let anchor = NaiveDate::from_ymd_opt(2026, 10, 15).unwrap();
        assert_eq!(list_window_end(anchor), NaiveDate::from_ymd_opt(2026, 11, 14).unwrap());
    }

    /// CAL-02/CAL-03: the List and Schedule pages are reachable through the
    /// shared view switcher and reported back by `active_view`.
    #[test]
    fn view_switcher_reaches_list_and_schedule() {
        if !crate::gtk_test::gtk_ready() {
            return;
        }
        let prefs = crate::settings::CalendarPreferences::default();
        let main = build_main(&prefs);
        set_view(&main, "list");
        assert_eq!(active_view(&main), "list");
        set_view(&main, "schedule");
        assert_eq!(active_view(&main), "schedule");
        set_view(&main, "month");
        assert_eq!(active_view(&main), "month");
    }

    fn calendar(id: &str) -> CalendarInfo {
        CalendarInfo {
            id: CalendarId(id.to_string()),
            account_id: lookout_core::AccountId("account".to_string()),
            display_name: id.to_string(),
            color: None,
            href: format!("/{id}"),
            supports_tasks: false,
            owner: None,
        }
    }

    fn fallback(title: &str, ids: &[&str]) -> CalendarFallbackSection {
        CalendarFallbackSection {
            title: title.to_string(),
            calendars: ids.iter().map(|id| calendar(id)).collect(),
            status: None,
        }
    }

    fn group(id: &str, name: &str, members: &[&str]) -> CalendarGroupChoice {
        CalendarGroupChoice {
            id: id.to_string(),
            name: name.to_string(),
            calendar_ids: members.iter().map(|m| CalendarId(m.to_string())).collect(),
        }
    }

    /// CAL-33: user groups render first, their members in the group's own
    /// order; an account keeps whatever calendars a group didn't claim, and an
    /// account whose calendars all moved drops out.
    #[test]
    fn sections_place_user_groups_before_unclaimed_account_calendars() {
        let fallbacks = vec![fallback("Work account", &["a", "b"]), fallback("Home account", &["c"])];
        let groups = vec![group("group:1", "Favourites", &["c", "a"])];
        let sections = build_calendar_sections(&fallbacks, &groups);

        assert_eq!(sections.len(), 2, "Home account has no calendars left, so it drops out");
        assert_eq!(sections[0].id.as_deref(), Some("group:1"));
        assert_eq!(sections[0].title, "Favourites");
        // Membership follows the group's order (c then a), not the account's.
        assert_eq!(sections[0].calendars.iter().map(|c| c.id.0.as_str()).collect::<Vec<_>>(), vec!["c", "a"]);
        // The work account keeps only its unclaimed calendar.
        assert_eq!(sections[1].id, None);
        assert_eq!(sections[1].title, "Work account");
        assert_eq!(sections[1].calendars.iter().map(|c| c.id.0.as_str()).collect::<Vec<_>>(), vec!["b"]);
    }

    /// A group renders even while empty (a new or emptied group), and an
    /// account with no calendars at all is kept for its status line.
    #[test]
    fn sections_keep_empty_groups_and_status_only_accounts() {
        let fallbacks = vec![CalendarFallbackSection {
            title: "Connecting…".to_string(),
            calendars: vec![],
            status: Some("Connecting…".to_string()),
        }];
        let groups = vec![group("group:1", "New group", &[])];
        let sections = build_calendar_sections(&fallbacks, &groups);

        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].id.as_deref(), Some("group:1"));
        assert!(sections[0].calendars.is_empty());
        assert_eq!(sections[1].title, "Connecting…");
        assert_eq!(sections[1].status.as_deref(), Some("Connecting…"));
    }

    /// With no groups, every fallback section survives unchanged and a stale
    /// member id is simply skipped.
    #[test]
    fn sections_without_groups_keep_every_account_and_skip_stale_members() {
        let fallbacks = vec![fallback("Work account", &["a"]), fallback("Home account", &["c"])];
        let sections = build_calendar_sections(&fallbacks, &[]);
        assert_eq!(sections.len(), 2);
        assert!(sections.iter().all(|s| s.id.is_none()));

        // A group naming a calendar that no longer exists renders empty rather
        // than failing.
        let groups = vec![group("group:1", "Stale", &["gone"])];
        let sections = build_calendar_sections(&fallbacks, &groups);
        assert_eq!(sections.len(), 3, "the empty stale group still renders");
        assert!(sections[0].calendars.is_empty());
        assert_eq!(sections[1].title, "Work account");
        assert_eq!(sections[2].title, "Home account");
    }

    /// Every widget under `root`, so a test can find the labels and buttons a
    /// rebuild produced without depending on the exact tree shape.
    fn collect_widgets(root: &gtk::Widget, out: &mut Vec<gtk::Widget>) {
        out.push(root.clone());
        let mut child = root.first_child();
        while let Some(current) = child {
            collect_widgets(&current, out);
            child = current.next_sibling();
        }
    }

    /// CAL-32/CAL-33: a rebuild renders every section's header, each calendar
    /// row's ⋯ menu, and the footer controls (whose clicks reach the handler) -
    /// and the colour rules land in the shared provider.
    #[test]
    fn checklist_renders_sections_menus_and_footer_actions() {
        if !crate::gtk_test::gtk_ready() {
            return;
        }
        let sections = vec![
            CalendarSection {
                id: Some("group:1".to_string()),
                title: "Favourites".to_string(),
                calendars: vec![calendar("a")],
                status: None,
            },
            CalendarSection {
                id: None,
                title: "Work account".to_string(),
                calendars: vec![calendar("b")],
                status: None,
            },
            CalendarSection {
                id: Some("group:2".to_string()),
                title: "Empty".to_string(),
                calendars: vec![],
                status: None,
            },
        ];
        let checked: HashSet<CalendarId> = sections.iter().flat_map(|s| s.calendars.iter().map(|c| c.id.clone())).collect();
        let colors: HashMap<CalendarId, String> = checked.iter().map(|id| (id.clone(), "#3584e4".to_string())).collect();
        let groups = vec![group("group:1", "Favourites", &["a"]), group("group:2", "Empty", &[])];
        let provider = gtk::CssProvider::new();
        let fired: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        let on_action: CalendarSidebarActionHandler = {
            let fired = fired.clone();
            Rc::new(move |action| {
                let label = match action {
                    CalendarSidebarAction::NewGroup => "new-group".to_string(),
                    CalendarSidebarAction::ShowAll => "show-all".to_string(),
                    CalendarSidebarAction::SetColor(id, hex) => format!("color:{hex}:{}", id.0),
                    CalendarSidebarAction::DeleteGroup(id) => format!("delete-group:{id}"),
                    _ => "other".to_string(),
                };
                fired.borrow_mut().push(label);
            })
        };

        let container = gtk::Box::new(gtk::Orientation::Vertical, 0);
        rebuild_calendar_checklist(&container, &sections, &checked, &colors, &provider, |_, _| {}, on_action, &groups, 2);

        let mut widgets = Vec::new();
        collect_widgets(container.upcast_ref::<gtk::Widget>(), &mut widgets);
        let labels: Vec<String> = widgets
            .iter()
            .filter_map(|w| w.downcast_ref::<gtk::Label>())
            .map(|label| label.text().to_string())
            .collect();
        let buttons: Vec<gtk::Button> = widgets.iter().filter_map(|w| w.downcast_ref::<gtk::Button>().cloned()).collect();
        let menus = widgets.iter().filter(|w| w.is::<gtk::MenuButton>()).count();

        assert!(labels.iter().any(|l| l == "Favourites"), "the group header renders: {labels:?}");
        assert!(labels.iter().any(|l| l == "a"), "the group's calendar row renders: {labels:?}");
        assert!(labels.iter().any(|l| l == "Work account"), "the account fallback renders");
        assert!(labels.iter().any(|l| l == "Empty"), "an empty group still renders its header");
        assert_eq!(menus, 4, "one ⋯ menu per calendar row (2) and per group header (2)");

        // The shared provider carries the chip rules and the swatch backdrops.
        let css = provider.to_str();
        assert!(css.contains("calendar-event-3584e4"), "chip rules written: {css}");
        assert!(css.contains("calendar-swatch-3584e4"), "swatch rules written: {css}");

        // The footer's New group / Show all buttons dispatch through the handler.
        let click = |name: &str| {
            let button = buttons.iter().find(|b| b.label().as_deref() == Some(name)).unwrap_or_else(|| panic!("no {name} button"));
            button.emit_clicked();
        };
        click("+ New group");
        click("Show all");
        assert_eq!(fired.borrow().as_slice(), &["new-group".to_string(), "show-all".to_string()]);
    }
}
