//! Follow-up flag semantics (OUTLOOK_PARITY.md §4.1, §4.4, M-10).
//!
//! A follow-up flag is *not* Pin: Pin rides the IMAP `\Flagged` system flag
//! and Outlook's `PR_RENEW_TIME` MAPI property, whereas a follow-up flag is a
//! state plus optional start/due/completed dates. Graph models it as
//! `followupFlag { flagStatus, startDateTime, dueDateTime, completedDateTime }`
//! and Outlook's internet form is the classic `X-Message-Flag: Follow up` plus
//! RFC 2156 `Reply-By:`. On non-Microsoft servers Lookout stores the state in
//! IMAP keywords, whose names are defined here so feature code never inlines
//! them.
//!
//! Only the mapping lives in this module; the presets, list column, reading
//! pane bar and Tasks section live in the app (item 1.16).
//!
//! Keyword encoding (§4.2): a flagged message carries [`FLAG_KEYWORD`], plus
//! `$Lookout-flag-due-<unix>` when it has a due date; a completed one carries
//! only [`FLAG_DONE_KEYWORD`]. Due dates are date-only, stored as UTC midnight
//! of the date, so a time-zone change never moves them.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use chrono::{DateTime, NaiveDate, NaiveDateTime, SecondsFormat, Utc};

use crate::email::header_value;

/// The classic Outlook internet follow-up header.
pub const X_MESSAGE_FLAG_HEADER: &str = "X-Message-Flag";
/// RFC 2156's "reply by this date" header, which carries a follow-up due date
/// to non-Exchange recipients.
pub const REPLY_BY_HEADER: &str = "Reply-By";
/// IMAP keyword prefix carrying a flag's due time, `$Lookout-flag-due-<unix>`.
/// Unix seconds keep the atom legal (no spaces or reserved characters) and
/// match the snooze keyword convention (§4.2).
pub const FLAG_DUE_KEYWORD_PREFIX: &str = "$Lookout-flag-due-";
/// IMAP keyword marking a message flagged for follow-up. A fixed atom (unlike
/// the due keyword) so `UID SEARCH KEYWORD` can find every flagged message.
pub const FLAG_KEYWORD: &str = "$Lookout-flagged";
/// IMAP keyword marking a follow-up flag complete.
pub const FLAG_DONE_KEYWORD: &str = "$Lookout-flag-done";

/// A follow-up flag's state, matching Graph's `flagStatus` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum FlagStatus {
    #[default]
    NotFlagged,
    Flagged,
    Complete,
}

impl FlagStatus {
    /// Parses Graph's `flagStatus` (`notFlagged` / `flagged` / `complete`).
    pub fn from_graph(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "notflagged" => Some(FlagStatus::NotFlagged),
            "flagged" => Some(FlagStatus::Flagged),
            "complete" => Some(FlagStatus::Complete),
            _ => None,
        }
    }

    /// Graph's `flagStatus` value for writing.
    pub fn as_graph_str(self) -> &'static str {
        match self {
            FlagStatus::NotFlagged => "notFlagged",
            FlagStatus::Flagged => "flagged",
            FlagStatus::Complete => "complete",
        }
    }
}

/// A message's follow-up flag: state plus the optional dates Graph and Outlook
/// carry. `start`/`completed` come only from Graph; the internet headers can
/// express just a due date (`Reply-By`) and the flagged state.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FollowUpFlag {
    pub status: FlagStatus,
    pub start: Option<DateTime<Utc>>,
    pub due: Option<DateTime<Utc>>,
    pub completed: Option<DateTime<Utc>>,
}

/// Reads a follow-up flag from a message's `(name, value)` header list: the
/// presence of `X-Message-Flag` marks it flagged, and `Reply-By` supplies the
/// due date. Returns `None` when neither header is present.
pub fn follow_up_from_headers(headers: &[(String, String)]) -> Option<FollowUpFlag> {
    let flagged = header_value(headers, X_MESSAGE_FLAG_HEADER).map(str::trim).is_some_and(|value| !value.is_empty());
    let due = header_value(headers, REPLY_BY_HEADER).and_then(parse_reply_by);
    if !flagged && due.is_none() {
        return None;
    }
    Some(FollowUpFlag {
        status: FlagStatus::Flagged,
        start: None,
        due,
        completed: None,
    })
}

/// The internet headers to write for `flag`: `X-Message-Flag: Follow up` when
/// flagged, plus `Reply-By` when a due date is set. A complete or unset flag
/// emits nothing (its state travels as an IMAP keyword instead).
pub fn follow_up_headers(flag: &FollowUpFlag) -> Vec<(&'static str, String)> {
    let mut headers = Vec::new();
    if flag.status == FlagStatus::Flagged {
        headers.push((X_MESSAGE_FLAG_HEADER, "Follow up".to_string()));
    }
    if let Some(due) = flag.due {
        headers.push((REPLY_BY_HEADER, due.to_rfc3339_opts(SecondsFormat::Secs, true)));
    }
    headers
}

/// The `$Lookout-flag-due-<unix>` keyword for a due time.
pub fn flag_due_keyword(due: DateTime<Utc>) -> String {
    format!("{FLAG_DUE_KEYWORD_PREFIX}{}", due.timestamp())
}

/// The due time encoded in a `$Lookout-flag-due-<unix>` keyword, or `None` if
/// the keyword isn't one or its timestamp is malformed.
pub fn flag_due_from_keyword(keyword: &str) -> Option<DateTime<Utc>> {
    let seconds = keyword.strip_prefix(FLAG_DUE_KEYWORD_PREFIX)?.parse::<i64>().ok()?;
    DateTime::from_timestamp(seconds, 0)
}

/// A message's follow-up flag from its IMAP keywords, or `None` when it isn't
/// flagged. [`FLAG_DONE_KEYWORD`] wins over a stray [`FLAG_KEYWORD`] (a racing
/// install may leave both), and the earliest due keyword wins when several
/// survive. A due keyword without the flagged atom still reads as flagged.
pub fn follow_up_from_keywords<'a>(keywords: impl IntoIterator<Item = &'a String>) -> Option<FollowUpFlag> {
    let mut flagged = false;
    let mut done = false;
    let mut due: Option<DateTime<Utc>> = None;
    for keyword in keywords {
        if keyword == FLAG_KEYWORD {
            flagged = true;
        } else if keyword == FLAG_DONE_KEYWORD {
            done = true;
        } else if let Some(at) = flag_due_from_keyword(keyword) {
            flagged = true;
            due = Some(due.map_or(at, |current| current.min(at)));
        }
    }
    if done {
        return Some(FollowUpFlag {
            status: FlagStatus::Complete,
            ..Default::default()
        });
    }
    flagged.then_some(FollowUpFlag {
        status: FlagStatus::Flagged,
        start: None,
        due,
        completed: None,
    })
}

/// The IMAP keywords that encode `flag` (empty for [`FlagStatus::NotFlagged`]).
pub fn follow_up_keywords(flag: &FollowUpFlag) -> Vec<String> {
    match flag.status {
        FlagStatus::NotFlagged => Vec::new(),
        FlagStatus::Complete => vec![FLAG_DONE_KEYWORD.to_string()],
        FlagStatus::Flagged => {
            let mut keywords = vec![FLAG_KEYWORD.to_string()];
            keywords.extend(flag.due.map(flag_due_keyword));
            keywords
        }
    }
}

/// Every follow-up flag keyword among `keywords` - what changing or clearing a
/// flag removes before adding the new state's keywords.
pub fn flag_keywords<'a>(keywords: impl IntoIterator<Item = &'a String>) -> Vec<String> {
    keywords.into_iter().filter(|keyword| is_flag_keyword(keyword)).cloned().collect()
}

/// Whether `keyword` is one of the follow-up flag keywords.
pub fn is_flag_keyword(keyword: &str) -> bool {
    keyword == FLAG_KEYWORD || keyword == FLAG_DONE_KEYWORD || keyword.starts_with(FLAG_DUE_KEYWORD_PREFIX)
}

/// The due instant for a due *date*: UTC midnight of that date, matching how
/// all-day calendar boundaries are stored.
pub fn flag_due_for_date(date: NaiveDate) -> DateTime<Utc> {
    date.and_hms_opt(0, 0, 0).expect("midnight is valid").and_utc()
}

/// Parses an RFC 2156 `Reply-By` value: a full RFC 3339 date-time, a bare local
/// date-time (assumed UTC), or a bare date. Outlook emits the first form.
fn parse_reply_by(value: &str) -> Option<DateTime<Utc>> {
    let value = value.trim();
    if let Ok(parsed) = DateTime::parse_from_rfc3339(value) {
        return Some(parsed.with_timezone(&Utc));
    }
    if let Ok(parsed) = NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S") {
        return Some(parsed.and_utc());
    }
    NaiveDate::parse_from_str(value, "%Y-%m-%d").ok().map(|date| date.and_hms_opt(0, 0, 0).unwrap().and_utc())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn graph_status_round_trips() {
        for status in [FlagStatus::NotFlagged, FlagStatus::Flagged, FlagStatus::Complete] {
            assert_eq!(FlagStatus::from_graph(status.as_graph_str()), Some(status));
        }
        assert_eq!(FlagStatus::from_graph("flagStatus"), None);
    }

    #[test]
    fn x_message_flag_marks_the_message_flagged() {
        let flag = follow_up_from_headers(&headers(&[("X-Message-Flag", "Follow up")])).expect("flagged");
        assert_eq!(flag.status, FlagStatus::Flagged);
        assert_eq!(flag.due, None);
    }

    #[test]
    fn reply_by_supplies_the_due_date() {
        let flag = follow_up_from_headers(&headers(&[("Reply-By", "2026-08-10T09:00:00Z")])).expect("due date implies flagged");
        assert_eq!(flag.status, FlagStatus::Flagged);
        assert_eq!(flag.due, Some("2026-08-10T09:00:00Z".parse().unwrap()));
        // A server that merely added Reply-By still parses.
        assert!(follow_up_from_headers(&headers(&[("Reply-By", "2026-08-10")])).is_some());
    }

    #[test]
    fn absent_or_empty_flag_headers_are_none() {
        assert_eq!(follow_up_from_headers(&[]), None);
        assert_eq!(follow_up_from_headers(&headers(&[("X-Message-Flag", "  ")])), None);
    }

    #[test]
    fn emission_round_trips_through_the_reader() {
        let flag = FollowUpFlag {
            status: FlagStatus::Flagged,
            start: None,
            due: Some("2026-08-10T09:00:00Z".parse().unwrap()),
            completed: None,
        };
        let written: Vec<(String, String)> = follow_up_headers(&flag).into_iter().map(|(k, v)| (k.to_string(), v)).collect();
        let read = follow_up_from_headers(&written).expect("round trips");
        assert_eq!(read, flag);
    }

    #[test]
    fn complete_and_unset_flags_emit_no_internet_headers() {
        let complete = FollowUpFlag {
            status: FlagStatus::Complete,
            ..Default::default()
        };
        assert!(follow_up_headers(&complete).is_empty());
        assert!(follow_up_headers(&FollowUpFlag::default()).is_empty());
    }

    #[test]
    fn due_keyword_round_trips() {
        let due: DateTime<Utc> = "2026-08-10T09:00:00Z".parse().unwrap();
        let keyword = flag_due_keyword(due);
        assert_eq!(keyword, format!("{FLAG_DUE_KEYWORD_PREFIX}{}", due.timestamp()));
        assert_eq!(flag_due_from_keyword(&keyword), Some(due));
        assert_eq!(flag_due_from_keyword("$Lookout-tag-x"), None);
        assert_eq!(flag_due_from_keyword("$Lookout-flag-due-notanumber"), None);
    }

    fn set(keywords: &[&str]) -> Vec<String> {
        keywords.iter().map(|k| k.to_string()).collect()
    }

    #[test]
    fn keywords_round_trip_every_state() {
        let due = flag_due_for_date(NaiveDate::from_ymd_opt(2026, 10, 16).unwrap());
        for flag in [
            FollowUpFlag {
                status: FlagStatus::Flagged,
                ..Default::default()
            },
            FollowUpFlag {
                status: FlagStatus::Flagged,
                due: Some(due),
                ..Default::default()
            },
            FollowUpFlag {
                status: FlagStatus::Complete,
                ..Default::default()
            },
        ] {
            let keywords = follow_up_keywords(&flag);
            assert_eq!(follow_up_from_keywords(&keywords), Some(flag));
        }
        assert!(follow_up_keywords(&FollowUpFlag::default()).is_empty());
        assert_eq!(follow_up_from_keywords(&set(&["$Lookout-tag-work", "\\Seen"])), None);
    }

    #[test]
    fn done_wins_and_earliest_due_wins() {
        let flag = follow_up_from_keywords(&set(&[FLAG_KEYWORD, FLAG_DONE_KEYWORD, "$Lookout-flag-due-100"])).unwrap();
        assert_eq!(flag.status, FlagStatus::Complete);
        assert_eq!(flag.due, None);

        let flag = follow_up_from_keywords(&set(&[FLAG_KEYWORD, "$Lookout-flag-due-200", "$Lookout-flag-due-100"])).unwrap();
        assert_eq!(flag.due, DateTime::from_timestamp(100, 0));
        // A due keyword alone still reads as flagged; a malformed one is ignored.
        assert_eq!(follow_up_from_keywords(&set(&["$Lookout-flag-due-100"])).unwrap().status, FlagStatus::Flagged);
        assert_eq!(follow_up_from_keywords(&set(&["$Lookout-flag-due-x"])), None);
    }

    #[test]
    fn flag_keywords_collects_only_flag_atoms() {
        let keywords = set(&[FLAG_KEYWORD, "$Lookout-flag-due-5", FLAG_DONE_KEYWORD, "$Lookout-tag-x", "$Lookout-snooze-5"]);
        assert_eq!(flag_keywords(&keywords), set(&[FLAG_KEYWORD, "$Lookout-flag-due-5", FLAG_DONE_KEYWORD]));
    }

    #[test]
    fn due_dates_are_utc_midnight_so_the_date_survives_any_zone() {
        let date = NaiveDate::from_ymd_opt(2026, 10, 16).unwrap();
        let due = flag_due_for_date(date);
        assert_eq!(due.date_naive(), date);
        assert_eq!(flag_due_from_keyword(&flag_due_keyword(due)).unwrap().date_naive(), date);
    }
}
