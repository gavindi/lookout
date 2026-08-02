use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use chrono::{Datelike, NaiveDate};
use gtk::prelude::*;
use lookout_core::{CalendarId, CalendarInfo, EventOccurrence};
use lookout_dav::session::ConnectionState as CalendarConnectionState;

/// Sunday-first week (matches Outlook's/the US default convention, per the
/// reference screenshot this view is matched against) rather than
/// locale-detected - an explicit simplification for this pass.
const WEEKDAY_LABELS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MAX_VISIBLE_EVENTS_PER_DAY: usize = 3;

struct DayCell {
    container: gtk::Box,
    date_label: gtk::Label,
    events_box: gtk::Box,
}

/// A deliberately dumb, read-only month-grid calendar view: no drag/resize/
/// creation, no per-event click handling. Kept as plain data-in/widget-state-
/// out functions (`set_month`/`set_occurrences`), mirroring `folder_tree.rs`'s
/// `build_multi_account_tree_model` precedent, so the date-bucketing logic
/// stays testable independent of a running GTK main loop.
pub struct MonthGrid {
    pub root: gtk::Widget,
    pub prev_button: gtk::Button,
    pub next_button: gtk::Button,
    pub today_button: gtk::Button,
    header_label: gtk::Label,
    day_cells: Vec<DayCell>,
    anchor_month: Rc<RefCell<NaiveDate>>,
}

/// Flat hairline grid (`.calendar-day-cell`) instead of libadwaita's rounded
/// `.card` panel per day, and a bordered highlight for today
/// (`.calendar-today-cell`) - matches the Outlook reference this view is
/// styled against. Registered once (from `build()`) on the default display,
/// same pattern as `window.rs`'s `install_paned_css()`.
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
        }",
    );
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(&display, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    }
}

pub fn build() -> MonthGrid {
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

    let grid = gtk::Grid::builder().row_homogeneous(true).column_homogeneous(true).row_spacing(1).column_spacing(1).vexpand(true).hexpand(true).build();

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
            day_cells.push(DayCell { container, date_label, events_box });
        }
    }

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
        .build();
    root_box.append(&header_row);
    root_box.append(&grid);

    let month_grid = MonthGrid {
        root: root_box.upcast(),
        prev_button,
        next_button,
        today_button,
        header_label,
        day_cells,
        anchor_month: Rc::new(RefCell::new(first_of_month(chrono::Utc::now().date_naive()))),
    };
    // Extracted into a local first (rather than inlined into the call
    // below) so the `Ref` temporary from `.borrow()` is dropped before
    // `set_month` runs - otherwise it would still be alive (temporaries live
    // until the end of the enclosing statement) when `set_month` takes its
    // own `borrow_mut()`, panicking with "already borrowed".
    let initial_month = *month_grid.anchor_month.borrow();
    set_month(&month_grid, initial_month);
    month_grid
}

/// Rebuilds the grid's date labels/highlighting for `month` and clears every
/// cell's event list (a subsequent `set_occurrences` call repopulates them).
pub fn set_month(mg: &MonthGrid, month: NaiveDate) {
    let month = first_of_month(month);
    *mg.anchor_month.borrow_mut() = month;
    mg.header_label.set_label(&month.format("%B %Y").to_string());

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
pub fn set_occurrences(mg: &MonthGrid, occurrences: &[EventOccurrence]) {
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

        let visible = day_occurrences.len().min(MAX_VISIBLE_EVENTS_PER_DAY);
        for occ in &day_occurrences[..visible] {
            let label = gtk::Label::builder()
                .label(occ.summary.as_deref().unwrap_or("(untitled)"))
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .css_classes(["caption"])
                .build();
            cell.events_box.append(&label);
        }
        if day_occurrences.len() > MAX_VISIBLE_EVENTS_PER_DAY {
            let more_label = gtk::Label::builder()
                .label(format!("+{} more", day_occurrences.len() - MAX_VISIBLE_EVENTS_PER_DAY))
                .xalign(0.0)
                .css_classes(["dim-label", "caption"])
                .build();
            cell.events_box.append(&more_label);
        }
    }
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
                let label = gtk::Label::builder()
                    .label(status)
                    .css_classes(["dim-label", "caption"])
                    .xalign(0.0)
                    .wrap(true)
                    .build();
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

    let add_calendar_button = gtk::Button::builder().label("Add calendar").css_classes(["flat"]).halign(gtk::Align::Start).sensitive(false).build();

    let my_calendars_label = gtk::Label::builder().label("My calendars").css_classes(["heading"]).xalign(0.0).margin_top(12).build();
    let calendar_list_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).build();

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
    root_box.append(&calendar_list_box);

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
    fn account_status_is_none_once_calendars_exist() {
        for state in [
            CalendarConnectionState::Disconnected,
            CalendarConnectionState::Connecting,
            CalendarConnectionState::Idle,
            CalendarConnectionState::Busy,
            CalendarConnectionState::Error { message: "boom".to_string(), retryable: true },
        ] {
            assert_eq!(calendar_account_status_text(&state, true), None);
        }
    }

    #[test]
    fn account_status_maps_connecting_and_disconnected() {
        assert_eq!(
            calendar_account_status_text(&CalendarConnectionState::Connecting, false).as_deref(),
            Some("Connecting…")
        );
        assert_eq!(
            calendar_account_status_text(&CalendarConnectionState::Disconnected, false).as_deref(),
            Some("Disconnected")
        );
    }

    #[test]
    fn account_status_says_no_calendars_when_idle_or_busy_with_none() {
        for state in [CalendarConnectionState::Idle, CalendarConnectionState::Busy] {
            assert_eq!(
                calendar_account_status_text(&state, false).as_deref(),
                Some("No calendars found")
            );
        }
    }

    #[test]
    fn account_status_surfaces_the_session_error() {
        let state = CalendarConnectionState::Error { message: "login failed".to_string(), retryable: true };
        assert_eq!(calendar_account_status_text(&state, false).as_deref(), Some("login failed"));
    }
}
