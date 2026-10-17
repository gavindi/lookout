//! The Scheduling Assistant's attendee × time grid (OUTLOOK_PARITY.md item
//! 3.8 / CAL-19), plus the "Suggested times" heuristic over a free/busy
//! answer.
//!
//! A horizontal timeline: one row per attendee (and the organizer), each
//! painted with the busy blocks a free/busy lookup returned, over a shared
//! hour axis. It is a Cairo widget like the main calendar's `TimeGrid`, but
//! oriented rows-by-time (Outlook's Scheduling Assistant, and the same shape
//! the Schedule view CAL-03 will want), so the two do not share code beyond
//! app-wide helpers.
//!
//! The widget is data-in/callback-out like the rest of the calendar code: the
//! editor fills it via [`ScheduleGrid::set_rows`] and hears back through
//! [`ScheduleGrid::connect_time_picked`] / [`ScheduleGrid::connect_date_changed`].

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use chrono::{Datelike, Duration, NaiveDate, Timelike};
use lookout_core::{all_free, AttendeeAvailability, BusyStatus, FreeBusyInterval};

use crate::calendar_tz::DisplayZone;
use crate::settings::CalendarPreferences;

const LABEL_WIDTH: f64 = 168.0;
const HEADER_HEIGHT: f64 = 24.0;
const ROW_HEIGHT: f64 = 36.0;
const PAD: f64 = 4.0;
const SLOT_MINUTES: i64 = 30;

const BUSY_RGB: (f64, f64, f64) = (0.208, 0.518, 0.894);
const TENTATIVE_RGB: (f64, f64, f64) = (0.478, 0.361, 0.145);
const OOF_RGB: (f64, f64, f64) = (0.482, 0.173, 0.749);
const FREE_RGB: (f64, f64, f64) = (0.36, 0.60, 0.36);
const CONFLICT_RGB: (f64, f64, f64) = (0.878, 0.345, 0.290);
const UNKNOWN_RGB: (f64, f64, f64) = (0.55, 0.55, 0.58);

/// One person's line in the grid.
#[derive(Clone, Default)]
pub struct ScheduleRow {
    /// The display label ("You", or the attendee's name/address).
    pub label: String,
    pub subtitle: Option<String>,
    /// The busy blocks reported for this person (empty when free or unknown).
    pub intervals: Vec<FreeBusyInterval>,
    /// A lookup that hasn't returned yet.
    pub loading: bool,
    /// The lookup returned no usable data (no backend, or an unresolvable
    /// address) - the row is drawn hatched rather than as "free".
    pub unknown: bool,
    /// This is the organizer's own row (drawn first, with a divider).
    pub organizer: bool,
}

/// Shared mutable render state.
struct GridState {
    rows: Vec<ScheduleRow>,
    date: NaiveDate,
    zone: DisplayZone,
    work_days: Vec<chrono::Weekday>,
    work_start: u32,
    work_end: u32,
    /// The event's current span in the display zone, drawn as a conflict
    /// outline so the user can see the proposed time against the grid.
    proposed: Option<(chrono::NaiveDateTime, chrono::NaiveDateTime)>,
}

/// A callback the grid invokes when the user picks a time.
type TimePickedCallback = Rc<dyn Fn(chrono::NaiveDateTime)>;
/// A callback the grid invokes when its date changes.
type DateChangedCallback = Rc<dyn Fn(NaiveDate)>;
/// A callback the grid invokes when the user asks to refresh.
type RefreshCallback = Rc<dyn Fn()>;

/// The Scheduling Assistant grid widget.
pub struct ScheduleGrid {
    pub root: gtk::Box,
    canvas: gtk::DrawingArea,
    header_label: gtk::Label,
    status_label: gtk::Label,
    state: Rc<RefCell<GridState>>,
    on_time_picked: Rc<RefCell<Option<TimePickedCallback>>>,
    on_date_changed: Rc<RefCell<Option<DateChangedCallback>>>,
    on_refresh: Rc<RefCell<Option<RefreshCallback>>>,
}

impl ScheduleGrid {
    /// Replaces the grid's rows (organizer first, then attendees).
    pub fn set_rows(&self, rows: Vec<ScheduleRow>) {
        self.state.borrow_mut().rows = rows;
        self.resize_canvas();
        self.canvas.queue_draw();
    }

    /// Sets the day the grid shows.
    pub fn set_date(&self, date: NaiveDate) {
        self.state.borrow_mut().date = date;
        self.header_label.set_label(&format_date(date));
        self.canvas.queue_draw();
    }

    /// The day the grid currently shows.
    pub fn date(&self) -> NaiveDate {
        self.state.borrow().date
    }

    /// Sets the zone times are drawn in (the event's own zone, so a click maps
    /// straight onto the editor's start/end fields).
    pub fn set_zone(&self, zone: DisplayZone) {
        self.state.borrow_mut().zone = zone;
        self.canvas.queue_draw();
    }

    /// The proposed span to outline as the conflict reference.
    pub fn set_proposed(&self, start: Option<chrono::NaiveDateTime>, end: Option<chrono::NaiveDateTime>) {
        self.state.borrow_mut().proposed = start.zip(end);
        self.canvas.queue_draw();
    }

    /// A short status line under the header ("Checking availability…", an
    /// error, or empty).
    pub fn set_status(&self, text: &str) {
        self.status_label.set_label(text);
        self.status_label.set_visible(!text.is_empty());
    }

    pub fn connect_time_picked(&self, f: impl Fn(chrono::NaiveDateTime) + 'static) {
        *self.on_time_picked.borrow_mut() = Some(Rc::new(f));
    }

    pub fn connect_date_changed(&self, f: impl Fn(NaiveDate) + 'static) {
        *self.on_date_changed.borrow_mut() = Some(Rc::new(f));
    }

    pub fn connect_refresh(&self, f: impl Fn() + 'static) {
        *self.on_refresh.borrow_mut() = Some(Rc::new(f));
    }

    fn resize_canvas(&self) {
        let rows = self.state.borrow().rows.len().max(1);
        let height = (HEADER_HEIGHT + rows as f64 * ROW_HEIGHT + PAD).round() as i32;
        self.canvas.set_content_height(height.max(80));
    }
}

/// Builds the grid, seeded with the given calendar preferences (work hours,
/// work days, display zone).
pub fn build_schedule_grid(prefs: &CalendarPreferences, date: NaiveDate, zone: DisplayZone) -> ScheduleGrid {
    let state = Rc::new(RefCell::new(GridState {
        rows: Vec::new(),
        date,
        zone,
        work_days: prefs.work_days.clone(),
        work_start: prefs.work_start,
        work_end: prefs.work_end,
        proposed: None,
    }));

    let header_label = gtk::Label::builder().label(format_date(date)).css_classes(["heading"]).build();
    crate::privacy::mark_ui(&header_label);
    let prev = gtk::Button::from_icon_name("go-previous-symbolic");
    let next = gtk::Button::from_icon_name("go-next-symbolic");
    let today = gtk::Button::with_label("Today");
    let refresh = gtk::Button::from_icon_name("view-refresh-symbolic");
    prev.set_tooltip_text(Some("Previous day"));
    next.set_tooltip_text(Some("Next day"));
    today.set_tooltip_text(Some("Today"));
    refresh.set_tooltip_text(Some("Check availability again"));

    let header = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .margin_start(8)
        .margin_end(8)
        .build();
    header.append(&prev);
    header.append(&next);
    header.append(&today);
    header.append(&header_label);
    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    header.append(&spacer);
    header.append(&refresh);

    let status_label = gtk::Label::builder()
        .css_classes(["dim-label", "caption"])
        .xalign(0.0)
        .halign(gtk::Align::Start)
        .margin_start(8)
        .margin_end(8)
        .wrap(true)
        .build();
    crate::privacy::mark_ui(&status_label);
    status_label.set_visible(false);

    let canvas = gtk::DrawingArea::new();
    canvas.set_hexpand(true);
    canvas.set_content_height(120);

    let on_time_picked: Rc<RefCell<Option<TimePickedCallback>>> = Rc::new(RefCell::new(None));
    let on_date_changed: Rc<RefCell<Option<DateChangedCallback>>> = Rc::new(RefCell::new(None));
    let on_refresh: Rc<RefCell<Option<RefreshCallback>>> = Rc::new(RefCell::new(None));

    {
        let draw_state = state.clone();
        canvas.set_draw_func(move |_, cr, width, height| {
            paint_grid(cr, width as f64, height as f64, &draw_state.borrow());
        });
        let on_time_picked = on_time_picked.clone();
        let canvas_clone = canvas.clone();
        let click_state = state.clone();
        let click = gtk::GestureClick::new();
        click.set_button(1);
        click.connect_pressed(move |_, _n, x, y| {
            let width = canvas_clone.width() as f64;
            if y < HEADER_HEIGHT {
                return;
            }
            let Some(minutes) = minutes_from_x(x, width) else { return };
            let snapped = (minutes / SLOT_MINUTES) * SLOT_MINUTES;
            let date = click_state.borrow().date;
            let naive = date.and_hms_opt(0, 0, 0).unwrap() + Duration::minutes(snapped);
            if let Some(callback) = on_time_picked.borrow().as_ref() {
                callback(naive);
            }
        });
        canvas.add_controller(click);
    }

    {
        let state = state.clone();
        let header_label = header_label.clone();
        let on_date_changed = on_date_changed.clone();
        prev.connect_clicked(move |_| {
            let date = state.borrow().date - Duration::days(1);
            state.borrow_mut().date = date;
            header_label.set_label(&format_date(date));
            if let Some(callback) = on_date_changed.borrow().as_ref() {
                callback(date);
            }
        });
    }
    {
        let state = state.clone();
        let header_label = header_label.clone();
        let on_date_changed = on_date_changed.clone();
        next.connect_clicked(move |_| {
            let date = state.borrow().date + Duration::days(1);
            state.borrow_mut().date = date;
            header_label.set_label(&format_date(date));
            if let Some(callback) = on_date_changed.borrow().as_ref() {
                callback(date);
            }
        });
    }
    {
        let state = state.clone();
        let header_label = header_label.clone();
        let on_date_changed = on_date_changed.clone();
        today.connect_clicked(move |_| {
            let date = chrono::Utc::now().date_naive();
            state.borrow_mut().date = date;
            header_label.set_label(&format_date(date));
            if let Some(callback) = on_date_changed.borrow().as_ref() {
                callback(date);
            }
        });
    }
    {
        let on_refresh = on_refresh.clone();
        refresh.connect_clicked(move |_| {
            if let Some(callback) = on_refresh.borrow().as_ref() {
                callback();
            }
        });
    }

    let scroller = gtk::ScrolledWindow::builder()
        .child(&canvas)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .vexpand(true)
        .build();

    let root = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).build();
    root.append(&header);
    root.append(&status_label);
    root.append(&scroller);

    ScheduleGrid {
        root,
        canvas,
        header_label,
        status_label,
        state,
        on_time_picked,
        on_date_changed,
        on_refresh,
    }
}

fn format_date(date: NaiveDate) -> String {
    date.format("%A, %-d %B %Y").to_string()
}

/// The display-zone minutes-since-midnight of `naive` measured from `date`
/// (negative or >1440 when it falls on another day).
fn minutes_on(date: NaiveDate, naive: chrono::NaiveDateTime) -> i64 {
    (naive.date() - date).num_days() * 1440 + i64::from(naive.hour()) * 60 + i64::from(naive.minute())
}

/// The x position for a minutes-since-midnight value, or the reverse. The
/// time plot runs from `LABEL_WIDTH` to the widget edge.
fn x_for_minutes(minutes: f64, width: f64) -> f64 {
    let plot = (width - LABEL_WIDTH).max(1.0);
    LABEL_WIDTH + (minutes / 1440.0).clamp(0.0, 1.0) * plot
}

fn minutes_from_x(x: f64, width: f64) -> Option<i64> {
    if x < LABEL_WIDTH {
        return None;
    }
    let plot = (width - LABEL_WIDTH).max(1.0);
    let minutes = ((x - LABEL_WIDTH) / plot * 1440.0).round() as i64;
    Some(minutes.clamp(0, 1440))
}

fn paint_grid(cr: &gtk::cairo::Context, width: f64, height: f64, state: &GridState) {
    let _ = height;
    cr.select_font_face("Sans", gtk::cairo::FontSlant::Normal, gtk::cairo::FontWeight::Normal);

    // Hour axis labels and vertical grid lines.
    for hour in (0..24).step_by(2) {
        let x = x_for_minutes(f64::from(hour) * 60.0, width);
        cr.set_source_rgba(0.5, 0.5, 0.5, 0.25);
        cr.set_line_width(0.5);
        cr.move_to(x, HEADER_HEIGHT);
        cr.line_to(x, HEADER_HEIGHT + state.rows.len() as f64 * ROW_HEIGHT);
        let _ = cr.stroke();
        cr.set_font_size(10.0);
        cr.set_source_rgba(0.5, 0.5, 0.5, 0.9);
        cr.move_to(x + 2.0, HEADER_HEIGHT - 8.0);
        let _ = cr.show_text(&format!("{hour:02}:00"));
    }

    let today = state.zone.today();
    for (index, row) in state.rows.iter().enumerate() {
        let y = HEADER_HEIGHT + index as f64 * ROW_HEIGHT;
        let is_work_day = state.work_days.contains(&state.date.weekday());

        // Working-hours shading.
        if is_work_day {
            let start = x_for_minutes(f64::from(state.work_start) * 60.0, width);
            let end = x_for_minutes(f64::from(state.work_end) * 60.0, width);
            cr.set_source_rgba(0.5, 0.5, 0.5, 0.08);
            cr.rectangle(start, y, (end - start).max(0.0), ROW_HEIGHT);
            let _ = cr.fill();
        }

        // Label column.
        cr.set_font_size(12.0);
        cr.set_source_rgba(0.15, 0.15, 0.17, if row.organizer { 1.0 } else { 0.9 });
        cr.move_to(8.0, y + 16.0);
        let _ = cr.show_text(&ellipsize(cr, &row.label, LABEL_WIDTH - 16.0));
        if let Some(subtitle) = &row.subtitle {
            cr.set_font_size(9.5);
            cr.set_source_rgba(0.5, 0.5, 0.5, 1.0);
            cr.move_to(8.0, y + 29.0);
            let _ = cr.show_text(&ellipsize(cr, subtitle, LABEL_WIDTH - 16.0));
        }

        if row.loading {
            cr.set_font_size(10.0);
            cr.set_source_rgba(0.5, 0.5, 0.5, 1.0);
            cr.move_to(LABEL_WIDTH + 6.0, y + 22.0);
            let _ = cr.show_text("Checking…");
        } else if row.unknown {
            hatch_rect(cr, LABEL_WIDTH, y + 2.0, width - LABEL_WIDTH, ROW_HEIGHT - 4.0, UNKNOWN_RGB);
            cr.set_font_size(10.0);
            cr.set_source_rgba(0.55, 0.55, 0.58, 1.0);
            cr.move_to(LABEL_WIDTH + 6.0, y + 22.0);
            let _ = cr.show_text("No availability data");
        } else {
            for interval in &row.intervals {
                let start = minutes_on(state.date, state.zone.to_local(interval.start));
                let end = minutes_on(state.date, state.zone.to_local(interval.end));
                if end <= 0 || start >= 1440 {
                    continue;
                }
                let start = start.max(0);
                let end = end.min(1440);
                let x0 = x_for_minutes(start as f64, width);
                let x1 = x_for_minutes(end as f64, width);
                paint_interval(cr, x0, y + 2.0, (x1 - x0).max(1.0), ROW_HEIGHT - 4.0, interval.status);
            }
        }

        // Proposed-event outline (conflict reference) on every row.
        if let Some((start, end)) = state.proposed {
            let start = minutes_on(state.date, start).max(0);
            let end = minutes_on(state.date, end).min(1440);
            if end > start {
                let x0 = x_for_minutes(start as f64, width);
                let x1 = x_for_minutes(end as f64, width);
                cr.set_source_rgba(CONFLICT_RGB.0, CONFLICT_RGB.1, CONFLICT_RGB.2, 0.9);
                cr.set_line_width(1.5);
                cr.rectangle(x0, y + 1.0, (x1 - x0).max(1.0), ROW_HEIGHT - 2.0);
                let _ = cr.stroke();
            }
        }

        // Row separator.
        cr.set_source_rgba(0.5, 0.5, 0.5, 0.2);
        cr.set_line_width(0.5);
        cr.move_to(0.0, y + ROW_HEIGHT);
        cr.line_to(width, y + ROW_HEIGHT);
        let _ = cr.stroke();
    }

    // Current-time line.
    if state.date == today {
        let now = state.zone.now();
        let minutes = i64::from(now.hour()) * 60 + i64::from(now.minute());
        let x = x_for_minutes(minutes as f64, width);
        cr.set_source_rgba(0.85, 0.2, 0.2, 0.8);
        cr.set_line_width(1.0);
        cr.move_to(x, HEADER_HEIGHT);
        cr.line_to(x, HEADER_HEIGHT + state.rows.len() as f64 * ROW_HEIGHT);
        let _ = cr.stroke();
    }

    // Label column separator.
    cr.set_source_rgba(0.5, 0.5, 0.5, 0.3);
    cr.set_line_width(0.5);
    cr.move_to(LABEL_WIDTH, 0.0);
    cr.line_to(LABEL_WIDTH, HEADER_HEIGHT + state.rows.len() as f64 * ROW_HEIGHT);
    let _ = cr.stroke();
}

fn paint_interval(cr: &gtk::cairo::Context, x: f64, y: f64, w: f64, h: f64, status: BusyStatus) {
    let rgb = match status {
        BusyStatus::Free => FREE_RGB,
        BusyStatus::Tentative => TENTATIVE_RGB,
        BusyStatus::Busy => BUSY_RGB,
        BusyStatus::Oof => OOF_RGB,
        BusyStatus::WorkingElsewhere => TENTATIVE_RGB,
    };
    match status {
        BusyStatus::Free => {
            cr.set_source_rgba(rgb.0, rgb.1, rgb.2, 0.8);
            cr.set_line_width(1.5);
            cr.rectangle(x, y, w, h);
            let _ = cr.stroke();
        }
        BusyStatus::Tentative | BusyStatus::WorkingElsewhere => hatch_rect(cr, x, y, w, h, rgb),
        _ => {
            cr.set_source_rgba(rgb.0, rgb.1, rgb.2, 0.85);
            cr.rectangle(x, y, w, h);
            let _ = cr.fill();
        }
    }
}

/// Fills `[x, y, w, h]` with diagonal hatching in `rgb` (the show-as styling
/// for tentative/no-data states).
fn hatch_rect(cr: &gtk::cairo::Context, x: f64, y: f64, w: f64, h: f64, rgb: (f64, f64, f64)) {
    let _ = cr.save();
    cr.rectangle(x, y, w, h);
    cr.clip();
    cr.set_source_rgba(rgb.0, rgb.1, rgb.2, 0.16);
    cr.rectangle(x, y, w, h);
    let _ = cr.fill();
    cr.set_source_rgba(rgb.0, rgb.1, rgb.2, 0.75);
    cr.set_line_width(1.0);
    let mut offset = -h;
    while offset < w {
        cr.move_to(x + offset, y + h);
        cr.line_to(x + offset + h, y);
        offset += 6.0;
    }
    let _ = cr.stroke();
    let _ = cr.restore();
}

/// Truncates `text` to fit `max_width` with an ellipsis.
fn ellipsize(cr: &gtk::cairo::Context, text: &str, max_width: f64) -> String {
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

/// The "Suggested times" heuristic (CAL-19): scan `date`'s working hours for
/// the first slots where every attendee in `availabilities` is free for
/// `duration_minutes`. Returns the candidate start times in the display zone.
/// An `unknown` attendee (or none at all) yields no suggestions - the grid
/// can't promise a free slot it has no data for.
pub fn suggest_times(
    availabilities: &[AttendeeAvailability],
    date: NaiveDate,
    work_start: u32,
    work_end: u32,
    zone: DisplayZone,
    duration_minutes: i64,
    limit: usize,
) -> Vec<chrono::NaiveDateTime> {
    if availabilities.is_empty() || availabilities.iter().any(|a| a.unknown) {
        return Vec::new();
    }
    let duration = duration_minutes.max(SLOT_MINUTES);
    let day_start = i64::from(work_start) * 60;
    let day_end = i64::from(work_end) * 60;
    let mut out = Vec::new();
    let mut minute = day_start;
    while minute + duration <= day_end && out.len() < limit {
        let start_local = date.and_hms_opt(0, 0, 0).unwrap() + Duration::minutes(minute);
        let end_local = start_local + Duration::minutes(duration);
        let start = zone.resolve_local(start_local);
        let end = zone.resolve_local(end_local);
        if all_free(availabilities, start, end) {
            out.push(start_local);
        }
        minute += SLOT_MINUTES;
    }
    out
}

/// A compact single-row "your day" timeline for the reading pane's invite card
/// (CAL-35 / UX-60): the day's busy blocks over an hour axis, the proposed span
/// outlined in the conflict colour, and a now-line when the day is today. No
/// label column or header - simpler than [`ScheduleGrid`], which it shares its
/// painters with.
pub fn build_day_timeline(
    prefs: &CalendarPreferences,
    date: NaiveDate,
    zone: DisplayZone,
    intervals: Vec<FreeBusyInterval>,
    proposed: Option<(chrono::NaiveDateTime, chrono::NaiveDateTime)>,
) -> gtk::Widget {
    let state = Rc::new(RefCell::new(GridState {
        rows: vec![ScheduleRow {
            intervals,
            ..ScheduleRow::default()
        }],
        date,
        zone,
        work_days: prefs.work_days.clone(),
        work_start: prefs.work_start,
        work_end: prefs.work_end,
        proposed,
    }));
    let canvas = gtk::DrawingArea::new();
    canvas.set_hexpand(true);
    canvas.set_content_height(44);
    canvas.set_widget_name("imip-timeline-canvas");
    let draw_state = state.clone();
    canvas.set_draw_func(move |_, cr, width, height| {
        paint_day_timeline(cr, width as f64, height as f64, &draw_state.borrow());
    });
    canvas.upcast()
}

/// Paints [`build_day_timeline`]'s single row: no label column, so the time
/// plot spans the widget's full width.
fn paint_day_timeline(cr: &gtk::cairo::Context, width: f64, height: f64, state: &GridState) {
    let top = 4.0;
    let row_height = (height - 8.0).max(20.0);
    cr.select_font_face("Sans", gtk::cairo::FontSlant::Normal, gtk::cairo::FontWeight::Normal);

    // Working-hours shading on work days.
    if state.work_days.contains(&state.date.weekday()) {
        let start = (f64::from(state.work_start) / 24.0) * width;
        let end = (f64::from(state.work_end) / 24.0) * width;
        cr.set_source_rgba(0.5, 0.5, 0.5, 0.08);
        cr.rectangle(start, top, (end - start).max(0.0), row_height);
        let _ = cr.fill();
    }

    // Hour ticks every three hours.
    for hour in (0..24).step_by(3) {
        let x = (hour as f64 / 24.0) * width;
        cr.set_source_rgba(0.5, 0.5, 0.5, 0.2);
        cr.set_line_width(0.5);
        cr.move_to(x, top);
        cr.line_to(x, top + row_height);
        let _ = cr.stroke();
    }

    // The day's busy blocks, clipped to the widget (cross-day intervals are cut
    // at midnight).
    if let Some(row) = state.rows.first() {
        for interval in &row.intervals {
            let start = minutes_on(state.date, state.zone.to_local(interval.start));
            let end = minutes_on(state.date, state.zone.to_local(interval.end));
            if end <= 0 || start >= 1440 {
                continue;
            }
            let start = start.max(0);
            let end = end.min(1440);
            let x0 = (start as f64 / 1440.0) * width;
            let x1 = (end as f64 / 1440.0) * width;
            paint_interval(cr, x0, top, (x1 - x0).max(1.0), row_height, interval.status);
        }
    }

    // The invited span, outlined in the conflict colour.
    if let Some((start, end)) = state.proposed {
        let start = minutes_on(state.date, start).max(0);
        let end = minutes_on(state.date, end).min(1440);
        if end > start {
            let x0 = (start as f64 / 1440.0) * width;
            let x1 = (end as f64 / 1440.0) * width;
            cr.set_source_rgba(CONFLICT_RGB.0, CONFLICT_RGB.1, CONFLICT_RGB.2, 0.95);
            cr.set_line_width(2.0);
            cr.rectangle(x0, top, (x1 - x0).max(1.0), row_height);
            let _ = cr.stroke();
        }
    }

    // Current-time line.
    if state.date == state.zone.today() {
        let now = state.zone.now();
        let minutes = i64::from(now.hour()) * 60 + i64::from(now.minute());
        let x = (minutes as f64 / 1440.0) * width;
        cr.set_source_rgba(0.85, 0.2, 0.2, 0.8);
        cr.set_line_width(1.0);
        cr.move_to(x, top);
        cr.line_to(x, top + row_height);
        let _ = cr.stroke();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveTime, TimeZone};
    use lookout_core::EmailAddress;

    fn utc(y: i32, m: u32, d: u32, h: u32) -> chrono::DateTime<chrono::Utc> {
        chrono::Utc.with_ymd_and_hms(y, m, d, h, 0, 0).unwrap()
    }

    fn availability(hours: &[(u32, u32)], unknown: bool) -> AttendeeAvailability {
        if unknown {
            return AttendeeAvailability::unknown(EmailAddress::new("x@example.com"));
        }
        let intervals = hours
            .iter()
            .map(|(s, e)| FreeBusyInterval {
                start: utc(2026, 10, 15, *s),
                end: utc(2026, 10, 15, *e),
                status: BusyStatus::Busy,
            })
            .collect();
        AttendeeAvailability::new(EmailAddress::new("x@example.com"), intervals)
    }

    #[test]
    fn minutes_on_measures_from_the_grid_date() {
        let date = chrono::NaiveDate::from_ymd_opt(2026, 10, 15).unwrap();
        assert_eq!(minutes_on(date, date.and_hms_opt(9, 30, 0).unwrap()), 570);
        let next_day = date.and_hms_opt(0, 0, 0).unwrap() + Duration::days(1);
        assert_eq!(minutes_on(date, next_day), 1440);
    }

    #[test]
    fn x_and_minutes_are_inverse() {
        let width = LABEL_WIDTH + 720.0;
        for minute in [0.0, 360.0, 720.0, 1439.0] {
            let x = x_for_minutes(minute, width);
            let back = minutes_from_x(x, width).unwrap();
            assert!((back as f64 - minute).abs() <= 1.0, "minute {minute} -> {back}");
        }
        assert_eq!(minutes_from_x(LABEL_WIDTH - 1.0, width), None);
    }

    #[test]
    fn suggest_times_finds_free_slots_inside_working_hours() {
        let zone = DisplayZone::Named(chrono_tz::UTC);
        let date = chrono::NaiveDate::from_ymd_opt(2026, 10, 15).unwrap();
        // Busy 9-10 and 11-12; working hours 8-18, 60-min meetings.
        let a = availability(&[(9, 10), (11, 12)], false);
        let suggestions = suggest_times(&[a], date, 8, 18, zone, 60, 10);
        assert_eq!(suggestions.first().unwrap().time(), NaiveTime::from_hms_opt(8, 0, 0).unwrap());
        assert!(!suggestions.iter().any(|s| s.time() == NaiveTime::from_hms_opt(9, 0, 0).unwrap()));
        assert!(!suggestions.iter().any(|s| s.time() == NaiveTime::from_hms_opt(11, 0, 0).unwrap()));
        assert!(suggestions.iter().any(|s| s.time() == NaiveTime::from_hms_opt(10, 0, 0).unwrap()));
    }

    #[test]
    fn suggest_times_declines_when_data_is_unknown_or_absent() {
        let zone = DisplayZone::Named(chrono_tz::UTC);
        let date = chrono::NaiveDate::from_ymd_opt(2026, 10, 15).unwrap();
        assert!(suggest_times(&[], date, 8, 18, zone, 60, 5).is_empty());
        assert!(suggest_times(&[availability(&[], true)], date, 8, 18, zone, 60, 5).is_empty());
    }

    #[test]
    fn suggestions_respect_the_limit() {
        let zone = DisplayZone::Named(chrono_tz::UTC);
        let date = chrono::NaiveDate::from_ymd_opt(2026, 10, 15).unwrap();
        let a = availability(&[], false);
        assert_eq!(suggest_times(&[a], date, 8, 18, zone, 60, 3).len(), 3);
    }
}
