//! Date-and-time picking shared across the app: the event editor's date
//! button plus `hh:mm` spin pair, and the "pick a time" preset menus with a
//! custom-time dialog behind them - schedule send (item 1.14) and snooze
//! (item 1.15) - plus the Flag menu's date-only dialog (item 1.16).

use std::cell::Cell;
use std::rc::Rc;

use adw::prelude::*;
use chrono::{DateTime, Datelike, Local, NaiveDate, NaiveDateTime, Timelike, Utc};

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
    let field_label = gtk::Label::builder().label(label).css_classes(["dim-label", "caption"]).xalign(0.0).build();
    crate::privacy::mark_ui(&field_label);
    cluster.append(&field_label);
    let row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(8).build();
    row.append(&date_button);
    row.append(&time_box);
    cluster.append(&row);
    cluster
}

/// One entry of a preset time menu: its name ("Tomorrow") and the instant.
pub(crate) struct TimeChoice {
    pub name: String,
    pub at: DateTime<Local>,
}

/// The custom-time dialog behind a preset menu's last entry.
pub(crate) struct TimeDialog {
    pub heading: &'static str,
    /// The caption above the date and time ("Send at").
    pub field_label: &'static str,
    /// The accept button ("Schedule").
    pub accept: &'static str,
    /// An optional dim note under the picker.
    pub note: Option<&'static str>,
}

/// A preset time menu (Outlook's Schedule send and Snooze menus): a heading,
/// the presets `presets` computes for the current time - rebuilt every time
/// the menu opens, so "Tomorrow" never goes stale - each showing its day and
/// time, then `custom_label`, which opens `dialog`. `on_pick` receives the
/// chosen instant.
pub(crate) fn build_time_preset_popover(
    heading: &'static str,
    presets: Rc<dyn Fn(DateTime<Local>) -> Vec<TimeChoice>>,
    custom_label: &'static str,
    dialog: TimeDialog,
    on_pick: Rc<dyn Fn(DateTime<Utc>)>,
) -> gtk::Popover {
    let list = gtk::Box::new(gtk::Orientation::Vertical, 2);
    let popover = gtk::Popover::builder().child(&list).build();
    popover.add_css_class("menu");
    let dialog = Rc::new(dialog);
    let fill = {
        let list = list.clone();
        let popover = popover.clone();
        move || {
            while let Some(child) = list.first_child() {
                list.remove(&child);
            }
            let heading = gtk::Label::builder()
                .label(heading)
                .css_classes(["heading"])
                .xalign(0.0)
                .margin_start(8)
                .margin_top(4)
                .margin_bottom(4)
                .build();
            crate::privacy::mark_ui(&heading);
            list.append(&heading);
            let now = Local::now();
            for choice in presets(now) {
                let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
                row.append(&gtk::Label::builder().label(&choice.name).xalign(0.0).hexpand(true).build());
                row.append(&gtk::Label::builder().label(format_short_time(&choice.at, &now)).css_classes(["dim-label"]).build());
                let button = gtk::Button::builder().child(&row).css_classes(["flat"]).build();
                let on_pick = on_pick.clone();
                let popover = popover.clone();
                let at = choice.at.with_timezone(&Utc);
                button.connect_clicked(move |_| {
                    popover.popdown();
                    on_pick(at);
                });
                list.append(&button);
            }
            list.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
            let custom = gtk::Button::builder()
                .child(&gtk::Label::builder().label(custom_label).xalign(0.0).build())
                .css_classes(["flat"])
                .build();
            let on_pick = on_pick.clone();
            let popover_for_custom = popover.clone();
            let dialog = dialog.clone();
            custom.connect_clicked(move |_| {
                popover_for_custom.popdown();
                let parent = popover_for_custom.parent();
                present_time_dialog(parent.as_ref(), &dialog, on_pick.clone());
            });
            list.append(&custom);
        }
    };
    fill();
    popover.connect_show(move |_| fill());
    popover
}

/// The custom-time dialog: a date and time, starting at the next half hour,
/// that must be in the future before the accept button is enabled.
pub(crate) fn present_time_dialog(parent: Option<&gtk::Widget>, spec: &TimeDialog, on_pick: Rc<dyn Fn(DateTime<Utc>)>) {
    let initial = next_half_hour(Local::now().naive_local());
    let date = Rc::new(Cell::new(initial.date()));
    let calendar = gtk::Calendar::new();
    set_calendar_date(&calendar, &date, initial.date());
    wire_calendar(&calendar, &date);
    let hour = gtk::SpinButton::with_range(0.0, 23.0, 1.0);
    let minute = gtk::SpinButton::with_range(0.0, 59.0, 1.0);
    for spin in [&hour, &minute] {
        spin.set_digits(0);
        spin.set_width_chars(2);
    }
    hour.set_value(f64::from(initial.hour()));
    minute.set_value(f64::from(initial.minute()));

    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.append(&time_cluster(spec.field_label, &calendar, &hour, &minute));
    if let Some(note) = spec.note {
        let note = gtk::Label::builder().label(note).css_classes(["dim-label", "caption"]).wrap(true).xalign(0.0).build();
        crate::privacy::mark_ui(&note);
        content.append(&note);
    }
    let dialog = adw::AlertDialog::builder()
        .heading(spec.heading)
        .default_response("accept")
        .close_response("cancel")
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("accept", spec.accept);
    dialog.set_response_appearance("accept", adw::ResponseAppearance::Suggested);
    dialog.set_extra_child(Some(&content));

    let chosen = {
        let date = date.clone();
        let hour = hour.clone();
        let minute = minute.clone();
        move || {
            let time = chrono::NaiveTime::from_hms_opt(hour.value() as u32, minute.value() as u32, 0)?;
            future_instant(date.get().and_time(time), &Local::now())
        }
    };
    let refresh: Rc<dyn Fn()> = {
        let dialog = dialog.clone();
        let chosen = chosen.clone();
        Rc::new(move || dialog.set_response_enabled("accept", chosen().is_some()))
    };
    refresh();
    {
        let refresh = refresh.clone();
        calendar.connect_day_selected(move |_| refresh());
    }
    for spin in [&hour, &minute] {
        let refresh = refresh.clone();
        spin.connect_value_changed(move |_| refresh());
    }
    dialog.connect_response(None, move |_, response| {
        if response == "accept" {
            if let Some(at) = chosen() {
                on_pick(at);
            }
        }
    });
    dialog.present(parent);
}

/// A date-only dialog (the Flag menu's "Custom date…", item 1.16): a
/// calendar starting on `initial`. Any date is accepted - a follow-up flag
/// may be due today, or even be back-dated.
pub(crate) fn present_date_dialog(parent: Option<&gtk::Widget>, heading: &str, field_label: &str, accept: &str, initial: NaiveDate, on_pick: Rc<dyn Fn(NaiveDate)>) {
    let date = Rc::new(Cell::new(initial));
    let calendar = gtk::Calendar::new();
    set_calendar_date(&calendar, &date, initial);
    wire_calendar(&calendar, &date);
    let content = gtk::Box::new(gtk::Orientation::Vertical, 2);
    let field_label_widget = gtk::Label::builder().label(field_label).css_classes(["dim-label", "caption"]).xalign(0.0).build();
    crate::privacy::mark_ui(&field_label_widget);
    content.append(&field_label_widget);
    content.append(&calendar);
    let dialog = adw::AlertDialog::builder().heading(heading).default_response("accept").close_response("cancel").build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("accept", accept);
    dialog.set_response_appearance("accept", adw::ResponseAppearance::Suggested);
    dialog.set_extra_child(Some(&content));
    dialog.connect_response(None, move |_, response| {
        if response == "accept" {
            on_pick(date.get());
        }
    });
    dialog.present(parent);
}

/// A time picked as a local date and time: the instant it names, or `None`
/// when it isn't in the future (or doesn't exist, in a DST gap) - the
/// dialog keeps its accept button insensitive then.
pub(crate) fn future_instant<Tz: chrono::TimeZone>(local: NaiveDateTime, now: &DateTime<Tz>) -> Option<DateTime<Utc>> {
    let at = local.and_local_timezone(now.timezone()).earliest()?;
    (at > *now).then(|| at.with_timezone(&Utc))
}

/// How a picked time reads in menus, toasts and banners: "Tue 08:00" within
/// the coming week, "Tue 14 Oct 08:00" further out.
pub(crate) fn format_short_time<Tz: chrono::TimeZone>(at: &DateTime<Tz>, now: &DateTime<Tz>) -> String
where
    Tz::Offset: std::fmt::Display,
{
    if at.date_naive() < now.date_naive() + chrono::Days::new(7) {
        at.format("%a %H:%M").to_string()
    } else {
        at.format("%a %-d %b %H:%M").to_string()
    }
}

/// The custom time's starting point: the next half hour after `now`.
fn next_half_hour(now: NaiveDateTime) -> NaiveDateTime {
    let base = now.with_second(0).and_then(|t| t.with_nanosecond(0)).unwrap_or(now);
    let minutes_past = base.minute() % 30;
    base + chrono::Duration::minutes(i64::from(30 - minutes_past))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn local(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<chrono::FixedOffset> {
        chrono::FixedOffset::east_opt(2 * 3600).unwrap().with_ymd_and_hms(y, m, d, h, min, 0).unwrap()
    }

    /// A custom time must be in the future; a valid one comes back in UTC.
    #[test]
    fn custom_times_must_be_in_the_future() {
        let now = local(2026, 10, 12, 10, 0);
        let at = |h, m| NaiveDate::from_ymd_opt(2026, 10, 12).unwrap().and_hms_opt(h, m, 0).unwrap();
        assert_eq!(future_instant(at(9, 59), &now), None);
        assert_eq!(future_instant(at(10, 0), &now), None, "now isn't the future");
        assert_eq!(future_instant(at(10, 30), &now), Some(Utc.with_ymd_and_hms(2026, 10, 12, 8, 30, 0).unwrap()));
    }

    #[test]
    fn picked_times_read_short_within_the_week() {
        let now = local(2026, 10, 12, 10, 0); // Monday
        assert_eq!(format_short_time(&local(2026, 10, 13, 8, 0), &now), "Tue 08:00");
        assert_eq!(format_short_time(&local(2026, 10, 20, 8, 0), &now), "Tue 20 Oct 08:00");
    }

    #[test]
    fn custom_time_starts_at_the_next_half_hour() {
        let at = |h, m, s| NaiveDate::from_ymd_opt(2026, 10, 12).unwrap().and_hms_opt(h, m, s).unwrap();
        assert_eq!(next_half_hour(at(10, 7, 33)), at(10, 30, 0));
        assert_eq!(next_half_hour(at(10, 30, 0)), at(11, 0, 0));
        assert_eq!(next_half_hour(at(23, 50, 0)), NaiveDate::from_ymd_opt(2026, 10, 13).unwrap().and_hms_opt(0, 0, 0).unwrap());
    }
}
