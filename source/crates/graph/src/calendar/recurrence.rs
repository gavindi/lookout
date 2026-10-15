//! Recurrence conversion between the model's raw RFC 5545 `RRULE`
//! (`CalendarEvent::rrule`) and Microsoft Graph's structured `recurrence`
//! object (OUTLOOK_PARITY.md §5.1, item 9 / A-03).
//!
//! Graph's recurrence model is a strict subset of RFC 5545: it knows daily,
//! weekly, absolute/relative monthly and absolute/relative yearly patterns,
//! each with an interval, a day/month selection and one of three range kinds.
//! A rule outside that subset (e.g. `FREQ=MONTHLY;BYDAY=MO` - "every Monday
//! of the month" - or `BYSETPOS`) returns `None` so the caller degrades
//! (leaves the rule alone) rather than misrepresenting it.
//!
//! `UNTIL` is carried as a date: Graph's `endDate` is inclusive and
//! date-granular, so a `T…Z` `UNTIL`'s date part is used, and a rule written
//! back out ends at `T235959Z` on that date.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use chrono::{Datelike, NaiveDate};
use serde_json::{json, Value};

/// Converts a raw RRULE value to a Graph `recurrence` object, or `None` when
/// it can't be expressed. `start_date` is the series anchor (the event's
/// start in its own zone); `time_zone` becomes `range.recurrenceTimeZone`
/// (`"UTC"` when absent).
pub fn rrule_to_graph(rrule: &str, start_date: NaiveDate, time_zone: Option<&str>) -> Option<Value> {
    let parsed = parse_rrule(rrule)?;
    let pattern = pattern_to_graph(&parsed, start_date)?;
    let mut range = json!({ "startDate": start_date.format("%Y-%m-%d").to_string() });
    match (parsed.count, parsed.until) {
        (Some(count), _) => {
            range["type"] = json!("numbered");
            range["numberOfOccurrences"] = json!(count);
        }
        (None, Some(until)) => {
            range["type"] = json!("endDate");
            range["endDate"] = json!(until.format("%Y-%m-%d").to_string());
        }
        (None, None) => range["type"] = json!("noEnd"),
    }
    range["recurrenceTimeZone"] = json!(time_zone.filter(|z| !z.trim().is_empty()).unwrap_or("UTC"));
    Some(json!({ "pattern": pattern, "range": range }))
}

/// Converts a Graph `recurrence` object to a raw RRULE value, or `None` when
/// it is malformed/unrecognised.
pub fn recurrence_from_graph(value: &Value) -> Option<String> {
    let pattern = value.get("pattern")?;
    let range = value.get("range")?;
    let kind = pattern.get("type")?.as_str()?;
    let freq = match kind {
        "daily" => "DAILY",
        "weekly" => "WEEKLY",
        "absoluteMonthly" | "relativeMonthly" => "MONTHLY",
        "absoluteYearly" | "relativeYearly" => "YEARLY",
        _ => return None,
    };
    let mut parts = vec![format!("FREQ={freq}")];
    let interval = pattern.get("interval").and_then(Value::as_u64).unwrap_or(1);
    if interval > 1 {
        parts.push(format!("INTERVAL={interval}"));
    }

    match kind {
        "daily" => {}
        "weekly" => {
            if let Some(days) = graph_days(pattern.get("daysOfWeek")) {
                if !days.is_empty() {
                    parts.push(format!("BYDAY={days}"));
                }
            }
            if let Some(first) = pattern.get("firstDayOfWeek").and_then(Value::as_str).and_then(Weekday::from_graph) {
                parts.push(format!("WKST={}", first.code()));
            }
        }
        "absoluteMonthly" => {
            let day = pattern.get("dayOfMonth").and_then(Value::as_i64)?;
            parts.push(format!("BYMONTHDAY={day}"));
        }
        "relativeMonthly" => {
            let index = pattern.get("index").and_then(Value::as_str).and_then(ord_from_index)?;
            let day = single_graph_day(pattern.get("daysOfWeek"))?;
            parts.push(format!("BYDAY={index}{}", day.code()));
        }
        "absoluteYearly" => {
            let month = pattern.get("month").and_then(Value::as_u64)?;
            let day = pattern.get("dayOfMonth").and_then(Value::as_i64)?;
            parts.push(format!("BYMONTH={month}"));
            parts.push(format!("BYMONTHDAY={day}"));
        }
        "relativeYearly" => {
            let month = pattern.get("month").and_then(Value::as_u64)?;
            let index = pattern.get("index").and_then(Value::as_str).and_then(ord_from_index)?;
            let day = single_graph_day(pattern.get("daysOfWeek"))?;
            parts.push(format!("BYMONTH={month}"));
            parts.push(format!("BYDAY={index}{}", day.code()));
        }
        _ => return None,
    }

    match range.get("type").and_then(Value::as_str)? {
        "numbered" => {
            let count = range.get("numberOfOccurrences").and_then(Value::as_u64)?;
            parts.push(format!("COUNT={count}"));
        }
        "endDate" => {
            let end = parse_date(range.get("endDate")?.as_str()?)?;
            parts.push(format!("UNTIL={}T235959Z", end.format("%Y%m%d")));
        }
        "noEnd" => {}
        _ => return None,
    }
    Some(parts.join(";"))
}

/// Maps a parsed rule's pattern to Graph's `pattern` object, or `None` when
/// the combination has no Graph equivalent.
fn pattern_to_graph(parsed: &Parsed, start_date: NaiveDate) -> Option<Value> {
    match parsed.freq {
        Freq::Daily => {
            if !parsed.byday.is_empty() || !parsed.bymonthday.is_empty() || !parsed.bymonth.is_empty() {
                return None;
            }
            Some(json!({ "type": "daily", "interval": parsed.interval }))
        }
        Freq::Weekly => {
            // A weekly rule's days are plain weekdays (no ordinal).
            if parsed.byday.iter().any(|(ordinal, _)| ordinal.is_some()) {
                return None;
            }
            let mut pattern = json!({ "type": "weekly", "interval": parsed.interval });
            let days: Vec<&str> = parsed.byday.iter().map(|(_, day)| day.graph()).collect();
            if !days.is_empty() {
                pattern["daysOfWeek"] = json!(days);
            }
            if let Some(first) = parsed.wkst {
                pattern["firstDayOfWeek"] = json!(first.graph());
            }
            Some(pattern)
        }
        Freq::Monthly => {
            if let Some((ordinal, day)) = single_byday(parsed) {
                let index = index_from_ord(ordinal?)?;
                Some(json!({ "type": "relativeMonthly", "interval": parsed.interval, "daysOfWeek": [day.graph()], "index": index }))
            } else if parsed.byday.is_empty() && parsed.bymonthday.len() <= 1 {
                // No BY* selects the anchor day-of-month (RFC 5545 default).
                let day = parsed.bymonthday.first().copied().unwrap_or_else(|| start_date.day() as i32);
                Some(json!({ "type": "absoluteMonthly", "interval": parsed.interval, "dayOfMonth": day }))
            } else {
                None
            }
        }
        Freq::Yearly => {
            let month = match parsed.bymonth.as_slice() {
                [month] => *month,
                _ => return None,
            };
            if let Some((ordinal, day)) = single_byday(parsed) {
                let index = index_from_ord(ordinal?)?;
                Some(json!({ "type": "relativeYearly", "interval": parsed.interval, "month": month, "daysOfWeek": [day.graph()], "index": index }))
            } else if parsed.byday.is_empty() && parsed.bymonthday.len() == 1 {
                Some(json!({ "type": "absoluteYearly", "interval": parsed.interval, "month": month, "dayOfMonth": parsed.bymonthday[0] }))
            } else {
                None
            }
        }
    }
}

/// The single BYDAY entry, when there is exactly one.
fn single_byday(parsed: &Parsed) -> Option<(Option<i8>, Weekday)> {
    match parsed.byday.as_slice() {
        [entry] => Some(*entry),
        _ => None,
    }
}

/// The single `daysOfWeek` entry of a Graph pattern, when there is exactly one.
fn single_graph_day(days: Option<&Value>) -> Option<Weekday> {
    match days.and_then(Value::as_array)?.as_slice() {
        [value] => value.as_str().and_then(Weekday::from_graph),
        _ => None,
    }
}

/// The RRULE `BYDAY` list for a Graph `daysOfWeek` array (plain weekdays),
/// comma-joined; `None` when a value is unrecognised.
fn graph_days(days: Option<&Value>) -> Option<String> {
    let items = days.and_then(Value::as_array)?;
    let codes: Option<Vec<&str>> = items.iter().map(|value| value.as_str().and_then(Weekday::from_graph).map(Weekday::code)).collect();
    let codes = codes?;
    if codes.is_empty() {
        return None;
    }
    Some(codes.join(","))
}

/// A parsed RRULE subset (RFC 5545 §3.3.10).
#[derive(Debug, PartialEq)]
struct Parsed {
    freq: Freq,
    interval: u32,
    count: Option<u32>,
    until: Option<NaiveDate>,
    byday: Vec<(Option<i8>, Weekday)>,
    bymonthday: Vec<i32>,
    bymonth: Vec<u32>,
    wkst: Option<Weekday>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Freq {
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

/// Parses the supported RRULE subset; `None` on a missing/unknown `FREQ` or a
/// part key the conversion doesn't model (`BYSETPOS`, `BYHOUR`, …), so an
/// unrepresentable rule degrades rather than being silently simplified.
fn parse_rrule(rrule: &str) -> Option<Parsed> {
    let mut freq = None;
    let mut interval = 1u32;
    let mut count = None;
    let mut until = None;
    let mut byday = Vec::new();
    let mut bymonthday = Vec::new();
    let mut bymonth = Vec::new();
    let mut wkst = None;

    for part in rrule.trim().split(';').filter(|part| !part.is_empty()) {
        let (key, value) = part.split_once('=')?;
        match key.trim().to_ascii_uppercase().as_str() {
            "FREQ" => {
                freq = Some(match value.trim().to_ascii_uppercase().as_str() {
                    "DAILY" => Freq::Daily,
                    "WEEKLY" => Freq::Weekly,
                    "MONTHLY" => Freq::Monthly,
                    "YEARLY" => Freq::Yearly,
                    _ => return None,
                });
            }
            "INTERVAL" => interval = value.trim().parse().ok()?,
            "COUNT" => count = Some(value.trim().parse().ok()?),
            "UNTIL" => until = parse_date(value.trim()),
            "BYDAY" => {
                for token in value.split(',') {
                    byday.push(parse_byday(token.trim())?);
                }
            }
            "BYMONTHDAY" => {
                for token in value.split(',') {
                    bymonthday.push(token.trim().parse().ok()?);
                }
            }
            "BYMONTH" => {
                for token in value.split(',') {
                    bymonth.push(token.trim().parse().ok()?);
                }
            }
            "WKST" => wkst = Some(Weekday::from_code(value.trim())?),
            // Any other part would change the recurrence in a way the Graph
            // model can't express; refuse rather than drop it.
            _ => return None,
        }
    }
    Some(Parsed {
        freq: freq?,
        interval: interval.max(1),
        count,
        until,
        byday,
        bymonthday,
        bymonth,
        wkst,
    })
}

/// Parses a date from an RRULE `UNTIL` (`YYYYMMDD` or `YYYYMMDDT…Z`) or a
/// Graph `endDate`/`startDate` (`YYYY-MM-DD`).
fn parse_date(value: &str) -> Option<NaiveDate> {
    let value = value.trim();
    if let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        return Some(date);
    }
    let digits: String = value.chars().take_while(char::is_ascii_digit).collect();
    if digits.len() < 8 {
        return None;
    }
    NaiveDate::parse_from_str(&digits[..8], "%Y%m%d").ok()
}

/// A `BYDAY` token (`MO`, `2TU`, `-1FR`, `+1MO`).
fn parse_byday(token: &str) -> Option<(Option<i8>, Weekday)> {
    if token.len() < 2 {
        return None;
    }
    let (ordinal, code) = token.split_at(token.len() - 2);
    let weekday = Weekday::from_code(code)?;
    let ordinal = if ordinal.is_empty() { None } else { Some(ordinal.parse().ok()?) };
    Some((ordinal, weekday))
}

fn index_from_ord(ord: i8) -> Option<&'static str> {
    match ord {
        1 => Some("first"),
        2 => Some("second"),
        3 => Some("third"),
        4 => Some("fourth"),
        -1 => Some("last"),
        _ => None,
    }
}

fn ord_from_index(index: &str) -> Option<&'static str> {
    match index {
        "first" => Some("1"),
        "second" => Some("2"),
        "third" => Some("3"),
        "fourth" => Some("4"),
        "last" => Some("-1"),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Weekday {
    Mo,
    Tu,
    We,
    Th,
    Fr,
    Sa,
    Su,
}

impl Weekday {
    fn from_code(code: &str) -> Option<Self> {
        match code.trim().to_ascii_uppercase().as_str() {
            "MO" => Some(Weekday::Mo),
            "TU" => Some(Weekday::Tu),
            "WE" => Some(Weekday::We),
            "TH" => Some(Weekday::Th),
            "FR" => Some(Weekday::Fr),
            "SA" => Some(Weekday::Sa),
            "SU" => Some(Weekday::Su),
            _ => None,
        }
    }

    fn code(self) -> &'static str {
        match self {
            Weekday::Mo => "MO",
            Weekday::Tu => "TU",
            Weekday::We => "WE",
            Weekday::Th => "TH",
            Weekday::Fr => "FR",
            Weekday::Sa => "SA",
            Weekday::Su => "SU",
        }
    }

    fn from_graph(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "monday" => Some(Weekday::Mo),
            "tuesday" => Some(Weekday::Tu),
            "wednesday" => Some(Weekday::We),
            "thursday" => Some(Weekday::Th),
            "friday" => Some(Weekday::Fr),
            "saturday" => Some(Weekday::Sa),
            "sunday" => Some(Weekday::Su),
            _ => None,
        }
    }

    fn graph(self) -> &'static str {
        match self {
            Weekday::Mo => "monday",
            Weekday::Tu => "tuesday",
            Weekday::We => "wednesday",
            Weekday::Th => "thursday",
            Weekday::Fr => "friday",
            Weekday::Sa => "saturday",
            Weekday::Su => "sunday",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    #[test]
    fn weekly_with_days_and_interval_maps_to_graph() {
        let graph = rrule_to_graph("FREQ=WEEKLY;INTERVAL=2;BYDAY=MO,WE", date(2026, 10, 15), Some("Europe/Berlin")).unwrap();
        assert_eq!(graph["pattern"]["type"], "weekly");
        assert_eq!(graph["pattern"]["interval"], 2);
        assert_eq!(graph["pattern"]["daysOfWeek"], json!(["monday", "wednesday"]));
        assert_eq!(graph["range"]["type"], "noEnd");
        assert_eq!(graph["range"]["startDate"], "2026-10-15");
        assert_eq!(graph["range"]["recurrenceTimeZone"], "Europe/Berlin");
    }

    #[test]
    fn graph_weekly_maps_back_to_rrule() {
        let graph = json!({
            "pattern": { "type": "weekly", "interval": 2, "daysOfWeek": ["monday", "wednesday"], "firstDayOfWeek": "sunday" },
            "range": { "type": "endDate", "startDate": "2026-10-15", "endDate": "2027-10-15", "recurrenceTimeZone": "Europe/Berlin" }
        });
        assert_eq!(
            recurrence_from_graph(&graph).as_deref(),
            Some("FREQ=WEEKLY;INTERVAL=2;BYDAY=MO,WE;WKST=SU;UNTIL=20271015T235959Z")
        );
    }

    #[test]
    fn count_range_becomes_numbered() {
        let graph = rrule_to_graph("FREQ=DAILY;COUNT=10", date(2026, 10, 15), None).unwrap();
        assert_eq!(graph["pattern"]["type"], "daily");
        assert_eq!(graph["range"]["type"], "numbered");
        assert_eq!(graph["range"]["numberOfOccurrences"], 10);
        assert_eq!(graph["range"]["recurrenceTimeZone"], "UTC");
    }

    #[test]
    fn until_range_becomes_end_date() {
        let graph = rrule_to_graph("FREQ=DAILY;UNTIL=20271015T000000Z", date(2026, 10, 15), None).unwrap();
        assert_eq!(graph["range"]["type"], "endDate");
        assert_eq!(graph["range"]["endDate"], "2027-10-15");
    }

    #[test]
    fn absolute_monthly_round_trips() {
        let graph = rrule_to_graph("FREQ=MONTHLY;BYMONTHDAY=15", date(2026, 10, 15), None).unwrap();
        assert_eq!(graph["pattern"]["type"], "absoluteMonthly");
        assert_eq!(graph["pattern"]["dayOfMonth"], 15);
        assert_eq!(recurrence_from_graph(&graph).as_deref(), Some("FREQ=MONTHLY;BYMONTHDAY=15"));
    }

    #[test]
    fn monthly_without_byday_uses_the_anchor_day() {
        let graph = rrule_to_graph("FREQ=MONTHLY", date(2026, 10, 15), None).unwrap();
        assert_eq!(graph["pattern"]["dayOfMonth"], 15);
    }

    #[test]
    fn relative_monthly_round_trips() {
        let graph = rrule_to_graph("FREQ=MONTHLY;BYDAY=2TU", date(2026, 10, 15), None).unwrap();
        assert_eq!(graph["pattern"]["type"], "relativeMonthly");
        assert_eq!(graph["pattern"]["index"], "second");
        assert_eq!(graph["pattern"]["daysOfWeek"], json!(["tuesday"]));
        assert_eq!(recurrence_from_graph(&graph).as_deref(), Some("FREQ=MONTHLY;BYDAY=2TU"));

        let last = rrule_to_graph("FREQ=MONTHLY;BYDAY=-1FR", date(2026, 10, 15), None).unwrap();
        assert_eq!(last["pattern"]["index"], "last");
        assert_eq!(recurrence_from_graph(&last).as_deref(), Some("FREQ=MONTHLY;BYDAY=-1FR"));
    }

    #[test]
    fn absolute_yearly_round_trips() {
        let graph = rrule_to_graph("FREQ=YEARLY;BYMONTH=12;BYMONTHDAY=25", date(2026, 12, 25), None).unwrap();
        assert_eq!(graph["pattern"]["type"], "absoluteYearly");
        assert_eq!(graph["pattern"]["month"], 12);
        assert_eq!(graph["pattern"]["dayOfMonth"], 25);
        assert_eq!(recurrence_from_graph(&graph).as_deref(), Some("FREQ=YEARLY;BYMONTH=12;BYMONTHDAY=25"));
    }

    #[test]
    fn relative_yearly_round_trips() {
        let graph = rrule_to_graph("FREQ=YEARLY;BYMONTH=11;BYDAY=4TH", date(2026, 11, 26), None).unwrap();
        assert_eq!(graph["pattern"]["type"], "relativeYearly");
        assert_eq!(graph["pattern"]["month"], 11);
        assert_eq!(graph["pattern"]["index"], "fourth");
        assert_eq!(recurrence_from_graph(&graph).as_deref(), Some("FREQ=YEARLY;BYMONTH=11;BYDAY=4TH"));
    }

    #[test]
    fn numbered_range_maps_back() {
        let graph = json!({
            "pattern": { "type": "daily", "interval": 3 },
            "range": { "type": "numbered", "startDate": "2026-10-15", "numberOfOccurrences": 5 }
        });
        assert_eq!(recurrence_from_graph(&graph).as_deref(), Some("FREQ=DAILY;INTERVAL=3;COUNT=5"));
    }

    #[test]
    fn unrepresentable_rules_return_none() {
        // Every Monday of the month has no Graph index.
        assert!(rrule_to_graph("FREQ=MONTHLY;BYDAY=MO", date(2026, 10, 15), None).is_none());
        // Multiple bymonthday, BYSETPOS, an unknown FREQ.
        assert!(rrule_to_graph("FREQ=MONTHLY;BYMONTHDAY=1,15", date(2026, 10, 15), None).is_none());
        assert!(rrule_to_graph("FREQ=MONTHLY;BYDAY=MO;BYSETPOS=-1", date(2026, 10, 15), None).is_none());
        assert!(rrule_to_graph("FREQ=HOURLY", date(2026, 10, 15), None).is_none());
        assert!(rrule_to_graph("INTERVAL=2", date(2026, 10, 15), None).is_none());
    }

    #[test]
    fn a_malformed_graph_recurrence_returns_none() {
        assert!(recurrence_from_graph(&json!({ "pattern": { "type": "nonsense" }, "range": {} })).is_none());
        assert!(recurrence_from_graph(&json!({ "pattern": { "type": "weekly" } })).is_none());
    }
}
