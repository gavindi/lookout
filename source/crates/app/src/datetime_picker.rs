//! The date-and-time picker shared by the event editor and the composer's
//! "Schedule send" dialog (item 1.14): a date button opening a month grid in
//! a popover, beside an `hh:mm` spin-button pair.

use std::cell::Cell;
use std::rc::Rc;

use adw::prelude::*;
use chrono::{Datelike, NaiveDate};

/// Shows `date` on `cal` and records it in `tracked`.
pub(crate) fn set_calendar_date(cal: &gtk::Calendar, tracked: &Rc<Cell<NaiveDate>>, date: NaiveDate) {
    tracked.set(date);
    if let Ok(dt) = gtk::glib::DateTime::from_utc(date.year(), date.month() as i32, date.day() as i32, 0, 0, 0.0) {
        cal.set_date(Some(&dt));
    }
}

/// Keeps `tracked` in step with the day picked on `cal` (GtkCalendar's
/// date getters need v4_14, which this crate builds against).
pub(crate) fn wire_calendar(cal: &gtk::Calendar, tracked: &Rc<Cell<NaiveDate>>) {
    let tracked = tracked.clone();
    cal.connect_day_selected(move |cal| {
        if let Some(date) = NaiveDate::from_ymd_opt(cal.year(), cal.month() as u32 + 1, cal.day() as u32) {
            tracked.set(date);
        }
    });
}

/// The calendar's currently selected date, formatted for the date button's
/// label (e.g. "Wed, Aug 12").
pub(crate) fn format_calendar_date(cal: &gtk::Calendar) -> String {
    NaiveDate::from_ymd_opt(cal.year(), cal.month() as u32 + 1, cal.day() as u32)
        .map(|d| d.format("%a, %b %-d").to_string())
        .unwrap_or_default()
}

/// A compact date button (opening a popover with the full month grid) beside
/// an `hh:mm` spin-button pair, labeled above, for one end of an event's span.
pub(crate) fn time_cluster(label: &str, calendar: &gtk::Calendar, hour: &gtk::SpinButton, minute: &gtk::SpinButton) -> gtk::Box {
    let separator = gtk::Label::builder().label(":").css_classes(["dim-label"]).build();
    let time_box = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(2).valign(gtk::Align::Center).build();
    time_box.append(hour);
    time_box.append(&separator);
    time_box.append(minute);

    let date_label = gtk::Label::new(Some(&format_calendar_date(calendar)));
    let popover = gtk::Popover::builder().child(calendar).build();
    {
        let date_label = date_label.clone();
        let popover = popover.clone();
        calendar.connect_day_selected(move |cal| {
            date_label.set_label(&format_calendar_date(cal));
            popover.popdown();
        });
    }
    let date_button = gtk::MenuButton::builder().popover(&popover).child(&date_label).build();

    let cluster = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).build();
    cluster.append(&gtk::Label::builder().label(label).css_classes(["dim-label", "caption"]).xalign(0.0).build());
    let row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(8).build();
    row.append(&date_button);
    row.append(&time_box);
    cluster.append(&row);
    cluster
}
