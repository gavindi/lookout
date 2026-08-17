//! A small, dependency-free RFC 5545 `RECUR`-value builder/parser plus the
//! "Series" popover UI it drives in the event editor. Deliberately doesn't
//! depend on the `rrule` crate (that stays a `lookout-dav`-only dependency,
//! used only for expanding a series into occurrences) - the subset of RECUR
//! syntax this builder edits (FREQ/INTERVAL/BYDAY/COUNT/UNTIL) is a simple
//! `KEY=VALUE;...` grammar that doesn't need a full RRULE engine to read or
//! write.
//!
//! Like `event_editor.rs`, this module is data-in/callback-out: it knows
//! nothing about `CalendarEvent` or the network, only the raw `rrule` string
//! `CalendarEvent::rrule` already carries.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use chrono::{Datelike, NaiveDate, Weekday};
use gtk::prelude::*;

/// One recurrence rule, editable in the UI. Round-trips through
/// [`to_rrule_string`]/[`parse_rrule_string`] to the raw value
/// `CalendarEvent::rrule` carries - this struct is never itself stored or
/// sent over the wire.
#[derive(Debug, Clone, PartialEq)]
pub struct RecurrenceRule {
    pub freq: Frequency,
    /// "Every N `freq`s" - RFC 5545 default is 1 when `INTERVAL` is absent.
    pub interval: u32,
    /// Only meaningful when `freq` is `Weekly`.
    pub by_weekday: Vec<Weekday>,
    pub end: RecurrenceEnd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Frequency {
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecurrenceEnd {
    Never,
    After(u32),
    On(NaiveDate),
}

/// Parses an RFC 5545 `RECUR` value into a [`RecurrenceRule`]. Returns `None`
/// when the rule can't be losslessly represented by this builder (an unknown
/// `FREQ`, or any part this builder doesn't model - `BYMONTHDAY`,
/// `BYSETPOS`, etc.) - the caller must then treat the rule as opaque/custom
/// and keep the original raw string untouched on save, rather than silently
/// truncating a richer rule than this parser understands.
pub fn parse_rrule_string(raw: &str) -> Option<RecurrenceRule> {
    let mut freq = None;
    let mut interval = 1u32;
    let mut by_weekday = Vec::new();
    let mut count = None;
    let mut until = None;

    for part in raw.split(';') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (key, value) = part.split_once('=')?;
        match key {
            "FREQ" => {
                freq = Some(match value {
                    "DAILY" => Frequency::Daily,
                    "WEEKLY" => Frequency::Weekly,
                    "MONTHLY" => Frequency::Monthly,
                    "YEARLY" => Frequency::Yearly,
                    _ => return None,
                });
            }
            "INTERVAL" => interval = value.parse().ok()?,
            "BYDAY" => {
                for code in value.split(',') {
                    by_weekday.push(weekday_from_code(code)?);
                }
            }
            "COUNT" => count = Some(value.parse().ok()?),
            "UNTIL" => until = Some(parse_until_date(value)?),
            // Anything else (BYMONTHDAY, BYSETPOS, WKST, ...) isn't modeled -
            // bail rather than silently drop it.
            _ => return None,
        }
    }

    if count.is_some() && until.is_some() {
        // RFC 5545 §3.3.10: COUNT and UNTIL are mutually exclusive - a value
        // with both isn't a rule this builder could have produced.
        return None;
    }

    Some(RecurrenceRule {
        freq: freq?,
        interval,
        by_weekday,
        end: match (count, until) {
            (Some(n), None) => RecurrenceEnd::After(n),
            (None, Some(date)) => RecurrenceEnd::On(date),
            (None, None) => RecurrenceEnd::Never,
            (Some(_), Some(_)) => unreachable!("rejected above"),
        },
    })
}

fn weekday_from_code(code: &str) -> Option<Weekday> {
    Some(match code.trim() {
        "MO" => Weekday::Mon,
        "TU" => Weekday::Tue,
        "WE" => Weekday::Wed,
        "TH" => Weekday::Thu,
        "FR" => Weekday::Fri,
        "SA" => Weekday::Sat,
        "SU" => Weekday::Sun,
        _ => return None,
    })
}

fn weekday_to_code(day: Weekday) -> &'static str {
    match day {
        Weekday::Mon => "MO",
        Weekday::Tue => "TU",
        Weekday::Wed => "WE",
        Weekday::Thu => "TH",
        Weekday::Fri => "FR",
        Weekday::Sat => "SA",
        Weekday::Sun => "SU",
    }
}

/// `UNTIL` may be a bare `DATE` (`YYYYMMDD`) or a UTC `DATE-TIME`
/// (`YYYYMMDDTHHMMSSZ`); only the date part matters to this builder's
/// day-granularity end picker.
fn parse_until_date(value: &str) -> Option<NaiveDate> {
    let date_part = value.split('T').next()?;
    if date_part.len() != 8 {
        return None;
    }
    let year = date_part.get(0..4)?.parse().ok()?;
    let month = date_part.get(4..6)?.parse().ok()?;
    let day = date_part.get(6..8)?.parse().ok()?;
    NaiveDate::from_ymd_opt(year, month, day)
}

/// The inverse of [`parse_rrule_string`], e.g.
/// `FREQ=WEEKLY;INTERVAL=2;BYDAY=MO,WE;COUNT=10`.
pub fn to_rrule_string(rule: &RecurrenceRule) -> String {
    let mut parts = vec![format!(
        "FREQ={}",
        match rule.freq {
            Frequency::Daily => "DAILY",
            Frequency::Weekly => "WEEKLY",
            Frequency::Monthly => "MONTHLY",
            Frequency::Yearly => "YEARLY",
        }
    )];
    if rule.interval > 1 {
        parts.push(format!("INTERVAL={}", rule.interval));
    }
    if rule.freq == Frequency::Weekly && !rule.by_weekday.is_empty() {
        let days = rule.by_weekday.iter().map(|d| weekday_to_code(*d)).collect::<Vec<_>>().join(",");
        parts.push(format!("BYDAY={days}"));
    }
    match rule.end {
        RecurrenceEnd::Never => {}
        RecurrenceEnd::After(n) => parts.push(format!("COUNT={n}")),
        RecurrenceEnd::On(date) => parts.push(format!("UNTIL={}T235959Z", date.format("%Y%m%d"))),
    }
    parts.join(";")
}

/// A human summary for the collapsed "Series" button label.
pub fn describe(rule: &RecurrenceRule) -> String {
    let freq = match (rule.freq, rule.interval) {
        (Frequency::Daily, 1) => "Daily".to_string(),
        (Frequency::Daily, n) => format!("Every {n} days"),
        (Frequency::Weekly, 1) => "Weekly".to_string(),
        (Frequency::Weekly, n) => format!("Every {n} weeks"),
        (Frequency::Monthly, 1) => "Monthly".to_string(),
        (Frequency::Monthly, n) => format!("Every {n} months"),
        (Frequency::Yearly, 1) => "Yearly".to_string(),
        (Frequency::Yearly, n) => format!("Every {n} years"),
    };
    let days = if rule.freq == Frequency::Weekly && !rule.by_weekday.is_empty() {
        let names = rule.by_weekday.iter().map(|d| short_weekday_name(*d)).collect::<Vec<_>>().join(", ");
        format!(" on {names}")
    } else {
        String::new()
    };
    let end = match rule.end {
        RecurrenceEnd::Never => String::new(),
        RecurrenceEnd::After(n) => format!(", {n} times"),
        RecurrenceEnd::On(date) => format!(", until {}", date.format("%b %-d, %Y")),
    };
    format!("{freq}{days}{end}")
}

fn short_weekday_name(day: Weekday) -> &'static str {
    match day {
        Weekday::Mon => "Mon",
        Weekday::Tue => "Tue",
        Weekday::Wed => "Wed",
        Weekday::Thu => "Thu",
        Weekday::Fri => "Fri",
        Weekday::Sat => "Sat",
        Weekday::Sun => "Sun",
    }
}

/// The result of opening the series popover: the button to place in the
/// editor's top action row, plus a getter for the current selection (read on
/// Save, rather than threading a live value through the `on_change`
/// callback's every intermediate state).
pub struct SeriesControl {
    pub button: gtk::MenuButton,
    current: std::rc::Rc<std::cell::RefCell<Option<RecurrenceRule>>>,
    /// `Some` when the initial rule couldn't be parsed by this builder (a
    /// "custom" recurrence) - kept so Save can preserve the original string
    /// untouched instead of losing it to a builder round-trip.
    pub unrepresentable_raw: Option<String>,
}

impl SeriesControl {
    /// The rule currently selected in the popover, or `None` for "does not
    /// repeat". Always `None` while [`Self::unrepresentable_raw`] is set -
    /// the popover stays in its inert "Custom recurrence" state until the
    /// user actively picks a frequency, which clears the custom string.
    pub fn current(&self) -> Option<RecurrenceRule> {
        self.current.borrow().clone()
    }
}

/// Builds the Series `MenuButton` + popover. `initial_raw` is the event's
/// existing raw `rrule` string, if any (`None` for a brand-new event).
pub fn build_series_control(initial_raw: Option<&str>) -> SeriesControl {
    let parsed = initial_raw.map(parse_rrule_string);
    let unrepresentable_raw = match &parsed {
        Some(None) => initial_raw.map(str::to_string),
        _ => None,
    };
    let initial = parsed.flatten();

    let current = std::rc::Rc::new(std::cell::RefCell::new(initial.clone()));
    let button = gtk::MenuButton::builder().icon_name("media-playlist-repeat-symbolic").build();
    update_button_label(&button, current.borrow().as_ref(), unrepresentable_raw.as_deref());

    let popover = gtk::Popover::new();
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();

    let freq_model = gtk::StringList::new(&["Does not repeat", "Daily", "Weekly", "Monthly", "Yearly"]);
    let freq_dropdown = gtk::DropDown::builder().model(&freq_model).build();
    freq_dropdown.set_selected(match &initial {
        None => 0,
        Some(r) => match r.freq {
            Frequency::Daily => 1,
            Frequency::Weekly => 2,
            Frequency::Monthly => 3,
            Frequency::Yearly => 4,
        },
    });
    content.append(&freq_dropdown);

    let interval_row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).build();
    interval_row.append(&gtk::Label::new(Some("Every")));
    let interval_spin = gtk::SpinButton::with_range(1.0, 999.0, 1.0);
    interval_spin.set_value(f64::from(initial.as_ref().map(|r| r.interval).unwrap_or(1)));
    interval_row.append(&interval_spin);
    let interval_unit_label = gtk::Label::new(Some("time(s)"));
    interval_row.append(&interval_unit_label);
    content.append(&interval_row);

    let weekday_row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(4).build();
    let weekday_toggles: Vec<(Weekday, gtk::ToggleButton)> = [Weekday::Mon, Weekday::Tue, Weekday::Wed, Weekday::Thu, Weekday::Fri, Weekday::Sat, Weekday::Sun]
        .into_iter()
        .map(|day| {
            let toggle = gtk::ToggleButton::builder().label(short_weekday_name(day)).build();
            toggle.set_active(initial.as_ref().is_some_and(|r| r.by_weekday.contains(&day)));
            weekday_row.append(&toggle);
            (day, toggle)
        })
        .collect();
    content.append(&weekday_row);

    let end_model = gtk::StringList::new(&["Never", "After", "On date"]);
    let end_dropdown = gtk::DropDown::builder().model(&end_model).build();
    end_dropdown.set_selected(match initial.as_ref().map(|r| &r.end) {
        None | Some(RecurrenceEnd::Never) => 0,
        Some(RecurrenceEnd::After(_)) => 1,
        Some(RecurrenceEnd::On(_)) => 2,
    });
    content.append(&end_dropdown);

    let end_count_spin = gtk::SpinButton::with_range(1.0, 999.0, 1.0);
    end_count_spin.set_value(f64::from(match initial.as_ref().map(|r| &r.end) {
        Some(RecurrenceEnd::After(n)) => *n,
        _ => 10,
    }));
    let end_date_calendar = gtk::Calendar::new();
    if let Some(RecurrenceEnd::On(date)) = initial.as_ref().map(|r| &r.end) {
        if let Ok(dt) = gtk::glib::DateTime::from_utc(date.year(), date.month() as i32, date.day() as i32, 0, 0, 0.0) {
            end_date_calendar.set_date(Some(&dt));
        }
    }
    content.append(&end_count_spin);
    content.append(&end_date_calendar);

    let update_visibility = {
        let weekday_row = weekday_row.clone();
        let interval_row = interval_row.clone();
        let end_dropdown = end_dropdown.clone();
        let end_count_spin = end_count_spin.clone();
        let end_date_calendar = end_date_calendar.clone();
        let freq_dropdown = freq_dropdown.clone();
        move || {
            let repeats = freq_dropdown.selected() != 0;
            interval_row.set_visible(repeats);
            weekday_row.set_visible(repeats && freq_dropdown.selected() == 2);
            end_dropdown.set_visible(repeats);
            end_count_spin.set_visible(repeats && end_dropdown.selected() == 1);
            end_date_calendar.set_visible(repeats && end_dropdown.selected() == 2);
        }
    };
    update_visibility();
    {
        let update_visibility = update_visibility.clone();
        freq_dropdown.connect_selected_notify(move |_| update_visibility());
    }
    {
        let update_visibility = update_visibility.clone();
        end_dropdown.connect_selected_notify(move |_| update_visibility());
    }

    let done_button = gtk::Button::with_label("Done");
    done_button.add_css_class("suggested-action");
    content.append(&done_button);
    popover.set_child(Some(&content));
    button.set_popover(Some(&popover));

    {
        let current = current.clone();
        let button = button.clone();
        let popover = popover.clone();
        let freq_dropdown = freq_dropdown.clone();
        let interval_spin = interval_spin.clone();
        let end_dropdown = end_dropdown.clone();
        let end_count_spin = end_count_spin.clone();
        let end_date_calendar = end_date_calendar.clone();
        done_button.connect_clicked(move |_| {
            let rule = if freq_dropdown.selected() == 0 {
                None
            } else {
                let freq = match freq_dropdown.selected() {
                    1 => Frequency::Daily,
                    2 => Frequency::Weekly,
                    3 => Frequency::Monthly,
                    _ => Frequency::Yearly,
                };
                let by_weekday = weekday_toggles.iter().filter(|(_, t)| t.is_active()).map(|(d, _)| *d).collect();
                let end = match end_dropdown.selected() {
                    1 => RecurrenceEnd::After(end_count_spin.value() as u32),
                    2 => {
                        // `GtkCalendar::month()` is 0-based (January = 0).
                        let date = NaiveDate::from_ymd_opt(end_date_calendar.year(), end_date_calendar.month() as u32 + 1, end_date_calendar.day() as u32)
                            .unwrap_or_else(|| chrono::Local::now().date_naive());
                        RecurrenceEnd::On(date)
                    }
                    _ => RecurrenceEnd::Never,
                };
                Some(RecurrenceRule {
                    freq,
                    interval: interval_spin.value() as u32,
                    by_weekday,
                    end,
                })
            };
            *current.borrow_mut() = rule.clone();
            update_button_label(&button, rule.as_ref(), None);
            popover.popdown();
        });
    }

    SeriesControl {
        button,
        current,
        unrepresentable_raw,
    }
}

fn update_button_label(button: &gtk::MenuButton, rule: Option<&RecurrenceRule>, unrepresentable_raw: Option<&str>) {
    let label = if unrepresentable_raw.is_some() {
        "Custom recurrence".to_string()
    } else {
        match rule {
            Some(r) => describe(r),
            None => "Does not repeat".to_string(),
        }
    };
    button.set_tooltip_text(Some(&label));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_weekly_with_byday_and_count() {
        let rule = parse_rrule_string("FREQ=WEEKLY;INTERVAL=2;BYDAY=MO,WE;COUNT=10").unwrap();
        assert_eq!(rule.freq, Frequency::Weekly);
        assert_eq!(rule.interval, 2);
        assert_eq!(rule.by_weekday, vec![Weekday::Mon, Weekday::Wed]);
        assert_eq!(rule.end, RecurrenceEnd::After(10));

        let reparsed = parse_rrule_string(&to_rrule_string(&rule)).unwrap();
        assert_eq!(reparsed, rule);
    }

    #[test]
    fn round_trips_monthly_until_date() {
        let rule = parse_rrule_string("FREQ=MONTHLY;UNTIL=20261231T000000Z").unwrap();
        assert_eq!(rule.freq, Frequency::Monthly);
        assert_eq!(rule.end, RecurrenceEnd::On(NaiveDate::from_ymd_opt(2026, 12, 31).unwrap()));

        let reparsed = parse_rrule_string(&to_rrule_string(&rule)).unwrap();
        assert_eq!(reparsed, rule);
    }

    #[test]
    fn parse_returns_none_for_unsupported_by_rules() {
        assert!(parse_rrule_string("FREQ=MONTHLY;BYMONTHDAY=15,-1").is_none());
    }

    #[test]
    fn parse_returns_none_for_count_and_until_together() {
        assert!(parse_rrule_string("FREQ=DAILY;COUNT=5;UNTIL=20261231T000000Z").is_none());
    }

    #[test]
    fn describe_produces_readable_summaries() {
        assert_eq!(
            describe(&RecurrenceRule {
                freq: Frequency::Weekly,
                interval: 1,
                by_weekday: vec![Weekday::Mon, Weekday::Wed],
                end: RecurrenceEnd::After(10)
            }),
            "Weekly on Mon, Wed, 10 times"
        );
        assert_eq!(
            describe(&RecurrenceRule {
                freq: Frequency::Daily,
                interval: 1,
                by_weekday: vec![],
                end: RecurrenceEnd::Never
            }),
            "Daily"
        );
        assert_eq!(
            describe(&RecurrenceRule {
                freq: Frequency::Monthly,
                interval: 3,
                by_weekday: vec![],
                end: RecurrenceEnd::Never
            }),
            "Every 3 months"
        );
    }
}
