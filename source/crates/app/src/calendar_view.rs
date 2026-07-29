use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use chrono::{Datelike, NaiveDate};
use gtk::prelude::*;
use lookout_core::EventOccurrence;

const WEEKDAY_LABELS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
/// Fixed Monday-first week start (matches most of the world outside the US)
/// rather than locale-detected - an explicit simplification for this pass.
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

pub fn build() -> MonthGrid {
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
            let date_label = gtk::Label::builder().xalign(1.0).margin_end(4).css_classes(["caption"]).build();
            let events_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(1).vexpand(true).build();
            let container = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .css_classes(["card"])
                .margin_start(1)
                .margin_end(1)
                .margin_top(1)
                .margin_bottom(1)
                .build();
            container.append(&date_label);
            container.append(&events_box);
            grid.attach(&container, col, row + 1, 1, 1);
            day_cells.push(DayCell { container, date_label, events_box });
        }
    }

    let root_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
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
        } else {
            cell.date_label.remove_css_class("accent");
            cell.date_label.remove_css_class("heading");
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

fn clear_children(container: &gtk::Box) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
}

fn first_of_month(date: NaiveDate) -> NaiveDate {
    date.with_day(1).unwrap_or(date)
}

/// The Monday that starts the grid's first row - on or before the 1st of `month`.
fn first_grid_day(month: NaiveDate) -> NaiveDate {
    let days_since_monday = month.weekday().num_days_from_monday() as i64;
    month - chrono::Duration::days(days_since_monday)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_grid_day_lands_on_monday_on_or_before_the_1st() {
        // 2026-07-01 is a Wednesday; the grid should start on Monday 2026-06-29.
        let month = NaiveDate::from_ymd_opt(2026, 7, 1).unwrap();
        let grid_start = first_grid_day(month);
        assert_eq!(grid_start, NaiveDate::from_ymd_opt(2026, 6, 29).unwrap());
        assert_eq!(grid_start.weekday(), chrono::Weekday::Mon);
    }

    #[test]
    fn first_grid_day_is_unchanged_when_month_already_starts_on_monday() {
        // 2026-06-01 is itself a Monday.
        let month = NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();
        assert_eq!(first_grid_day(month), month);
    }
}
