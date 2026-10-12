//! Snooze sync conventions (OUTLOOK_PARITY.md §4.2, M-11).
//!
//! IMAP has no snooze. New Outlook moves a snoozed item to a server
//! "Scheduled" folder and keeps the wake time in a MAPI property IMAP can't
//! see, so Lookout follows the same folder convention and carries the wake
//! time in an IMAP keyword instead: any Lookout install connected to the
//! account can then wake the message - moving it back to the Inbox, unread -
//! when the time comes. Outlook's own snooze can't interoperate with this
//! over IMAP (⛔ in §4.2).

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use chrono::{DateTime, Utc};

/// The server folder snoozed messages wait in - new Outlook's name for it.
pub const SCHEDULED_FOLDER_NAME: &str = "Scheduled";

/// IMAP keyword prefix carrying a snoozed message's wake time,
/// `$Lookout-snooze-<unix>` - unix seconds keep the atom legal, the same
/// convention as `$Lookout-flag-due-<unix>`.
pub const SNOOZE_KEYWORD_PREFIX: &str = "$Lookout-snooze-";

/// The keyword recording a snooze until `until`.
pub fn snooze_keyword(until: DateTime<Utc>) -> String {
    format!("{SNOOZE_KEYWORD_PREFIX}{}", until.timestamp())
}

/// The wake time in a `$Lookout-snooze-<unix>` keyword, or `None` if the
/// keyword isn't one or its timestamp is malformed or out of range.
pub fn snooze_until_from_keyword(keyword: &str) -> Option<DateTime<Utc>> {
    let seconds = keyword.strip_prefix(SNOOZE_KEYWORD_PREFIX)?.parse::<i64>().ok()?;
    DateTime::from_timestamp(seconds, 0)
}

/// A message's wake time from its keywords: the earliest snooze keyword, if
/// several were left behind by racing installs.
pub fn snooze_until_from_keywords<'a>(keywords: impl IntoIterator<Item = &'a String>) -> Option<DateTime<Utc>> {
    keywords.into_iter().filter_map(|keyword| snooze_until_from_keyword(keyword)).min()
}

/// Every snooze keyword among `keywords` - what waking a message removes.
pub fn snooze_keywords<'a>(keywords: impl IntoIterator<Item = &'a String>) -> Vec<String> {
    keywords.into_iter().filter(|keyword| keyword.starts_with(SNOOZE_KEYWORD_PREFIX)).cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snooze_keywords_round_trip_and_reject_junk() {
        let until = DateTime::from_timestamp(1_790_000_000, 0).unwrap();
        let keyword = snooze_keyword(until);
        assert_eq!(keyword, "$Lookout-snooze-1790000000");
        assert_eq!(snooze_until_from_keyword(&keyword), Some(until));
        assert_eq!(snooze_until_from_keyword("$Lookout-snooze-soon"), None);
        assert_eq!(snooze_until_from_keyword("$Lookout-snooze-99999999999999999999"), None);
        assert_eq!(snooze_until_from_keyword("$Lookout-flag-due-1790000000"), None);
    }

    #[test]
    fn the_earliest_snooze_keyword_wins() {
        let keywords: Vec<String> = vec!["$Lookout-tag-red".into(), "$Lookout-snooze-1790000500".into(), "$Lookout-snooze-1790000000".into()];
        assert_eq!(snooze_until_from_keywords(&keywords), DateTime::from_timestamp(1_790_000_000, 0));
        assert_eq!(snooze_keywords(&keywords).len(), 2);
        assert_eq!(snooze_until_from_keywords(&vec!["\\Seen".to_string()]), None);
    }
}
