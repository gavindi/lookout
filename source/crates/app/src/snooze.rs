//! Snooze presets (item 1.15 / M-11): the times the Snooze menu offers,
//! following new Outlook's list. The snooze itself - a move to the server's
//! "Scheduled" folder carrying the wake time, or a local-only fallback - is
//! the mail session's (`AccountCommand::SnoozeMessages`).

use chrono::{DateTime, Datelike, Days, NaiveDate, TimeZone, Timelike, Weekday};

/// One Snooze menu entry: its name and the wake time.
#[derive(Debug, Clone, PartialEq)]
pub struct SnoozePreset<Tz: TimeZone> {
    pub name: &'static str,
    pub at: DateTime<Tz>,
}

/// The hour of the day the day-based presets wake at (Outlook's 8:00 AM).
const MORNING_HOUR: u32 = 8;

/// "Later today" is offered while it still lands before this hour.
const LATER_TODAY_CUTOFF_HOUR: u32 = 22;

/// The Snooze menu's presets relative to `now`, as new Outlook offers them:
/// - "Later today": three hours on, rounded up to the hour - only while that
///   is still today and before 22:00;
/// - "Tomorrow": 08:00 tomorrow;
/// - "This weekend": Saturday 08:00, on weekdays;
/// - "Next week": next Monday 08:00.
///
/// An entry landing on the same time as an earlier one (Friday's "This
/// weekend" is tomorrow, Sunday's "Next week" too) is left out, and a wall
/// time a DST change skips resolves to its earliest valid instant.
pub fn snooze_presets<Tz: TimeZone>(now: &DateTime<Tz>) -> Vec<SnoozePreset<Tz>> {
    let tz = now.timezone();
    let today = now.date_naive();
    let at = |date: NaiveDate, hour: u32| date.and_hms_opt(hour, 0, 0).and_then(|naive| naive.and_local_timezone(tz.clone()).earliest());
    let mut presets: Vec<SnoozePreset<Tz>> = Vec::new();
    let mut push = |name: &'static str, at: Option<DateTime<Tz>>| {
        if let Some(at) = at {
            if !presets.iter().any(|p| p.at == at) {
                presets.push(SnoozePreset { name, at });
            }
        }
    };

    let later = now.naive_local() + chrono::Duration::hours(3);
    let rounded_hour = if later.minute() == 0 && later.second() == 0 { later.hour() } else { later.hour() + 1 };
    if later.date() == today && rounded_hour < LATER_TODAY_CUTOFF_HOUR {
        push("Later today", at(today, rounded_hour));
    }
    let tomorrow = today + Days::new(1);
    push("Tomorrow", at(tomorrow, MORNING_HOUR));
    let weekday = today.weekday();
    if !matches!(weekday, Weekday::Sat | Weekday::Sun) {
        let saturday = today + Days::new(u64::from(Weekday::Sat.num_days_from_monday() - weekday.num_days_from_monday()));
        push("This weekend", at(saturday, MORNING_HOUR));
    }
    let next_monday = today + Days::new(u64::from(7 - weekday.num_days_from_monday()));
    push("Next week", at(next_monday, MORNING_HOUR));
    presets
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<chrono::FixedOffset> {
        chrono::FixedOffset::east_opt(10 * 3600).unwrap().with_ymd_and_hms(y, m, d, h, min, 0).unwrap()
    }

    fn names<Tz: TimeZone>(presets: &[SnoozePreset<Tz>]) -> Vec<&'static str> {
        presets.iter().map(|p| p.name).collect()
    }

    /// Tuesday 13 Oct 2026, 10:20: every entry, Later today rounded up.
    #[test]
    fn weekday_morning_offers_every_preset() {
        let presets = snooze_presets(&local(2026, 10, 13, 10, 20));
        assert_eq!(
            presets.iter().map(|p| (p.name, p.at)).collect::<Vec<_>>(),
            vec![
                ("Later today", local(2026, 10, 13, 14, 0)),
                ("Tomorrow", local(2026, 10, 14, 8, 0)),
                ("This weekend", local(2026, 10, 17, 8, 0)),
                ("Next week", local(2026, 10, 19, 8, 0)),
            ]
        );
        // On the hour, three hours on is already whole.
        assert_eq!(snooze_presets(&local(2026, 10, 13, 10, 0))[0].at, local(2026, 10, 13, 13, 0));
    }

    /// Later today disappears once it would land at 22:00 or later.
    #[test]
    fn later_today_has_an_evening_cut_off() {
        assert_eq!(snooze_presets(&local(2026, 10, 13, 17, 30))[0].at, local(2026, 10, 13, 21, 0));
        assert_eq!(names(&snooze_presets(&local(2026, 10, 13, 18, 30)))[0], "Tomorrow");
        assert_eq!(names(&snooze_presets(&local(2026, 10, 13, 23, 0)))[0], "Tomorrow");
    }

    /// Friday's weekend is tomorrow and Sunday's next week is tomorrow: the
    /// duplicates go; at the weekend "This weekend" isn't offered at all.
    #[test]
    fn duplicates_and_weekend_entries_drop_out() {
        assert_eq!(names(&snooze_presets(&local(2026, 10, 16, 21, 0))), vec!["Tomorrow", "Next week"]); // Friday night
        assert_eq!(names(&snooze_presets(&local(2026, 10, 17, 9, 0))), vec!["Later today", "Tomorrow", "Next week"]); // Saturday
        let sunday = snooze_presets(&local(2026, 10, 18, 20, 0));
        assert_eq!(names(&sunday), vec!["Tomorrow"]);
        assert_eq!(sunday[0].at, local(2026, 10, 19, 8, 0));
    }

    /// Europe/Berlin falls back on Sunday 25 Oct 2026: presets still resolve.
    #[test]
    fn presets_survive_a_dst_change() {
        let now = chrono_tz::Europe::Berlin.with_ymd_and_hms(2026, 10, 24, 9, 0, 0).unwrap();
        let presets = snooze_presets(&now);
        assert_eq!(names(&presets), vec!["Later today", "Tomorrow", "Next week"]);
        assert_eq!(presets[1].at.naive_local(), NaiveDate::from_ymd_opt(2026, 10, 25).unwrap().and_hms_opt(8, 0, 0).unwrap());
    }
}
