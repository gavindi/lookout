//! Follow-up flag presets and wording (item 1.16 / M-10): the due dates the
//! Flag menu offers, following Outlook's list, and the labels the message
//! list, reading pane and Tasks view show for a flag. The flag itself lives on
//! the message (`AccountCommand::SetFollowUp`, keywords per §4.2).
//!
//! Due dates are date-only: a flag's `due` is UTC midnight of its date
//! (`lookout_core::flag_due_for_date`), read back with `date_naive()` and no
//! zone conversion, so a time-zone change never moves it.

use chrono::{Datelike, Days, NaiveDate, Weekday};
use lookout_core::{FlagStatus, FollowUpFlag};

/// One Flag menu entry: its name and due date (`None` = "No date").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlagPreset {
    pub name: &'static str,
    pub due: Option<NaiveDate>,
}

/// The first day of the week holding `date`.
fn week_start_of(date: NaiveDate, week_start: Weekday) -> NaiveDate {
    let offset = (7 + date.weekday().num_days_from_monday() - week_start.num_days_from_monday()) % 7;
    date - Days::new(u64::from(offset))
}

/// The last work day in the week starting `start`, or its last day when
/// `work_days` names none.
fn last_work_day(start: NaiveDate, work_days: &[Weekday]) -> NaiveDate {
    (0..7u64)
        .rev()
        .map(|offset| start + Days::new(offset))
        .find(|day| work_days.contains(&day.weekday()))
        .unwrap_or(start + Days::new(6))
}

/// The Flag menu's presets relative to `today`, as Outlook offers them:
/// - "Today" and "Tomorrow";
/// - "This week": the week's last work day - left out once that has passed
///   or is today/tomorrow (it would repeat an earlier entry);
/// - "Next week": next week's last work day;
/// - "No date".
///
/// Weeks follow the calendar's week start and work days (item 0.6).
pub fn flag_presets(today: NaiveDate, week_start: Weekday, work_days: &[Weekday]) -> Vec<FlagPreset> {
    let tomorrow = today + Days::new(1);
    let this_week = week_start_of(today, week_start);
    let this_week_due = last_work_day(this_week, work_days);
    let next_week_due = last_work_day(this_week + Days::new(7), work_days);
    let mut presets = vec![
        FlagPreset { name: "Today", due: Some(today) },
        FlagPreset {
            name: "Tomorrow",
            due: Some(tomorrow),
        },
    ];
    if this_week_due > tomorrow {
        presets.push(FlagPreset {
            name: "This week",
            due: Some(this_week_due),
        });
    }
    presets.push(FlagPreset {
        name: "Next week",
        due: Some(next_week_due),
    });
    presets.push(FlagPreset { name: "No date", due: None });
    presets
}

/// An open flag due on `due` (or with no date).
pub fn flagged(due: Option<NaiveDate>) -> FollowUpFlag {
    FollowUpFlag {
        status: FlagStatus::Flagged,
        due: due.map(lookout_core::flag_due_for_date),
        ..Default::default()
    }
}

/// A completed flag.
pub fn completed() -> FollowUpFlag {
    FollowUpFlag {
        status: FlagStatus::Complete,
        ..Default::default()
    }
}

/// No flag (what clearing writes).
pub fn cleared() -> FollowUpFlag {
    FollowUpFlag::default()
}

/// A flag's due date (the UTC date of its stored midnight).
pub fn due_date(flag: &FollowUpFlag) -> Option<NaiveDate> {
    flag.due.map(|due| due.date_naive())
}

/// Where a message sorts and groups by flag, in list order: open flags by
/// due date, then undated, then completed, then unflagged - Outlook's "Flag:
/// Due Date" arrangement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FlagBucket {
    Overdue,
    Today,
    Tomorrow,
    ThisWeek,
    NextWeek,
    Later,
    NoDate,
    Completed,
    Unflagged,
}

impl FlagBucket {
    pub fn label(self) -> &'static str {
        match self {
            FlagBucket::Overdue => "Overdue",
            FlagBucket::Today => "Today",
            FlagBucket::Tomorrow => "Tomorrow",
            FlagBucket::ThisWeek => "This week",
            FlagBucket::NextWeek => "Next week",
            FlagBucket::Later => "Later",
            FlagBucket::NoDate => "No date",
            FlagBucket::Completed => "Completed",
            FlagBucket::Unflagged => "Unflagged",
        }
    }
}

/// The bucket for `flag` as of `today`, with weeks starting on `week_start`.
pub fn bucket(flag: Option<&FollowUpFlag>, today: NaiveDate, week_start: Weekday) -> FlagBucket {
    let Some(flag) = flag else {
        return FlagBucket::Unflagged;
    };
    match flag.status {
        FlagStatus::NotFlagged => FlagBucket::Unflagged,
        FlagStatus::Complete => FlagBucket::Completed,
        FlagStatus::Flagged => {
            let Some(due) = due_date(flag) else {
                return FlagBucket::NoDate;
            };
            let next_week = week_start_of(today, week_start) + Days::new(7);
            if due < today {
                FlagBucket::Overdue
            } else if due == today {
                FlagBucket::Today
            } else if due == today + Days::new(1) {
                FlagBucket::Tomorrow
            } else if due < next_week {
                FlagBucket::ThisWeek
            } else if due < next_week + Days::new(7) {
                FlagBucket::NextWeek
            } else {
                FlagBucket::Later
            }
        }
    }
}

/// A short date: the weekday within the coming week ("Tue"), otherwise the
/// day and month ("14 Oct").
fn short_date(date: NaiveDate, today: NaiveDate) -> String {
    if date > today && date < today + Days::new(7) {
        date.format("%a").to_string()
    } else {
        date.format("%-d %b").to_string()
    }
}

/// The message list's due label for an open flag due on `due`: "Overdue",
/// "Due today", "Due tomorrow", "Due Tue" or "Due 14 Oct".
pub fn due_label(due: NaiveDate, today: NaiveDate) -> String {
    if due < today {
        "Overdue".to_string()
    } else if due == today {
        "Due today".to_string()
    } else if due == today + Days::new(1) {
        "Due tomorrow".to_string()
    } else {
        format!("Due {}", short_date(due, today))
    }
}

/// The reading pane's follow-up bar text (Outlook's "Follow up. Due by …").
pub fn banner_text(flag: &FollowUpFlag, today: NaiveDate) -> String {
    match (flag.status, due_date(flag)) {
        (FlagStatus::Complete, _) => match flag.completed {
            Some(at) => format!("Completed on {}", at.date_naive().format("%a %-d %b")),
            None => "Completed.".to_string(),
        },
        (_, Some(due)) if due < today => format!("Follow up. Overdue since {}", due.format("%a %-d %b")),
        (_, Some(due)) if due == today => "Follow up. Due today.".to_string(),
        (_, Some(due)) => format!("Follow up. Due by {}", due.format("%a %-d %b")),
        (_, None) => "Follow up.".to_string(),
    }
}

/// The Undo toast's text after setting `flag`.
pub fn action_label(flag: &FollowUpFlag, today: NaiveDate) -> String {
    match (flag.status, due_date(flag)) {
        (FlagStatus::NotFlagged, _) => "Flag cleared".to_string(),
        (FlagStatus::Complete, _) => "Marked complete".to_string(),
        (FlagStatus::Flagged, None) => "Flagged".to_string(),
        (FlagStatus::Flagged, Some(due)) if due == today => "Flagged: due today".to_string(),
        (FlagStatus::Flagged, Some(due)) if due == today + Days::new(1) => "Flagged: due tomorrow".to_string(),
        (FlagStatus::Flagged, Some(due)) => format!("Flagged: due {}", short_date(due, today)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WEEKDAYS: [Weekday; 5] = [Weekday::Mon, Weekday::Tue, Weekday::Wed, Weekday::Thu, Weekday::Fri];

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn dues(presets: &[FlagPreset]) -> Vec<(&'static str, Option<NaiveDate>)> {
        presets.iter().map(|p| (p.name, p.due)).collect()
    }

    #[test]
    fn midweek_offers_every_preset() {
        // Tuesday 13 Oct 2026; weeks start Monday.
        let presets = flag_presets(date(2026, 10, 13), Weekday::Mon, &WEEKDAYS);
        assert_eq!(
            dues(&presets),
            vec![
                ("Today", Some(date(2026, 10, 13))),
                ("Tomorrow", Some(date(2026, 10, 14))),
                ("This week", Some(date(2026, 10, 16))),
                ("Next week", Some(date(2026, 10, 23))),
                ("No date", None),
            ]
        );
    }

    #[test]
    fn this_week_is_dropped_once_it_would_repeat_or_has_passed() {
        // Thursday: Friday is tomorrow.
        let thursday = flag_presets(date(2026, 10, 15), Weekday::Mon, &WEEKDAYS);
        assert!(thursday.iter().all(|p| p.name != "This week"));
        // Saturday: the work week is over; next week is the coming one.
        let saturday = flag_presets(date(2026, 10, 17), Weekday::Mon, &WEEKDAYS);
        assert!(saturday.iter().all(|p| p.name != "This week"));
        assert_eq!(saturday.iter().find(|p| p.name == "Next week").unwrap().due, Some(date(2026, 10, 23)));
    }

    #[test]
    fn weeks_follow_the_calendar_week_start_and_work_days() {
        // Weeks starting Sunday: on Saturday the week is still the current
        // one, so "Next week" is the Friday after next.
        let saturday = flag_presets(date(2026, 10, 17), Weekday::Sun, &WEEKDAYS);
        assert_eq!(saturday.iter().find(|p| p.name == "Next week").unwrap().due, Some(date(2026, 10, 23)));
        let sunday = flag_presets(date(2026, 10, 18), Weekday::Sun, &WEEKDAYS);
        assert_eq!(sunday.iter().find(|p| p.name == "This week").unwrap().due, Some(date(2026, 10, 23)));
        assert_eq!(sunday.iter().find(|p| p.name == "Next week").unwrap().due, Some(date(2026, 10, 30)));
        // A Sunday-to-Thursday work week ends on Thursday.
        let sun_thu = [Weekday::Sun, Weekday::Mon, Weekday::Tue, Weekday::Wed, Weekday::Thu];
        let monday = flag_presets(date(2026, 10, 12), Weekday::Sun, &sun_thu);
        assert_eq!(monday.iter().find(|p| p.name == "This week").unwrap().due, Some(date(2026, 10, 15)));
    }

    #[test]
    fn buckets_order_flags_by_due_date() {
        let today = date(2026, 10, 13);
        let at = |d: NaiveDate| bucket(Some(&flagged(Some(d))), today, Weekday::Mon);
        assert_eq!(at(date(2026, 10, 12)), FlagBucket::Overdue);
        assert_eq!(at(today), FlagBucket::Today);
        assert_eq!(at(date(2026, 10, 14)), FlagBucket::Tomorrow);
        assert_eq!(at(date(2026, 10, 18)), FlagBucket::ThisWeek);
        assert_eq!(at(date(2026, 10, 19)), FlagBucket::NextWeek);
        assert_eq!(at(date(2026, 10, 26)), FlagBucket::Later);
        assert_eq!(bucket(Some(&flagged(None)), today, Weekday::Mon), FlagBucket::NoDate);
        assert_eq!(bucket(Some(&completed()), today, Weekday::Mon), FlagBucket::Completed);
        assert_eq!(bucket(None, today, Weekday::Mon), FlagBucket::Unflagged);
        assert!(FlagBucket::Overdue < FlagBucket::NoDate && FlagBucket::NoDate < FlagBucket::Completed);
    }

    #[test]
    fn labels_read_like_outlook() {
        let today = date(2026, 10, 13);
        assert_eq!(due_label(date(2026, 10, 12), today), "Overdue");
        assert_eq!(due_label(today, today), "Due today");
        assert_eq!(due_label(date(2026, 10, 14), today), "Due tomorrow");
        assert_eq!(due_label(date(2026, 10, 16), today), "Due Fri");
        assert_eq!(due_label(date(2026, 10, 23), today), "Due 23 Oct");
        assert_eq!(banner_text(&flagged(Some(date(2026, 10, 16))), today), "Follow up. Due by Fri 16 Oct");
        assert_eq!(banner_text(&flagged(None), today), "Follow up.");
        assert_eq!(action_label(&flagged(Some(date(2026, 10, 16))), today), "Flagged: due Fri");
        assert_eq!(action_label(&cleared(), today), "Flag cleared");
    }

    #[test]
    fn a_due_date_survives_the_round_trip_in_any_zone() {
        // Stored as UTC midnight and read back without a zone conversion.
        let due = date(2026, 10, 16);
        assert_eq!(due_date(&flagged(Some(due))), Some(due));
    }
}
