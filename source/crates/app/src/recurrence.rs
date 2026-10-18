//! A small, dependency-free RFC 5545 `RECUR`-value builder/parser plus the
//! "Series" popover UI it drives in the event editor. Deliberately doesn't
//! depend on the `rrule` crate (that stays a `lookout-dav`-only dependency,
//! used only for expanding a series into occurrences) - the subset of RECUR
//! syntax this builder edits is a simple `KEY=VALUE;...` grammar that doesn't
//! need a full RRULE engine to read or write.
//!
//! The builder models Outlook's recurrence choices (OUTLOOK_PARITY.md item
//! 3.10 / CAL-13): daily, weekly by weekday, monthly on a day-of-month or
//! "the Nth weekday", the "last weekday" form (`BYDAY=MO..FR;BYSETPOS=-1`),
//! and yearly by month/day or month/Nth-weekday - plus a count or end date.
//! A rule outside that subset (sub-daily frequencies, several `BYMONTHDAY`s,
//! an arbitrary `BYSETPOS` set, `BYYEARDAY`, ...) parses as `None`, and the
//! caller keeps the original raw string untouched rather than truncating it.
//!
//! Like `event_editor.rs`, this module is data-in/callback-out: it knows
//! nothing about `CalendarEvent` or the network, only the raw `rrule` string
//! `CalendarEvent::rrule` already carries.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use chrono::{Datelike, NaiveDate, Weekday};
use gtk::prelude::*;

/// The five Monday-to-Friday weekdays, the set "last weekday" is built from
/// (`BYDAY=MO,TU,WE,TH,FR;BYSETPOS=-1`).
const WORKDAYS: [Weekday; 5] = [Weekday::Mon, Weekday::Tue, Weekday::Wed, Weekday::Thu, Weekday::Fri];

const MONTH_NAMES: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// The "On" choices shared by monthly and yearly rules. The first is a
/// day-of-month; the next four are an ordinal weekday; the fifth is the last
/// weekday; the last two are the `BYSETPOS` weekday-set forms.
const MODE_LABELS: [&str; 8] = [
    "Day",
    "The first",
    "The second",
    "The third",
    "The fourth",
    "The last",
    "The first weekday",
    "The last weekday",
];

/// One recurrence rule, editable in the UI. Round-trips through
/// [`to_rrule_string`]/[`parse_rrule_string`] to the raw value
/// `CalendarEvent::rrule` carries - this struct is never itself stored or
/// sent over the wire.
#[derive(Debug, Clone, PartialEq)]
pub struct RecurrenceRule {
    pub pattern: RecurrencePattern,
    /// "Every N `pattern`s" - RFC 5545 default is 1 when `INTERVAL` is absent.
    pub interval: u32,
    /// A `WKST` the source rule carried. The builder has no control for it, so
    /// it is preserved untouched across an edit rather than dropped.
    pub wkst: Option<Weekday>,
    pub end: RecurrenceEnd,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RecurrencePattern {
    Daily,
    Weekly { days: Vec<Weekday> },
    Monthly(MonthPattern),
    Yearly { month: u32, on: MonthPattern },
}

/// How a monthly (or a yearly rule's chosen month) recurrence picks its day.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MonthPattern {
    /// `BYMONTHDAY=n`: 1-31, or `-1` for the month's last day.
    DayOfMonth(i32),
    /// `BYDAY=2TU`: the Nth (or last) named weekday.
    NthWeekday { ordinal: Ordinal, weekday: Weekday },
    /// `BYDAY=<set>;BYSETPOS=<ordinal>`: Outlook's "last weekday" (and the
    /// first-weekday form), the only set-position shape the builder offers.
    NthWeekdayOfSet { ordinal: Ordinal, weekdays: Vec<Weekday> },
}

/// An ordinal position a monthly/yearly rule can use. Outlook offers the first
/// four and the last; Graph's recurrence model knows the same set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ordinal {
    First,
    Second,
    Third,
    Fourth,
    Last,
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

impl RecurrencePattern {
    pub fn frequency(&self) -> Frequency {
        match self {
            RecurrencePattern::Daily => Frequency::Daily,
            RecurrencePattern::Weekly { .. } => Frequency::Weekly,
            RecurrencePattern::Monthly(_) => Frequency::Monthly,
            RecurrencePattern::Yearly { .. } => Frequency::Yearly,
        }
    }
}

impl RecurrenceRule {
    pub fn frequency(&self) -> Frequency {
        self.pattern.frequency()
    }
}

impl Ordinal {
    fn from_ical(value: i8) -> Option<Self> {
        Some(match value {
            1 => Ordinal::First,
            2 => Ordinal::Second,
            3 => Ordinal::Third,
            4 => Ordinal::Fourth,
            -1 => Ordinal::Last,
            _ => return None,
        })
    }

    fn to_ical(self) -> i8 {
        match self {
            Ordinal::First => 1,
            Ordinal::Second => 2,
            Ordinal::Third => 3,
            Ordinal::Fourth => 4,
            Ordinal::Last => -1,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Ordinal::First => "first",
            Ordinal::Second => "second",
            Ordinal::Third => "third",
            Ordinal::Fourth => "fourth",
            Ordinal::Last => "last",
        }
    }
}

/// Parses an RFC 5545 `RECUR` value into a [`RecurrenceRule`]. Returns `None`
/// when the rule can't be losslessly represented by this builder (an unknown
/// `FREQ`, a sub-daily frequency, several `BYMONTHDAY`s, a `BYSETPOS` set this
/// UI can't show, ...) - the caller must then treat the rule as opaque/custom
/// and keep the original raw string untouched on save, rather than silently
/// truncating a richer rule than this parser understands.
///
/// `start_date` is the series anchor (the event's `DTSTART` local date): RFC
/// 5545 defaults a bare `FREQ=MONTHLY` to the start's day-of-month and a
/// bare `FREQ=YEARLY` to the start's month/day, so those resolve here.
pub fn parse_rrule_string(raw: &str, start_date: NaiveDate) -> Option<RecurrenceRule> {
    let mut freq = None;
    let mut interval = 1u32;
    let mut by_weekday: Vec<(Option<i8>, Weekday)> = Vec::new();
    let mut by_month_day: Vec<i32> = Vec::new();
    let mut by_month: Vec<u32> = Vec::new();
    let mut by_set_pos: Vec<i8> = Vec::new();
    let mut wkst = None;
    let mut count = None;
    let mut until = None;

    for part in raw.split(';') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (key, value) = part.split_once('=')?;
        let value = value.trim();
        match key.trim().to_ascii_uppercase().as_str() {
            "FREQ" => {
                freq = Some(match value.to_ascii_uppercase().as_str() {
                    "DAILY" => Frequency::Daily,
                    "WEEKLY" => Frequency::Weekly,
                    "MONTHLY" => Frequency::Monthly,
                    "YEARLY" => Frequency::Yearly,
                    _ => return None,
                });
            }
            "INTERVAL" => interval = value.parse::<u32>().ok()?.max(1),
            "BYDAY" => {
                for code in value.split(',') {
                    by_weekday.push(parse_byday(code.trim())?);
                }
            }
            "BYMONTHDAY" => {
                for day in value.split(',') {
                    by_month_day.push(day.trim().parse().ok()?);
                }
            }
            "BYMONTH" => {
                for month in value.split(',') {
                    by_month.push(month.trim().parse::<u32>().ok().filter(|month| (1..=12).contains(month))?);
                }
            }
            "BYSETPOS" => {
                for position in value.split(',') {
                    by_set_pos.push(position.trim().parse::<i8>().ok()?);
                }
            }
            "WKST" => wkst = Some(weekday_from_code(value)?),
            "COUNT" => count = Some(value.parse::<u32>().ok().filter(|count| *count > 0)?),
            "UNTIL" => until = Some(parse_until_date(value)?),
            // Anything else (BYYEARDAY, BYHOUR, ...) isn't modeled - bail
            // rather than silently drop it.
            _ => return None,
        }
    }

    if count.is_some() && until.is_some() {
        // RFC 5545 §3.3.10: COUNT and UNTIL are mutually exclusive - a value
        // with both isn't a rule this builder could have produced.
        return None;
    }
    // Only a single set position is modelled.
    if by_set_pos.len() > 1 {
        return None;
    }
    let set_pos = by_set_pos.first().copied();

    let freq = freq?;
    let pattern = match freq {
        Frequency::Daily => {
            if !by_weekday.is_empty() || !by_month_day.is_empty() || !by_month.is_empty() || set_pos.is_some() {
                return None;
            }
            RecurrencePattern::Daily
        }
        Frequency::Weekly => {
            // Weekly days are plain weekdays (no ordinal), and no other BY
            // part selects days.
            if by_weekday.iter().any(|(ordinal, _)| ordinal.is_some()) || !by_month_day.is_empty() || !by_month.is_empty() || set_pos.is_some() {
                return None;
            }
            RecurrencePattern::Weekly {
                days: sorted_days(by_weekday.iter().map(|(_, day)| *day)),
            }
        }
        Frequency::Monthly => {
            if !by_month.is_empty() {
                return None;
            }
            RecurrencePattern::Monthly(month_pattern(&by_weekday, &by_month_day, set_pos, start_date)?)
        }
        Frequency::Yearly => {
            let month = match by_month.as_slice() {
                // A bare yearly rule repeats on DTSTART's month/day.
                [] => start_date.month(),
                [month] => *month,
                _ => return None,
            };
            // With no BYMONTH, a BY* part would select days across the whole
            // year, not "the same day each year".
            if by_month.is_empty() && (!by_weekday.is_empty() || !by_month_day.is_empty() || set_pos.is_some()) {
                return None;
            }
            RecurrencePattern::Yearly {
                month,
                on: month_pattern(&by_weekday, &by_month_day, set_pos, start_date)?,
            }
        }
    };

    Some(RecurrenceRule {
        pattern,
        interval,
        wkst,
        end: match (count, until) {
            (Some(n), None) => RecurrenceEnd::After(n),
            (None, Some(date)) => RecurrenceEnd::On(date),
            (None, None) => RecurrenceEnd::Never,
            (Some(_), Some(_)) => unreachable!("rejected above"),
        },
    })
}

/// Parses one `BYDAY` token: `MO`, `2TU`, `-1FR`, `+1MO`.
fn parse_byday(token: &str) -> Option<(Option<i8>, Weekday)> {
    let token = token.trim();
    let split = token.len().checked_sub(2)?;
    let weekday = weekday_from_code(token.get(split..)?)?;
    let ordinal = token.get(..split)?;
    Some((if ordinal.is_empty() { None } else { Some(ordinal.parse().ok()?) }, weekday))
}

/// The day-selection half of a monthly rule (or a yearly rule's month),
/// shared by both frequencies.
fn month_pattern(by_weekday: &[(Option<i8>, Weekday)], by_month_day: &[i32], set_pos: Option<i8>, start_date: NaiveDate) -> Option<MonthPattern> {
    if let Some(position) = set_pos {
        if !by_month_day.is_empty() || by_weekday.is_empty() || by_weekday.iter().any(|(ordinal, _)| ordinal.is_some()) {
            return None;
        }
        let ordinal = Ordinal::from_ical(position)?;
        let weekdays = sorted_days(by_weekday.iter().map(|(_, day)| *day));
        return if weekdays.as_slice() == WORKDAYS.as_slice() {
            // The UI offers first/last weekday only, so a set position in
            // between (e.g. "the second weekday") stays custom.
            if !matches!(ordinal, Ordinal::First | Ordinal::Last) {
                return None;
            }
            Some(MonthPattern::NthWeekdayOfSet { ordinal, weekdays })
        } else if let [weekday] = weekdays.as_slice() {
            // "2nd Tuesday" written as BYDAY=TU;BYSETPOS=2 - the same rule,
            // canonicalised to the ordinal-BYDAY form.
            Some(MonthPattern::NthWeekday { ordinal, weekday: *weekday })
        } else {
            None
        };
    }

    if !by_month_day.is_empty() {
        if !by_weekday.is_empty() {
            return None;
        }
        return match by_month_day {
            [day] if *day == -1 || (1..=31).contains(day) => Some(MonthPattern::DayOfMonth(*day)),
            _ => None,
        };
    }

    if !by_weekday.is_empty() {
        return match by_weekday {
            [(Some(ordinal), weekday)] => Some(MonthPattern::NthWeekday {
                ordinal: Ordinal::from_ical(*ordinal)?,
                weekday: *weekday,
            }),
            // "Every Monday of the month" has no single day; not modelled.
            _ => None,
        };
    }

    // No BY* selects the start's day-of-month (RFC 5545's default).
    Some(MonthPattern::DayOfMonth(start_date.day() as i32))
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

/// Monday-first, deduplicated weekday list, so the emitted `BYDAY` doesn't
/// depend on the order the user toggled days in.
fn sorted_days(days: impl IntoIterator<Item = Weekday>) -> Vec<Weekday> {
    let mut days: Vec<Weekday> = days.into_iter().collect();
    days.sort_by_key(|day| day.num_days_from_monday());
    days.dedup();
    days
}

/// `UNTIL` may be a bare `DATE` (`YYYYMMDD`) or a UTC `DATE-TIME`
/// (`YYYYMMDDTHHMMSSZ`); only the date part matters to this builder's
/// day-granularity end picker.
fn parse_until_date(value: &str) -> Option<NaiveDate> {
    let value = value.trim().to_ascii_uppercase();
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
///
/// `all_day` picks the RFC's date `UNTIL` form for an all-day series (a
/// `DATE-TIME` beside a `VALUE=DATE` start is technically mismatched) and the
/// UTC date-time form for a timed one.
pub fn to_rrule_string(rule: &RecurrenceRule, all_day: bool) -> String {
    let mut parts = vec![format!(
        "FREQ={}",
        match rule.frequency() {
            Frequency::Daily => "DAILY",
            Frequency::Weekly => "WEEKLY",
            Frequency::Monthly => "MONTHLY",
            Frequency::Yearly => "YEARLY",
        }
    )];
    if rule.interval > 1 {
        parts.push(format!("INTERVAL={}", rule.interval));
    }
    match &rule.pattern {
        RecurrencePattern::Daily => {}
        RecurrencePattern::Weekly { days } => {
            if !days.is_empty() {
                let days = days.iter().map(|day| weekday_to_code(*day)).collect::<Vec<_>>().join(",");
                parts.push(format!("BYDAY={days}"));
            }
        }
        RecurrencePattern::Monthly(pattern) => append_month_pattern(&mut parts, pattern, None),
        RecurrencePattern::Yearly { month, on } => append_month_pattern(&mut parts, on, Some(*month)),
    }
    if let Some(wkst) = rule.wkst {
        parts.push(format!("WKST={}", weekday_to_code(wkst)));
    }
    match rule.end {
        RecurrenceEnd::Never => {}
        RecurrenceEnd::After(n) => parts.push(format!("COUNT={n}")),
        RecurrenceEnd::On(date) => {
            if all_day {
                parts.push(format!("UNTIL={}", date.format("%Y%m%d")));
            } else {
                parts.push(format!("UNTIL={}T235959Z", date.format("%Y%m%d")));
            }
        }
    }
    parts.join(";")
}

/// Appends a monthly/yearly day selection's `BY*` parts in RFC order
/// (`BYMONTH`, then `BYDAY`/`BYMONTHDAY`, then `BYSETPOS`).
fn append_month_pattern(parts: &mut Vec<String>, pattern: &MonthPattern, month: Option<u32>) {
    if let Some(month) = month {
        parts.push(format!("BYMONTH={month}"));
    }
    match pattern {
        MonthPattern::DayOfMonth(day) => parts.push(format!("BYMONTHDAY={day}")),
        MonthPattern::NthWeekday { ordinal, weekday } => {
            parts.push(format!("BYDAY={}{}", ordinal.to_ical(), weekday_to_code(*weekday)));
        }
        MonthPattern::NthWeekdayOfSet { ordinal, weekdays } => {
            let weekdays = weekdays.iter().map(|day| weekday_to_code(*day)).collect::<Vec<_>>().join(",");
            parts.push(format!("BYDAY={weekdays}"));
            parts.push(format!("BYSETPOS={}", ordinal.to_ical()));
        }
    }
}

/// A human summary for the collapsed "Series" button's tooltip, the form's
/// read-only preview row and the popover's live preview.
pub fn describe(rule: &RecurrenceRule) -> String {
    let freq = match (rule.frequency(), rule.interval) {
        (Frequency::Daily, 1) => "Daily".to_string(),
        (Frequency::Daily, n) => format!("Every {n} days"),
        (Frequency::Weekly, 1) => "Weekly".to_string(),
        (Frequency::Weekly, n) => format!("Every {n} weeks"),
        (Frequency::Monthly, 1) => "Monthly".to_string(),
        (Frequency::Monthly, n) => format!("Every {n} months"),
        (Frequency::Yearly, 1) => "Yearly".to_string(),
        (Frequency::Yearly, n) => format!("Every {n} years"),
    };
    let detail = match &rule.pattern {
        RecurrencePattern::Daily => String::new(),
        RecurrencePattern::Weekly { days } if !days.is_empty() => {
            let names = days.iter().map(|day| short_weekday_name(*day)).collect::<Vec<_>>().join(", ");
            format!(" on {names}")
        }
        RecurrencePattern::Weekly { .. } => String::new(),
        RecurrencePattern::Monthly(pattern) => monthly_detail(pattern),
        RecurrencePattern::Yearly { month, on } => yearly_detail(*month, on),
    };
    let end = match rule.end {
        RecurrenceEnd::Never => String::new(),
        RecurrenceEnd::After(n) => format!(", {n} times"),
        RecurrenceEnd::On(date) => format!(", until {}", date.format("%b %-d, %Y")),
    };
    format!("{freq}{detail}{end}")
}

fn monthly_detail(pattern: &MonthPattern) -> String {
    match pattern {
        MonthPattern::DayOfMonth(-1) => " on the last day".to_string(),
        MonthPattern::DayOfMonth(day) => format!(" on day {day}"),
        MonthPattern::NthWeekday { ordinal, weekday } => format!(" on the {} {}", ordinal.label(), long_weekday_name(*weekday)),
        MonthPattern::NthWeekdayOfSet { ordinal, weekdays } if weekdays.as_slice() == WORKDAYS.as_slice() => {
            format!(" on the {} weekday", ordinal.label())
        }
        MonthPattern::NthWeekdayOfSet { ordinal, weekdays } => {
            let names = weekdays.iter().map(|day| short_weekday_name(*day)).collect::<Vec<_>>().join(", ");
            format!(" on the {} of {names}", ordinal.label())
        }
    }
}

fn yearly_detail(month: u32, pattern: &MonthPattern) -> String {
    let month = month_name(month);
    match pattern {
        MonthPattern::DayOfMonth(-1) => format!(" on the last day of {month}"),
        MonthPattern::DayOfMonth(day) => format!(" on {month} {day}"),
        MonthPattern::NthWeekday { ordinal, weekday } => format!(" on the {} {} of {month}", ordinal.label(), long_weekday_name(*weekday)),
        MonthPattern::NthWeekdayOfSet { ordinal, weekdays } if weekdays.as_slice() == WORKDAYS.as_slice() => {
            format!(" on the {} weekday of {month}", ordinal.label())
        }
        MonthPattern::NthWeekdayOfSet { ordinal, weekdays } => {
            let names = weekdays.iter().map(|day| short_weekday_name(*day)).collect::<Vec<_>>().join(", ");
            format!(" on the {} of {names} in {month}", ordinal.label())
        }
    }
}

fn month_name(month: u32) -> &'static str {
    MONTH_NAMES.get(month.saturating_sub(1) as usize).copied().unwrap_or("")
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

fn long_weekday_name(day: Weekday) -> &'static str {
    match day {
        Weekday::Mon => "Monday",
        Weekday::Tue => "Tuesday",
        Weekday::Wed => "Wednesday",
        Weekday::Thu => "Thursday",
        Weekday::Fri => "Friday",
        Weekday::Sat => "Saturday",
        Weekday::Sun => "Sunday",
    }
}

/// The result of opening the series popover: the button to place in the
/// editor's top action row, plus a getter for the current selection (read on
/// Save, rather than threading a live value through the `on_change`
/// callback's every intermediate state).
pub struct SeriesControl {
    pub button: gtk::MenuButton,
    current: Rc<RefCell<Option<RecurrenceRule>>>,
    /// The event's existing rule, kept verbatim so an untouched series isn't
    /// needlessly canonicalised (an Outlook writer may use a form this
    /// builder would rewrite, e.g. a bare `FREQ=MONTHLY`).
    initial_raw: Option<String>,
    /// Set once the user confirms a change with **Done**. Until then Save uses
    /// `initial_raw` unchanged.
    dirty: Rc<Cell<bool>>,
    /// `Some` when the initial rule couldn't be parsed by this builder (a
    /// "custom" recurrence) - kept so the preview can show the original string
    /// and Save can preserve it untouched.
    pub unrepresentable_raw: Option<String>,
}

impl SeriesControl {
    /// The rule currently selected in the popover, or `None` for "does not
    /// repeat". `None` while [`Self::unrepresentable_raw`] is set and the user
    /// hasn't actively picked a frequency.
    pub fn current(&self) -> Option<RecurrenceRule> {
        self.current.borrow().clone()
    }

    /// The RRULE to save: the original string when the user never confirmed a
    /// change, otherwise the builder's canonical form (`None` = no repeat).
    pub fn rrule(&self, all_day: bool) -> Option<String> {
        if !self.dirty.get() {
            return self.initial_raw.clone();
        }
        self.current().map(|rule| to_rrule_string(&rule, all_day))
    }

    /// The human summary the read-only preview shows: a custom rule keeps its
    /// original value until the user actually replaces it.
    pub fn summary(&self) -> String {
        if !self.dirty.get() {
            if let Some(raw) = &self.unrepresentable_raw {
                return raw.clone();
            }
        }
        self.current().map(|rule| describe(&rule)).unwrap_or_else(|| "Does not repeat".to_string())
    }
}

/// The seven weekdays in display order for the weekly toggles, starting at
/// the configured first day of the week (item 3.1).
fn weekday_order(week_start: Weekday) -> Vec<Weekday> {
    let mut day = week_start;
    (0..7)
        .map(|_| {
            let current = day;
            day = day.succ();
            current
        })
        .collect()
}

/// Builds the Series `MenuButton` + popover. `initial_raw` is the event's
/// existing raw `rrule` string, if any (`None` for a brand-new event);
/// `week_start` orders the weekly day toggles; `start_date` is the series
/// anchor, used to resolve RFC defaults and seed a fresh monthly/yearly rule.
pub fn build_series_control(initial_raw: Option<&str>, week_start: Weekday, start_date: NaiveDate) -> SeriesControl {
    let parsed = initial_raw.map(|raw| parse_rrule_string(raw, start_date));
    let unrepresentable_raw = matches!(parsed, Some(None)).then(|| initial_raw.unwrap_or_default().to_string());
    let initial = parsed.flatten();
    let initial_wkst = initial.as_ref().and_then(|rule| rule.wkst);

    let current = Rc::new(RefCell::new(initial.clone()));
    let dirty = Rc::new(Cell::new(false));
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
        .width_request(340)
        .build();

    let freq_model = gtk::StringList::new(&["Does not repeat", "Daily", "Weekly", "Monthly", "Yearly"]);
    let freq_dropdown = gtk::DropDown::builder().model(&freq_model).tooltip_text("Repeat").build();
    freq_dropdown.set_selected(match initial.as_ref().map(|rule| rule.frequency()) {
        None => 0,
        Some(Frequency::Daily) => 1,
        Some(Frequency::Weekly) => 2,
        Some(Frequency::Monthly) => 3,
        Some(Frequency::Yearly) => 4,
    });
    content.append(&freq_dropdown);

    let interval_row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).build();
    let every_label = gtk::Label::new(Some("Every"));
    crate::privacy::mark_ui(&every_label);
    interval_row.append(&every_label);
    let interval_spin = gtk::SpinButton::with_range(1.0, 999.0, 1.0);
    interval_spin.set_tooltip_text(Some("Interval"));
    interval_spin.set_value(f64::from(initial.as_ref().map(|rule| rule.interval).unwrap_or(1)));
    interval_row.append(&interval_spin);
    let interval_unit_label = gtk::Label::new(None);
    crate::privacy::mark_ui(&interval_unit_label);
    interval_row.append(&interval_unit_label);
    content.append(&interval_row);

    let weekday_days = weekday_order(week_start);
    let weekday_row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(4).build();
    let initial_days = match initial.as_ref().map(|rule| &rule.pattern) {
        Some(RecurrencePattern::Weekly { days }) => days.clone(),
        _ => vec![],
    };
    let weekday_toggles: Vec<(Weekday, gtk::ToggleButton)> = weekday_days
        .iter()
        .map(|day| {
            let toggle = gtk::ToggleButton::builder().label(short_weekday_name(*day)).build();
            toggle.set_active(initial_days.contains(day));
            weekday_row.append(&toggle);
            (*day, toggle)
        })
        .collect();
    content.append(&weekday_row);

    // The monthly/yearly "On" selector: a day number, an ordinal weekday, or
    // the first/last weekday (BYSETPOS).
    let pattern_row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).build();
    let mode_model = gtk::StringList::new(&MODE_LABELS);
    let mode_dropdown = gtk::DropDown::builder().model(&mode_model).tooltip_text("Repeat on").build();
    pattern_row.append(&mode_dropdown);

    let day_labels: Vec<String> = (1..=31).map(|day| day.to_string()).chain(std::iter::once("Last day".to_string())).collect();
    let day_label_refs: Vec<&str> = day_labels.iter().map(String::as_str).collect();
    let day_model = gtk::StringList::new(&day_label_refs);
    let day_dropdown = gtk::DropDown::builder().model(&day_model).tooltip_text("Day of month").build();
    pattern_row.append(&day_dropdown);

    let weekday_names: Vec<&str> = weekday_days.iter().map(|day| long_weekday_name(*day)).collect();
    let pattern_weekday_model = gtk::StringList::new(&weekday_names);
    let pattern_weekday_dropdown = gtk::DropDown::builder().model(&pattern_weekday_model).tooltip_text("Weekday").build();
    pattern_row.append(&pattern_weekday_dropdown);
    content.append(&pattern_row);

    let month_row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).build();
    let month_label = gtk::Label::new(Some("in"));
    crate::privacy::mark_ui(&month_label);
    month_row.append(&month_label);
    let month_model = gtk::StringList::new(&MONTH_NAMES);
    let month_dropdown = gtk::DropDown::builder().model(&month_model).tooltip_text("Month").build();
    month_row.append(&month_dropdown);
    content.append(&month_row);

    let initial_month = match initial.as_ref().map(|rule| &rule.pattern) {
        Some(RecurrencePattern::Yearly { month, .. }) => *month,
        _ => start_date.month(),
    };
    month_dropdown.set_selected(initial_month.saturating_sub(1));

    // Seed the "On" row from the parsed rule, or from the start date for a
    // fresh monthly/yearly rule.
    let (initial_mode, initial_day_index, initial_pattern_weekday) = initial_month_ui(initial.as_ref(), start_date, &weekday_days);
    mode_dropdown.set_selected(initial_mode);
    day_dropdown.set_selected(initial_day_index);
    pattern_weekday_dropdown.set_selected(initial_pattern_weekday);

    let end_model = gtk::StringList::new(&["Never", "After", "On date"]);
    let end_dropdown = gtk::DropDown::builder().model(&end_model).tooltip_text("End").build();
    end_dropdown.set_selected(match initial.as_ref().map(|rule| &rule.end) {
        None | Some(RecurrenceEnd::Never) => 0,
        Some(RecurrenceEnd::After(_)) => 1,
        Some(RecurrenceEnd::On(_)) => 2,
    });
    content.append(&end_dropdown);

    let end_count_spin = gtk::SpinButton::with_range(1.0, 999.0, 1.0);
    end_count_spin.set_tooltip_text(Some("Number of occurrences"));
    end_count_spin.set_value(f64::from(match initial.as_ref().map(|rule| &rule.end) {
        Some(RecurrenceEnd::After(n)) => *n,
        _ => 10,
    }));
    let end_date_calendar = gtk::Calendar::new();
    end_date_calendar.set_tooltip_text(Some("End date"));
    if let Some(RecurrenceEnd::On(date)) = initial.as_ref().map(|rule| &rule.end) {
        if let Ok(dt) = gtk::glib::DateTime::from_utc(date.year(), date.month() as i32, date.day() as i32, 0, 0, 0.0) {
            end_date_calendar.set_date(Some(&dt));
        }
    }
    content.append(&end_count_spin);
    content.append(&end_date_calendar);

    let preview_label = gtk::Label::builder()
        .wrap(true)
        .xalign(0.0)
        .halign(gtk::Align::Start)
        .css_classes(["dim-label", "caption"])
        .build();
    crate::privacy::mark_ui(&preview_label);
    content.append(&preview_label);

    let update_visibility: Rc<dyn Fn()> = {
        let weekday_row = weekday_row.clone();
        let interval_row = interval_row.clone();
        let interval_unit_label = interval_unit_label.clone();
        let interval_spin = interval_spin.clone();
        let end_dropdown = end_dropdown.clone();
        let end_count_spin = end_count_spin.clone();
        let end_date_calendar = end_date_calendar.clone();
        let freq_dropdown = freq_dropdown.clone();
        let pattern_row = pattern_row.clone();
        let mode_dropdown = mode_dropdown.clone();
        let day_dropdown = day_dropdown.clone();
        let pattern_weekday_dropdown = pattern_weekday_dropdown.clone();
        let month_row = month_row.clone();
        Rc::new(move || {
            let freq = freq_dropdown.selected();
            let repeats = freq != 0;
            interval_row.set_visible(repeats);
            weekday_row.set_visible(repeats && freq == 2);
            pattern_row.set_visible(repeats && (freq == 3 || freq == 4));
            month_row.set_visible(repeats && freq == 4);
            let mode = mode_dropdown.selected();
            day_dropdown.set_visible(repeats && (freq == 3 || freq == 4) && mode == 0);
            pattern_weekday_dropdown.set_visible(repeats && (freq == 3 || freq == 4) && (1..=5).contains(&mode));
            end_dropdown.set_visible(repeats);
            end_count_spin.set_visible(repeats && end_dropdown.selected() == 1);
            end_date_calendar.set_visible(repeats && end_dropdown.selected() == 2);
            interval_unit_label.set_text(interval_unit(freq, interval_spin.value() as u32));
        })
    };

    // The pending rule from the widgets, shared by the live preview and the
    // Done button so both read the same state.
    let draft: Rc<dyn Fn() -> Option<RecurrenceRule>> = {
        let freq_dropdown = freq_dropdown.clone();
        let interval_spin = interval_spin.clone();
        let weekday_toggles = weekday_toggles.clone();
        let mode_dropdown = mode_dropdown.clone();
        let day_dropdown = day_dropdown.clone();
        let pattern_weekday_dropdown = pattern_weekday_dropdown.clone();
        let pattern_weekday_days = weekday_days.clone();
        let month_dropdown = month_dropdown.clone();
        let end_dropdown = end_dropdown.clone();
        let end_count_spin = end_count_spin.clone();
        let end_date_calendar = end_date_calendar.clone();
        let wkst = initial_wkst;
        Rc::new(move || {
            let freq = match freq_dropdown.selected() {
                0 => return None,
                1 => Frequency::Daily,
                2 => Frequency::Weekly,
                3 => Frequency::Monthly,
                _ => Frequency::Yearly,
            };
            let weekday = pattern_weekday_days
                .get(pattern_weekday_dropdown.selected() as usize)
                .copied()
                .unwrap_or(start_date.weekday());
            let pattern = match freq {
                Frequency::Daily => RecurrencePattern::Daily,
                Frequency::Weekly => RecurrencePattern::Weekly {
                    days: sorted_days(weekday_toggles.iter().filter(|(_, toggle)| toggle.is_active()).map(|(day, _)| *day)),
                },
                Frequency::Monthly => RecurrencePattern::Monthly(draft_month_pattern(mode_dropdown.selected(), day_dropdown.selected(), weekday)),
                Frequency::Yearly => RecurrencePattern::Yearly {
                    month: month_dropdown.selected() + 1,
                    on: draft_month_pattern(mode_dropdown.selected(), day_dropdown.selected(), weekday),
                },
            };
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
                pattern,
                interval: interval_spin.value() as u32,
                wkst,
                end,
            })
        })
    };

    let refresh_preview: Rc<dyn Fn()> = {
        let preview_label = preview_label.clone();
        let draft = draft.clone();
        Rc::new(move || {
            let text = draft().map(|rule| describe(&rule)).unwrap_or_default();
            preview_label.set_text(&text);
            preview_label.set_visible(!text.is_empty());
        })
    };

    update_visibility();
    refresh_preview();
    {
        let update_visibility = update_visibility.clone();
        let refresh_preview = refresh_preview.clone();
        freq_dropdown.connect_selected_notify(move |_| {
            update_visibility();
            refresh_preview();
        });
    }
    {
        let update_visibility = update_visibility.clone();
        let refresh_preview = refresh_preview.clone();
        mode_dropdown.connect_selected_notify(move |_| {
            update_visibility();
            refresh_preview();
        });
    }
    {
        let update_visibility = update_visibility.clone();
        let refresh_preview = refresh_preview.clone();
        end_dropdown.connect_selected_notify(move |_| {
            update_visibility();
            refresh_preview();
        });
    }
    {
        let update_visibility = update_visibility.clone();
        let refresh_preview = refresh_preview.clone();
        interval_spin.connect_value_changed(move |_| {
            update_visibility();
            refresh_preview();
        });
    }
    {
        let refresh_preview = refresh_preview.clone();
        end_count_spin.connect_value_changed(move |_| refresh_preview());
    }
    for (_, toggle) in &weekday_toggles {
        let refresh_preview = refresh_preview.clone();
        toggle.connect_toggled(move |_| refresh_preview());
    }
    for dropdown in [&day_dropdown, &pattern_weekday_dropdown, &month_dropdown] {
        let refresh_preview = refresh_preview.clone();
        dropdown.connect_selected_notify(move |_| refresh_preview());
    }
    {
        let refresh_preview = refresh_preview.clone();
        end_date_calendar.connect_day_selected(move |_| refresh_preview());
    }

    let done_button = gtk::Button::with_label("Done");
    done_button.add_css_class("suggested-action");
    content.append(&done_button);
    popover.set_child(Some(&content));
    button.set_popover(Some(&popover));

    {
        let current = current.clone();
        let dirty = dirty.clone();
        let button = button.clone();
        let popover = popover.clone();
        let draft = draft.clone();
        let initial = initial.clone();
        done_button.connect_clicked(move |_| {
            let rule = draft();
            // An untouched rule stays byte-for-byte as the server wrote it;
            // only a real change marks the control dirty.
            if rule != initial {
                dirty.set(true);
            }
            *current.borrow_mut() = rule.clone();
            update_button_label(&button, rule.as_ref(), None);
            popover.popdown();
        });
    }

    SeriesControl {
        button,
        current,
        initial_raw: initial_raw.map(str::to_string),
        dirty,
        unrepresentable_raw,
    }
}

/// The initial "On" row state: `(mode index, day index, weekday index)`.
fn initial_month_ui(initial: Option<&RecurrenceRule>, start_date: NaiveDate, weekday_days: &[Weekday]) -> (u32, u32, u32) {
    let default_weekday = weekday_days.iter().position(|day| *day == start_date.weekday()).unwrap_or(0) as u32;
    let pattern = match initial.map(|rule| &rule.pattern) {
        Some(RecurrencePattern::Monthly(pattern)) => Some(pattern),
        Some(RecurrencePattern::Yearly { on, .. }) => Some(on),
        _ => None,
    };
    match pattern {
        Some(MonthPattern::DayOfMonth(day)) => (0, day_index(*day), default_weekday),
        Some(MonthPattern::NthWeekday { ordinal, weekday }) => (
            match ordinal {
                Ordinal::First => 1,
                Ordinal::Second => 2,
                Ordinal::Third => 3,
                Ordinal::Fourth => 4,
                Ordinal::Last => 5,
            },
            0,
            weekday_days.iter().position(|day| day == weekday).unwrap_or(0) as u32,
        ),
        Some(MonthPattern::NthWeekdayOfSet { ordinal, .. }) => (
            match ordinal {
                Ordinal::First => 6,
                _ => 7,
            },
            0,
            default_weekday,
        ),
        None => (0, day_index(start_date.day() as i32), default_weekday),
    }
}

/// The dropdown index for a day-of-month value: `1..=31` maps to `0..=30`,
/// `-1` ("last day") to the 32nd entry.
fn day_index(day: i32) -> u32 {
    if (1..=31).contains(&day) {
        day as u32 - 1
    } else {
        31
    }
}

fn draft_month_pattern(mode: u32, day_index: u32, weekday: Weekday) -> MonthPattern {
    match mode {
        0 => MonthPattern::DayOfMonth(if day_index >= 31 { -1 } else { day_index as i32 + 1 }),
        1..=5 => MonthPattern::NthWeekday {
            ordinal: match mode {
                1 => Ordinal::First,
                2 => Ordinal::Second,
                3 => Ordinal::Third,
                4 => Ordinal::Fourth,
                _ => Ordinal::Last,
            },
            weekday,
        },
        6 => MonthPattern::NthWeekdayOfSet {
            ordinal: Ordinal::First,
            weekdays: WORKDAYS.to_vec(),
        },
        _ => MonthPattern::NthWeekdayOfSet {
            ordinal: Ordinal::Last,
            weekdays: WORKDAYS.to_vec(),
        },
    }
}

fn interval_unit(freq: u32, value: u32) -> &'static str {
    let plural = value != 1;
    match (freq, plural) {
        (1, false) => "day",
        (1, true) => "days",
        (2, false) => "week",
        (2, true) => "weeks",
        (3, false) => "month",
        (3, true) => "months",
        (_, false) => "year",
        (_, true) => "years",
    }
}

fn update_button_label(button: &gtk::MenuButton, rule: Option<&RecurrenceRule>, unrepresentable_raw: Option<&str>) {
    let label = if unrepresentable_raw.is_some() {
        "Custom recurrence".to_string()
    } else {
        match rule {
            Some(rule) => describe(rule),
            None => "Does not repeat".to_string(),
        }
    };
    button.set_tooltip_text(Some(&label));
    button.update_property(&[gtk::accessible::Property::Label(&label)]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).unwrap()
    }

    #[test]
    fn weekly_toggles_follow_the_week_start() {
        assert_eq!(
            weekday_order(Weekday::Sun),
            vec![Weekday::Sun, Weekday::Mon, Weekday::Tue, Weekday::Wed, Weekday::Thu, Weekday::Fri, Weekday::Sat]
        );
        assert_eq!(weekday_order(Weekday::Mon)[0], Weekday::Mon);
        assert_eq!(weekday_order(Weekday::Sat)[6], Weekday::Fri);
    }

    #[test]
    fn round_trips_weekly_with_byday_count_and_wkst() {
        let raw = "FREQ=WEEKLY;INTERVAL=2;BYDAY=MO,WE;WKST=SU;COUNT=10";
        let rule = parse_rrule_string(raw, date(2026, 10, 15)).unwrap();
        assert_eq!(
            rule.pattern,
            RecurrencePattern::Weekly {
                days: vec![Weekday::Mon, Weekday::Wed]
            }
        );
        assert_eq!(rule.interval, 2);
        assert_eq!(rule.wkst, Some(Weekday::Sun));
        assert_eq!(rule.end, RecurrenceEnd::After(10));

        let reparsed = parse_rrule_string(&to_rrule_string(&rule, false), date(2026, 10, 15)).unwrap();
        assert_eq!(reparsed, rule);
    }

    #[test]
    fn parses_monthly_day_of_month() {
        let rule = parse_rrule_string("FREQ=MONTHLY;BYMONTHDAY=15", date(2026, 10, 1)).unwrap();
        assert_eq!(rule.pattern, RecurrencePattern::Monthly(MonthPattern::DayOfMonth(15)));
        assert_eq!(to_rrule_string(&rule, false), "FREQ=MONTHLY;BYMONTHDAY=15");
    }

    #[test]
    fn parses_monthly_last_day() {
        let rule = parse_rrule_string("FREQ=MONTHLY;BYMONTHDAY=-1", date(2026, 10, 1)).unwrap();
        assert_eq!(rule.pattern, RecurrencePattern::Monthly(MonthPattern::DayOfMonth(-1)));
        assert_eq!(describe(&rule), "Monthly on the last day");
    }

    #[test]
    fn parses_monthly_nth_weekday() {
        let rule = parse_rrule_string("FREQ=MONTHLY;BYDAY=2TU", date(2026, 10, 1)).unwrap();
        assert_eq!(
            rule.pattern,
            RecurrencePattern::Monthly(MonthPattern::NthWeekday {
                ordinal: Ordinal::Second,
                weekday: Weekday::Tue,
            })
        );
        assert_eq!(to_rrule_string(&rule, false), "FREQ=MONTHLY;BYDAY=2TU");
        assert_eq!(describe(&rule), "Monthly on the second Tuesday");
    }

    #[test]
    fn parses_monthly_bysetpos_single_day_as_nth_weekday() {
        let rule = parse_rrule_string("FREQ=MONTHLY;BYDAY=TU;BYSETPOS=2", date(2026, 10, 1)).unwrap();
        assert_eq!(
            rule.pattern,
            RecurrencePattern::Monthly(MonthPattern::NthWeekday {
                ordinal: Ordinal::Second,
                weekday: Weekday::Tue,
            })
        );
    }

    #[test]
    fn parses_monthly_last_weekday() {
        let raw = "FREQ=MONTHLY;BYDAY=MO,TU,WE,TH,FR;BYSETPOS=-1";
        let rule = parse_rrule_string(raw, date(2026, 10, 1)).unwrap();
        assert_eq!(
            rule.pattern,
            RecurrencePattern::Monthly(MonthPattern::NthWeekdayOfSet {
                ordinal: Ordinal::Last,
                weekdays: WORKDAYS.to_vec(),
            })
        );
        assert_eq!(to_rrule_string(&rule, false), raw);
        assert_eq!(describe(&rule), "Monthly on the last weekday");
    }

    #[test]
    fn monthly_without_by_month_day_uses_the_start_day() {
        let rule = parse_rrule_string("FREQ=MONTHLY", date(2026, 10, 15)).unwrap();
        assert_eq!(rule.pattern, RecurrencePattern::Monthly(MonthPattern::DayOfMonth(15)));
        assert_eq!(to_rrule_string(&rule, false), "FREQ=MONTHLY;BYMONTHDAY=15");
    }

    #[test]
    fn parses_yearly_by_month_and_day() {
        let rule = parse_rrule_string("FREQ=YEARLY;BYMONTH=12;BYMONTHDAY=25", date(2026, 1, 1)).unwrap();
        assert_eq!(
            rule.pattern,
            RecurrencePattern::Yearly {
                month: 12,
                on: MonthPattern::DayOfMonth(25),
            }
        );
        assert_eq!(to_rrule_string(&rule, false), "FREQ=YEARLY;BYMONTH=12;BYMONTHDAY=25");
        assert_eq!(describe(&rule), "Yearly on December 25");
    }

    #[test]
    fn parses_yearly_relative_weekday() {
        let rule = parse_rrule_string("FREQ=YEARLY;BYMONTH=11;BYDAY=4TH", date(2026, 1, 1)).unwrap();
        assert_eq!(
            rule.pattern,
            RecurrencePattern::Yearly {
                month: 11,
                on: MonthPattern::NthWeekday {
                    ordinal: Ordinal::Fourth,
                    weekday: Weekday::Thu,
                },
            }
        );
        assert_eq!(to_rrule_string(&rule, false), "FREQ=YEARLY;BYMONTH=11;BYDAY=4TH");
        assert_eq!(describe(&rule), "Yearly on the fourth Thursday of November");
    }

    #[test]
    fn yearly_without_by_parts_uses_the_start_date() {
        let rule = parse_rrule_string("FREQ=YEARLY", date(2026, 10, 15)).unwrap();
        assert_eq!(
            rule.pattern,
            RecurrencePattern::Yearly {
                month: 10,
                on: MonthPattern::DayOfMonth(15),
            }
        );
        assert_eq!(to_rrule_string(&rule, false), "FREQ=YEARLY;BYMONTH=10;BYMONTHDAY=15");
    }

    #[test]
    fn yearly_month_only_uses_the_start_day() {
        let rule = parse_rrule_string("FREQ=YEARLY;BYMONTH=2", date(2026, 10, 9)).unwrap();
        assert_eq!(
            rule.pattern,
            RecurrencePattern::Yearly {
                month: 2,
                on: MonthPattern::DayOfMonth(9),
            }
        );
    }

    #[test]
    fn all_day_until_is_a_date() {
        let mut rule = parse_rrule_string("FREQ=DAILY;UNTIL=20261231", date(2026, 10, 15)).unwrap();
        assert_eq!(to_rrule_string(&rule, true), "FREQ=DAILY;UNTIL=20261231");
        assert_eq!(to_rrule_string(&rule, false), "FREQ=DAILY;UNTIL=20261231T235959Z");
        rule.end = RecurrenceEnd::On(date(2026, 12, 31));
        assert_eq!(to_rrule_string(&rule, true), "FREQ=DAILY;UNTIL=20261231");
    }

    #[test]
    fn parse_returns_none_for_unsupported_by_rules() {
        // Several days-of-month, a bare monthly BYDAY ("every Monday"), an
        // arbitrary BYSETPOS weekday set, sub-daily frequencies, unknown parts.
        assert!(parse_rrule_string("FREQ=MONTHLY;BYMONTHDAY=15,-1", date(2026, 10, 1)).is_none());
        assert!(parse_rrule_string("FREQ=MONTHLY;BYDAY=MO", date(2026, 10, 1)).is_none());
        assert!(parse_rrule_string("FREQ=MONTHLY;BYDAY=MO,WE;BYSETPOS=-1", date(2026, 10, 1)).is_none());
        assert!(parse_rrule_string("FREQ=MONTHLY;BYDAY=MO,TU,WE,TH,FR;BYSETPOS=2", date(2026, 10, 1)).is_none());
        assert!(parse_rrule_string("FREQ=WEEKLY;BYDAY=2MO", date(2026, 10, 1)).is_none());
        assert!(parse_rrule_string("FREQ=HOURLY", date(2026, 10, 1)).is_none());
        assert!(parse_rrule_string("FREQ=DAILY;BYHOUR=9", date(2026, 10, 1)).is_none());
        assert!(parse_rrule_string("FREQ=YEARLY;BYMONTHDAY=1", date(2026, 10, 1)).is_none());
        assert!(parse_rrule_string("FREQ=MONTHLY;BYMONTH=10", date(2026, 10, 1)).is_none());
    }

    #[test]
    fn parse_returns_none_for_count_and_until_together() {
        assert!(parse_rrule_string("FREQ=DAILY;COUNT=5;UNTIL=20261231T000000Z", date(2026, 10, 1)).is_none());
    }

    #[test]
    fn describe_produces_readable_summaries() {
        let weekly = parse_rrule_string("FREQ=WEEKLY;BYDAY=MO,WE;COUNT=10", date(2026, 10, 1)).unwrap();
        assert_eq!(describe(&weekly), "Weekly on Mon, Wed, 10 times");

        let daily = parse_rrule_string("FREQ=DAILY", date(2026, 10, 1)).unwrap();
        assert_eq!(describe(&daily), "Daily");

        let quarterly = parse_rrule_string("FREQ=MONTHLY;INTERVAL=3", date(2026, 10, 1)).unwrap();
        assert_eq!(describe(&quarterly), "Every 3 months on day 1");

        let last_day = parse_rrule_string("FREQ=YEARLY;BYMONTH=2;BYMONTHDAY=-1", date(2026, 10, 1)).unwrap();
        assert_eq!(describe(&last_day), "Yearly on the last day of February");
    }

    #[test]
    fn parses_the_outlook_fixture() {
        const ICS: &str = include_str!("../../../test-fixtures/outlook/recurrence-rules.ics");
        let rules: Vec<&str> = ICS.lines().filter_map(|line| line.strip_prefix("RRULE:")).collect();
        assert_eq!(rules.len(), 6, "fixture should carry six rules");
        let parsed: Vec<RecurrenceRule> = rules.iter().map(|rule| parse_rrule_string(rule, date(2026, 10, 1)).expect("fixture rule parses")).collect();

        assert_eq!(parsed[0].pattern, RecurrencePattern::Monthly(MonthPattern::DayOfMonth(15)));
        assert_eq!(
            parsed[1].pattern,
            RecurrencePattern::Monthly(MonthPattern::NthWeekday {
                ordinal: Ordinal::Second,
                weekday: Weekday::Tue,
            })
        );
        assert_eq!(
            parsed[2].pattern,
            RecurrencePattern::Monthly(MonthPattern::NthWeekdayOfSet {
                ordinal: Ordinal::Last,
                weekdays: WORKDAYS.to_vec(),
            })
        );
        assert_eq!(
            parsed[3].pattern,
            RecurrencePattern::Yearly {
                month: 12,
                on: MonthPattern::DayOfMonth(25),
            }
        );
        assert_eq!(
            parsed[4].pattern,
            RecurrencePattern::Yearly {
                month: 11,
                on: MonthPattern::NthWeekday {
                    ordinal: Ordinal::Fourth,
                    weekday: Weekday::Thu,
                },
            }
        );
        assert_eq!(
            parsed[5].pattern,
            RecurrencePattern::Weekly {
                days: vec![Weekday::Mon, Weekday::Wed]
            }
        );
        assert_eq!(parsed[5].wkst, Some(Weekday::Sun));
    }

    #[test]
    fn interval_and_position_helpers_cover_the_edges() {
        assert_eq!(day_index(-1), 31);
        assert_eq!(day_index(1), 0);
        assert_eq!(draft_month_pattern(0, 31, Weekday::Mon), MonthPattern::DayOfMonth(-1));
        assert_eq!(draft_month_pattern(0, 14, Weekday::Mon), MonthPattern::DayOfMonth(15));
        assert_eq!(interval_unit(1, 1), "day");
        assert_eq!(interval_unit(1, 2), "days");
        assert_eq!(interval_unit(4, 1), "year");
        assert_eq!(interval_unit(4, 3), "years");
    }
}
