//! Syncs follow-up flags with Outlook on Microsoft 365 accounts through
//! Microsoft Graph (item 1.16 / M-10, §4.4).
//!
//! Exchange's IMAP doesn't keep custom keywords, so on Microsoft 365 the mail
//! session stores a follow-up flag in the cache's local table and this module
//! carries it to and from Outlook's own `flag` property (`followupFlag`):
//! - **write**: every flag change is PATCHed onto the Graph message matched
//!   by `internetMessageId` (the same lookup `graph_pin` uses);
//! - **read**: a snapshot of every flagged (and recently completed) message
//!   is fetched on connect, on a timer and on F5, and handed to the session as
//!   `AccountCommand::ApplyRemoteFollowUps`, so flags set in Outlook show here.
//!
//! Both only run once the account has granted Graph access (Config's "Sync
//! with Outlook"), the same gate as pin mirroring.
//!
//! Due dates are date-only in Lookout (UTC midnight of the date). Outlook
//! shows a due date in the mailbox's zone, so the date is written as local
//! midnight in the calendar's display zone and read back the same way.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::sync::Arc;

use chrono::{DateTime, NaiveDateTime, Utc};
use lookout_core::text::percent_encode;
use lookout_core::{AccountId, FlagStatus, FollowUpFlag};
use lookout_graph::{GraphClient, GraphError};
use serde_json::{json, Value};

use crate::calendar_tz::DisplayZone;
use crate::microsoft_oauth::MicrosoftOAuth;

/// Most open flags read per snapshot (Outlook's own To Do list caps lower).
const MAX_FLAGGED: usize = 2000;
/// Most completed flags read per snapshot; older completions read as
/// unflagged, which shows the same in the list.
const MAX_COMPLETED: usize = 500;

/// One Microsoft 365 account's Graph follow-up flag client.
pub struct GraphMailClient {
    oauth: Arc<MicrosoftOAuth>,
    graph: GraphClient,
}

impl GraphMailClient {
    pub fn new(account_id: AccountId) -> Self {
        let oauth = Arc::new(MicrosoftOAuth::new(account_id));
        GraphMailClient {
            graph: GraphClient::new(oauth.clone()),
            oauth,
        }
    }

    /// Whether the account has granted Graph access (disk-only check).
    pub fn consented(&self) -> bool {
        self.oauth.graph_consented()
    }

    /// Writes `flag` onto the Graph message whose `internetMessageId` is
    /// `message_id` (angle brackets included, as in the envelope).
    pub async fn set_follow_up(&self, message_id: &str, flag: &FollowUpFlag, zone: DisplayZone) -> Result<(), GraphError> {
        let Some(graph_id) = self.resolve_message_id(message_id).await? else {
            return Err(GraphError::Status {
                status: 404,
                message: format!("no Graph message matches {message_id}"),
                request_id: None,
            });
        };
        self.graph.patch(&format!("me/messages/{graph_id}"), &json!({ "flag": flag_body(flag, zone) })).await?;
        Ok(())
    }

    /// Every flagged and recently completed message on the mailbox, as
    /// `(internetMessageId, flag)` pairs.
    pub async fn flagged_snapshot(&self, zone: DisplayZone) -> Result<Vec<(String, FollowUpFlag)>, GraphError> {
        let mut snapshot = Vec::new();
        for (status, max) in [("flagged", MAX_FLAGGED), ("complete", MAX_COMPLETED)] {
            let filter = percent_encode(&format!("flag/flagStatus eq '{status}'"));
            let items = self
                .graph
                .get_all(&format!("me/messages?$filter={filter}&$select=internetMessageId,flag&$top=100"), max)
                .await?;
            snapshot.extend(items.iter().filter_map(|item| {
                let message_id = item.get("internetMessageId")?.as_str()?.to_string();
                Some((message_id, flag_from_graph(item.get("flag")?, zone)?))
            }));
        }
        Ok(snapshot)
    }

    async fn resolve_message_id(&self, message_id: &str) -> Result<Option<String>, GraphError> {
        // OData string literals only need the quote doubled.
        let filter = percent_encode(&format!("internetMessageId eq '{}'", message_id.replace('\'', "''")));
        let page = self.graph.get(&format!("me/messages?$filter={filter}&$select=id")).await?;
        Ok(lookout_graph::page_items(&page)
            .first()
            .and_then(|item| item.get("id"))
            .and_then(Value::as_str)
            .map(str::to_string))
    }
}

/// Graph's `dateTimeTimeZone` for a Lookout due date: local midnight of the
/// date in `zone`, expressed in UTC.
fn graph_date(at: DateTime<Utc>, zone: DisplayZone) -> Value {
    let midnight = at.date_naive().and_hms_opt(0, 0, 0).expect("midnight is valid");
    let instant = zone.resolve_local(midnight);
    json!({ "dateTime": instant.format("%Y-%m-%dT%H:%M:%S").to_string(), "timeZone": "UTC" })
}

/// Reads a Graph `dateTimeTimeZone` back to a Lookout due date: the date the
/// instant falls on in `zone`, as UTC midnight. Graph answers in UTC unless
/// asked otherwise; any other zone is read as the wall date it names.
fn date_from_graph(value: &Value, zone: DisplayZone) -> Option<DateTime<Utc>> {
    let raw = value.get("dateTime")?.as_str()?;
    let naive = NaiveDateTime::parse_from_str(raw.split('.').next()?, "%Y-%m-%dT%H:%M:%S").ok()?;
    let in_utc = value.get("timeZone").and_then(Value::as_str).is_none_or(|tz| tz.eq_ignore_ascii_case("UTC"));
    let date = if in_utc { zone.to_local(naive.and_utc()).date() } else { naive.date() };
    Some(lookout_core::flag_due_for_date(date))
}

/// The Graph `flag` (`followupFlag`) body for `flag`. Outlook won't take a
/// due date without a start date, so a dated flag starts on its due date.
pub fn flag_body(flag: &FollowUpFlag, zone: DisplayZone) -> Value {
    let mut body = json!({ "flagStatus": flag.status.as_graph_str() });
    if flag.status == FlagStatus::Flagged {
        if let Some(due) = flag.due {
            body["dueDateTime"] = graph_date(due, zone);
            body["startDateTime"] = graph_date(flag.start.unwrap_or(due), zone);
        }
    }
    if flag.status == FlagStatus::Complete {
        body["completedDateTime"] = graph_date(flag.completed.unwrap_or_else(Utc::now), zone);
    }
    body
}

/// Parses a Graph `flag` object. `None` for a malformed one or `notFlagged`.
pub fn flag_from_graph(value: &Value, zone: DisplayZone) -> Option<FollowUpFlag> {
    let status = FlagStatus::from_graph(value.get("flagStatus")?.as_str()?)?;
    if status == FlagStatus::NotFlagged {
        return None;
    }
    let date = |key: &str| value.get(key).and_then(|v| date_from_graph(v, zone));
    Some(FollowUpFlag {
        status,
        start: None,
        due: (status == FlagStatus::Flagged).then(|| date("dueDateTime")).flatten(),
        completed: date("completedDateTime"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn sydney() -> DisplayZone {
        DisplayZone::from_setting(Some(chrono_tz::Australia::Sydney))
    }

    fn la() -> DisplayZone {
        DisplayZone::from_setting(Some(chrono_tz::America::Los_Angeles))
    }

    fn due(y: i32, m: u32, d: u32) -> DateTime<Utc> {
        lookout_core::flag_due_for_date(NaiveDate::from_ymd_opt(y, m, d).unwrap())
    }

    #[test]
    fn a_dated_flag_writes_local_midnight_with_a_start() {
        let flag = FollowUpFlag {
            status: FlagStatus::Flagged,
            due: Some(due(2026, 10, 16)),
            ..Default::default()
        };
        let body = flag_body(&flag, sydney());
        assert_eq!(body["flagStatus"], "flagged");
        // Midnight 16 Oct in Sydney (AEDT, UTC+11) is 13:00 UTC on the 15th.
        assert_eq!(body["dueDateTime"], json!({ "dateTime": "2026-10-15T13:00:00", "timeZone": "UTC" }));
        assert_eq!(body["startDateTime"], body["dueDateTime"]);
        assert_eq!(flag_body(&FollowUpFlag::default(), sydney()), json!({ "flagStatus": "notFlagged" }));
    }

    #[test]
    fn due_dates_round_trip_through_graph_in_any_zone() {
        for zone in [sydney(), la(), DisplayZone::from_setting(Some(chrono_tz::Pacific::Kiritimati))] {
            let flag = FollowUpFlag {
                status: FlagStatus::Flagged,
                due: Some(due(2026, 10, 16)),
                ..Default::default()
            };
            assert_eq!(flag_from_graph(&flag_body(&flag, zone), zone), Some(flag));
        }
    }

    #[test]
    fn reads_outlooks_flag_shapes() {
        let outlook = json!({
            "flagStatus": "flagged",
            "startDateTime": { "dateTime": "2026-10-13T07:00:00.0000000", "timeZone": "UTC" },
            "dueDateTime": { "dateTime": "2026-10-16T07:00:00.0000000", "timeZone": "UTC" }
        });
        let flag = flag_from_graph(&outlook, la()).unwrap();
        assert_eq!(flag.due, Some(due(2026, 10, 16)));
        // A flag with no dates, a completed one, and an unflagged one.
        assert_eq!(flag_from_graph(&json!({ "flagStatus": "flagged" }), la()).unwrap().due, None);
        assert_eq!(flag_from_graph(&json!({ "flagStatus": "complete" }), la()).unwrap().status, FlagStatus::Complete);
        assert_eq!(flag_from_graph(&json!({ "flagStatus": "notFlagged" }), la()), None);
        // A wall time in a named zone keeps its date.
        let named = json!({ "flagStatus": "flagged", "dueDateTime": { "dateTime": "2026-10-16T00:00:00", "timeZone": "Pacific Standard Time" } });
        assert_eq!(flag_from_graph(&named, sydney()).unwrap().due, Some(due(2026, 10, 16)));
    }
}
