//! The Lookout module: a dashboard tab snapshotting the user's connected
//! accounts - people most contacted, emails by time of day, outstanding
//! tasks, and upcoming calendar events. Follows the same
//! data-in/widget-state-out convention as `tasks_view`: the caller owns
//! the data and pushes it via [`LookoutView::set_data`]; the view owns
//! only widget state. Selection and ordering logic live in pure,
//! unit-tested functions so the layout is testable independent of a
//! running GTK main loop.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use chrono::{DateTime, Duration, Local, NaiveDateTime};
use gtk::prelude::*;
use lookout_core::{CalendarId, CalendarTask, EmailAddress, EventOccurrence};

use crate::calendar_colors::{CalendarColorMap, DEFAULT_CHECK_COLOR};
use crate::tasks_view::{self, ActivateHandler, TaskBucket, ToggleHandler};

/// How many rows each dashboard section shows. `TOP_CONTACTS_LIMIT` is
/// `pub(crate)` because the caller unions each account's top list and needs
/// the same cap to re-rank by.
pub(crate) const TOP_CONTACTS_LIMIT: usize = 8;
const OUTSTANDING_TASKS_LIMIT: usize = 8;
const UPCOMING_EVENTS_LIMIT: usize = 10;
/// How far ahead the events section looks (bounded by what the sessions
/// have synced; the caller widens that by requesting the months on open).
const EVENT_HORIZON_DAYS: i64 = 14;

/// Everything the dashboard needs to repaint, merged across accounts by
/// the caller.
#[derive(Debug, Clone, Default)]
pub struct LookoutData {
    /// Most-contacted people first, with their lifetime appearance count.
    pub contacts: Vec<(EmailAddress, i64)>,
    /// Count of cached messages per local hour of day (index 0 = midnight).
    pub histogram: [i64; 24],
    /// All tasks from every source; the view keeps only the outstanding ones.
    pub tasks: Vec<CalendarTask>,
    /// Every synced occurrence; the view keeps the checked, upcoming ones.
    pub events: Vec<EventOccurrence>,
    /// Which calendars (by id) the user has checked in "My calendars".
    pub checked_calendar_ids: HashSet<CalendarId>,
    /// Per-calendar colours, for the event rows' dots.
    pub colors: CalendarColorMap,
}

/// The outstanding subset of `tasks`, in section order (Overdue, Today,
/// This week, Later) with each bucket's sort kept, truncated to `limit`.
/// Completed tasks are excluded - the dashboard is a to-do, not an archive.
pub fn outstanding_tasks(tasks: &[CalendarTask], now: NaiveDateTime, limit: usize) -> Vec<CalendarTask> {
    let mut out = Vec::new();
    for (bucket, bucket_tasks) in tasks_view::group_tasks(tasks, now) {
        if bucket == TaskBucket::Completed {
            continue;
        }
        out.extend(bucket_tasks);
        if out.len() >= limit {
            break;
        }
    }
    out.truncate(limit);
    out
}

/// The `limit` next occurrences at or after `now` (and within `horizon`),
/// restricted to the checked calendars, sorted by start time.
pub fn upcoming_occurrences<'a>(
    occurrences: impl IntoIterator<Item = &'a EventOccurrence>,
    now: DateTime<Local>,
    horizon: Duration,
    checked: &HashSet<CalendarId>,
    limit: usize,
) -> Vec<&'a EventOccurrence> {
    let horizon_end = now + horizon;
    let mut upcoming: Vec<&'a EventOccurrence> = occurrences
        .into_iter()
        .filter(|occ| checked.contains(&occ.calendar_id))
        .filter(|occ| {
            let start = occ.start.with_timezone(&Local);
            start >= now && start <= horizon_end
        })
        .collect();
    upcoming.sort_by_key(|occ| occ.start);
    upcoming.truncate(limit);
    upcoming
}

/// The Lookout view's widget state. `root` is the scrolled dashboard;
/// callers read nothing back from it - `set_data` repaints from scratch on
/// every push. The task rows' callbacks are stored on the view (via
/// [`LookoutView::set_handlers`]) so `set_data` callers don't need to know
/// about the window or the session routing.
pub struct LookoutView {
    pub root: gtk::ScrolledWindow,
    contacts_list: gtk::Box,
    histogram_area: gtk::DrawingArea,
    histogram: Rc<RefCell<[i64; 24]>>,
    histogram_caption: gtk::Label,
    tasks_list: gtk::Box,
    events_list: gtk::Box,
    toggle: RefCell<Option<ToggleHandler>>,
    activate: RefCell<Option<ActivateHandler>>,
}

/// Builds a fresh Lookout dashboard: a scrollable two-column grid of four
/// cards (contacts, hour histogram, outstanding tasks, upcoming events),
/// each with an empty-state placeholder.
pub fn build_lookout_view() -> LookoutView {
    install_lookout_css();

    let (contacts_card, contacts_list) = section_card("People most contacted");
    let (histogram_card, histogram_box) = section_card("Emails by time of day");
    // The histogram card sits beside the contacts card, which is usually
    // taller; expand the box and the chart area so the painted chart fills
    // the card instead of leaving a dead band below the caption.
    histogram_box.set_vexpand(true);

    let histogram = Rc::new(RefCell::new([0i64; 24]));
    let histogram_for_draw = histogram.clone();
    let histogram_area = gtk::DrawingArea::builder().height_request(120).hexpand(true).vexpand(true).build();
    histogram_area.set_draw_func(move |_, cr, width, height| {
        draw_histogram(cr, width as f64, height as f64, &histogram_for_draw.borrow());
    });
    histogram_box.append(&histogram_area);
    let histogram_caption = gtk::Label::builder().css_classes(["dim-label", "caption"]).xalign(0.0).build();
    histogram_box.append(&histogram_caption);

    let (tasks_card, tasks_list) = section_card("Outstanding tasks");
    let (events_card, events_list) = section_card("Upcoming events");

    let row_top = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).build();
    row_top.append(&contacts_card);
    row_top.append(&histogram_card);
    let row_bottom = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).build();
    row_bottom.append(&tasks_card);
    row_bottom.append(&events_card);

    let content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).build();
    content.append(&row_top);
    content.append(&row_bottom);
    let root = gtk::ScrolledWindow::builder()
        .child(&content)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .build();

    LookoutView {
        root,
        contacts_list,
        histogram_area,
        histogram,
        histogram_caption,
        tasks_list,
        events_list,
        toggle: RefCell::new(None),
        activate: RefCell::new(None),
    }
}

/// A dashboard card: the `card_section` look (rounded `.card`, margin)
/// with a heading label and a repaint-from-scratch content box. The heading
/// and rows get real margins from the card's border (the app's other
/// modules put 8-12px between a `.card`'s edge and its content).
fn section_card(title: &str) -> (gtk::Box, gtk::Box) {
    let heading = gtk::Label::builder()
        .label(title)
        .css_classes(["heading"])
        .xalign(0.0)
        .margin_top(10)
        .margin_start(12)
        .margin_end(12)
        .build();
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .margin_top(6)
        .margin_start(12)
        .margin_end(12)
        .margin_bottom(10)
        .build();
    let card = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .css_classes(["card"])
        .overflow(gtk::Overflow::Hidden)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(6)
        .margin_end(6)
        .hexpand(true)
        .build();
    card.append(&heading);
    card.append(&content);
    (card, content)
}

/// Stores the task rows' callbacks (completion toggle, click-to-edit).
/// Called once at startup; every later `set_data` repaint reuses them.
pub fn set_handlers(view: &LookoutView, toggle: ToggleHandler, activate: ActivateHandler) {
    *view.toggle.borrow_mut() = Some(toggle);
    *view.activate.borrow_mut() = Some(activate);
}

/// Repaints the whole dashboard from `data`. Rebuilds rows from scratch on
/// every call - the same repaint-from-scratch convention as the calendar
/// checklist and the Tasks view.
pub fn set_data(view: &LookoutView, data: &LookoutData) {
    let toggle = view.toggle.borrow().clone().unwrap_or_else(|| Rc::new(|_t, _c| {}));
    let activate = view.activate.borrow().clone().unwrap_or_else(|| Rc::new(|_t| {}));
    let now_local = Local::now();

    // --- People most contacted.
    clear_box(&view.contacts_list);
    if data.contacts.is_empty() {
        append_empty(&view.contacts_list, "No mail history yet - your most-contacted people appear here.");
    } else {
        for (address, count) in data.contacts.iter().take(TOP_CONTACTS_LIMIT) {
            view.contacts_list.append(&contact_row(address, *count));
        }
    }

    // --- Emails by time of day.
    *view.histogram.borrow_mut() = data.histogram;
    let total: i64 = data.histogram.iter().sum();
    view.histogram_caption
        .set_label(&format!("{total} cached message{} · all time", if total == 1 { "" } else { "s" }));
    view.histogram_area.queue_draw();

    // --- Outstanding tasks.
    clear_box(&view.tasks_list);
    let outstanding = outstanding_tasks(&data.tasks, now_local.naive_local(), OUTSTANDING_TASKS_LIMIT);
    if outstanding.is_empty() {
        append_empty(&view.tasks_list, "No outstanding tasks.");
    } else {
        for task in outstanding {
            view.tasks_list.append(&tasks_view::task_row(&task, &data.colors, toggle.clone(), activate.clone(), &[], true));
        }
    }

    // --- Upcoming events.
    clear_box(&view.events_list);
    let upcoming = upcoming_occurrences(
        data.events.iter(),
        now_local,
        Duration::days(EVENT_HORIZON_DAYS),
        &data.checked_calendar_ids,
        UPCOMING_EVENTS_LIMIT,
    );
    if upcoming.is_empty() {
        append_empty(&view.events_list, "No upcoming events.");
    } else {
        for occ in upcoming {
            view.events_list.append(&event_row(occ, &data.colors, now_local));
        }
    }
}

/// One contact row: a display name (falling back to the address), the
/// address as a dim subtitle, and a right-aligned lifetime count.
fn contact_row(address: &EmailAddress, count: i64) -> gtk::Box {
    let name = gtk::Label::builder()
        .label(address.name.clone().unwrap_or_else(|| address.address.clone()))
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    let row_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .margin_start(4)
        .margin_end(4)
        .margin_top(2)
        .margin_bottom(2)
        .build();
    row_box.append(&name);
    if address.name.is_some() {
        let address_label = gtk::Label::builder()
            .label(&address.address)
            .css_classes(["dim-label", "caption"])
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        row_box.append(&address_label);
    }
    let count_label = gtk::Label::builder().label(count.to_string()).css_classes(["lookout-count"]).build();
    let row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(8).build();
    row.append(&row_box);
    row.append(&count_label);
    row
}

/// One upcoming-event row: a calendar colour dot, the summary, and a
/// right-aligned time caption ("All day", today's time, or day + time).
fn event_row(occ: &EventOccurrence, colors: &CalendarColorMap, now: DateTime<Local>) -> gtk::Box {
    let color = colors.get(&occ.calendar_id).map(String::as_str).unwrap_or(DEFAULT_CHECK_COLOR).to_string();
    let dot = gtk::DrawingArea::builder().width_request(10).height_request(10).valign(gtk::Align::Center).build();
    dot.set_draw_func(move |_, cr, width, height| {
        let (r, g, b) = tasks_view::parse_css_color(&color);
        let radius = width.min(height) as f64 / 2.0 - 1.0;
        cr.arc(width as f64 / 2.0, height as f64 / 2.0, radius, 0.0, 2.0 * std::f64::consts::PI);
        cr.set_source_rgba(r, g, b, 1.0);
        let _ = cr.fill();
    });
    dot.set_tooltip_text(Some(&occ.calendar_id.0));

    let summary = gtk::Label::builder()
        .label(occ.summary.clone().unwrap_or_else(|| "(untitled)".to_string()))
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();

    let start = occ.start.with_timezone(&Local);
    let time_text = if occ.all_day {
        "All day".to_string()
    } else if start.date_naive() == now.date_naive() {
        start.format("%H:%M").to_string()
    } else {
        start.format("%a %d %b · %H:%M").to_string()
    };
    let time = gtk::Label::builder().label(&time_text).css_classes(["dim-label", "caption"]).build();

    let row_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .margin_start(4)
        .margin_end(4)
        .build();
    row_box.append(&dot);
    row_box.append(&summary);
    row_box.append(&time);
    row_box
}

/// Paints the 24-bar hour histogram: bars scaled to the largest hour, with
/// hour tick labels at 0/6/12/18/23. A "No mail yet" placeholder replaces
/// the chart while the total is zero.
fn draw_histogram(cr: &gtk::cairo::Context, width: f64, height: f64, histogram: &[i64; 24]) {
    let pad_top = 8.0;
    let pad_bottom = 18.0;
    let pad_x = 8.0;
    let total: i64 = histogram.iter().sum();
    let max: i64 = histogram.iter().copied().max().unwrap_or(0);

    let chart_height = height - pad_top - pad_bottom;
    let plot_width = width - 2.0 * pad_x;

    if total == 0 || max == 0 || chart_height <= 0.0 || plot_width <= 0.0 {
        cr.select_font_face("sans-serif", gtk::cairo::FontSlant::Normal, gtk::cairo::FontWeight::Normal);
        cr.set_font_size(10.0);
        let extents = cr.text_extents("No mail yet").ok();
        let (advance, text_height, y_bearing) = match &extents {
            Some(e) => (e.x_advance(), e.height(), e.y_bearing()),
            None => (0.0, 0.0, 0.0),
        };
        cr.move_to((width - advance) / 2.0, (height - text_height) / 2.0 - y_bearing);
        cr.set_source_rgba(0.5, 0.5, 0.5, 1.0);
        let _ = cr.show_text("No mail yet");
        return;
    }

    let gap = 2.0;
    let bar_width = (plot_width - gap * 23.0) / 24.0;
    if bar_width <= 0.0 {
        return;
    }

    // Baseline.
    cr.set_source_rgba(0.5, 0.5, 0.5, 0.4);
    cr.set_line_width(1.0);
    cr.move_to(pad_x, height - pad_bottom);
    cr.line_to(pad_x + plot_width, height - pad_bottom);
    let _ = cr.stroke();

    // Bars.
    cr.set_source_rgba(0.384, 0.627, 0.918, 1.0); // #62a0ea
    for (hour, count) in histogram.iter().enumerate() {
        let x = pad_x + hour as f64 * (bar_width + gap);
        let bar_height = (*count as f64 / max as f64) * chart_height;
        cr.rectangle(x, height - pad_bottom - bar_height, bar_width, bar_height);
        let _ = cr.fill();
    }

    // Hour tick labels.
    cr.select_font_face("sans-serif", gtk::cairo::FontSlant::Normal, gtk::cairo::FontWeight::Normal);
    cr.set_font_size(9.0);
    for &hour in &[0, 6, 12, 18, 23] {
        let text = hour.to_string();
        let advance = cr.text_extents(&text).ok().map(|e| e.x_advance()).unwrap_or(0.0);
        let x = pad_x + hour as f64 * (bar_width + gap) + (bar_width - advance) / 2.0;
        cr.move_to(x.max(0.0), height - pad_bottom + 12.0);
        let _ = cr.show_text(&text);
    }
}

fn clear_box(box_: &gtk::Box) {
    while let Some(child) = box_.first_child() {
        box_.remove(&child);
    }
}

fn append_empty(box_: &gtk::Box, text: &str) {
    let placeholder = gtk::Label::builder()
        .label(text)
        .css_classes(["dim-label", "caption"])
        .xalign(0.0)
        .wrap(true)
        .margin_top(8)
        .build();
    box_.append(&placeholder);
}

/// The dashboard's handful of display-level CSS rules, installed once per
/// process (multiple installs are harmless, but a `Once` keeps it to one).
fn install_lookout_css() {
    use std::sync::Once;
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let provider = gtk::CssProvider::new();
        provider.load_from_string(
            ".lookout-count {
                color: @lookout-unread;
                font-weight: bold;
                font-feature-settings: 'tnum';
            }",
        );
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::style_context_add_provider_for_display(&display, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use lookout_core::{TaskStatus, TaskUid};

    fn cal(id: &str) -> CalendarId {
        CalendarId(id.to_string())
    }

    fn task(uid: &str, due_day: u32, status: TaskStatus) -> CalendarTask {
        CalendarTask {
            uid: TaskUid(uid.to_string()),
            calendar_id: cal("test:cal"),
            summary: Some(uid.to_string()),
            description: None,
            due: Some(Local.with_ymd_and_hms(2026, 8, due_day, 9, 0, 0).unwrap().with_timezone(&Utc)),
            start: None,
            completed: None,
            status,
            priority: Default::default(),
            percent_complete: None,
            categories: Vec::new(),
            href: None,
            etag: None,
        }
    }

    fn occ(uid: &str, day: u32, hour: u32) -> EventOccurrence {
        EventOccurrence {
            uid: lookout_core::EventUid(uid.to_string()),
            calendar_id: cal("test:cal"),
            summary: Some(uid.to_string()),
            description: None,
            location: None,
            start: Local.with_ymd_and_hms(2026, 8, day, hour, 0, 0).unwrap().with_timezone(&Utc),
            end: Local.with_ymd_and_hms(2026, 8, day, hour + 1, 0, 0).unwrap().with_timezone(&Utc),
            all_day: false,
            rrule: None,
            recurrence_id: None,
            exdates: Vec::new(),
            master_start: None,
            master_end: None,
            href: None,
            etag: None,
            master_href: None,
            master_etag: None,
            attendees: Vec::new(),
            organizer: None,
            categories: Vec::new(),
            sensitivity: Default::default(),
            transparency: Default::default(),
            reminder_minutes_before: None,
            conference_url: None,
        }
    }

    #[test]
    fn outstanding_tasks_excludes_completed_and_keeps_bucket_order() {
        let now = NaiveDateTime::parse_from_str("2026-08-10 12:00:00", "%Y-%m-%d %H:%M:%S").unwrap();
        let tasks = vec![
            task("later", 20, TaskStatus::NeedsAction),
            task("completed", 1, TaskStatus::Completed),
            task("overdue", 8, TaskStatus::NeedsAction),
            task("today", 10, TaskStatus::NeedsAction),
        ];
        let outstanding = outstanding_tasks(&tasks, now, 10);
        let order: Vec<&str> = outstanding.iter().map(|t| t.summary.as_deref().unwrap()).collect();
        assert_eq!(order, vec!["overdue", "today", "later"], "bucket order with completed dropped");

        let truncated = outstanding_tasks(&tasks, now, 2);
        assert_eq!(truncated.len(), 2);
        assert_eq!(truncated[0].summary.as_deref(), Some("overdue"));
        assert_eq!(truncated[1].summary.as_deref(), Some("today"));
    }

    #[test]
    fn upcoming_occurrences_filters_checks_and_horizon_and_sorts() {
        let now = Local.with_ymd_and_hms(2026, 8, 10, 12, 0, 0).unwrap();
        let checked: HashSet<CalendarId> = [cal("test:cal")].into_iter().collect();
        let occurrences = [
            occ("hidden", 10, 15), // unchecked calendar
            occ("later", 12, 9),   // within horizon
            occ("past", 9, 11),    // before now
            occ("far", 30, 9),     // beyond the 14-day horizon
            occ("soon", 10, 14),   // next, sorted first
        ];
        let mut hidden = occurrences[0].clone();
        hidden.calendar_id = cal("other:cal");

        let upcoming = upcoming_occurrences(
            [&occurrences[1], &hidden, &occurrences[2], &occurrences[3], &occurrences[4]],
            now,
            Duration::days(14),
            &checked,
            10,
        );
        let order: Vec<&str> = upcoming.iter().map(|o| o.summary.as_deref().unwrap()).collect();
        assert_eq!(order, vec!["soon", "later"]);

        let limited = upcoming_occurrences([&occurrences[1], &hidden, &occurrences[4]], now, Duration::days(14), &checked, 1);
        assert_eq!(limited.len(), 1);
        assert_eq!(limited[0].summary.as_deref(), Some("soon"));
    }
}
