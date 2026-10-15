//! Microsoft Graph ⇄ `lookout-core` message-property mapping (OUTLOOK_PARITY.md
//! §4.4, item 5.4 / A-06, M-03, M-09, M-10, M-12).
//!
//! Graph's `/me/messages` speaks a JSON message vocabulary, and Lookout reads
//! mail over IMAP. This module is the one place the per-message Outlook
//! properties - **categories** (M-09), the **follow-up flag** (M-10),
//! **importance** (M-12) and **Focused / Other** classification (M-03) - are
//! translated, so neither the sync loop nor the feature code hard-codes Graph
//! property names (§10 "Interop strings"). Like `calendar`, `contacts` and
//! `todo`, it is deliberately pure - no HTTP, no [`GraphClient`] - so every
//! rule is unit-testable against inline JSON. The HTTP transport lives in
//! [`client`] (`MailClient`).
//!
//! [`GraphClient`]: crate::client::GraphClient
//!
//! Two Graph shapes back Focused Inbox (M-03):
//! - the per-message `inferenceClassification` (`focused` / `other`), read from
//!   the delta and writable with a `PATCH` (a single message); and
//! - the per-**sender** `inferenceClassificationOverride`
//!   (`/me/inferenceClassification/overrides`), which classifies future mail
//!   from that SMTP address and is what Outlook's "Always move…" sets.
//!
//! Importance and the follow-up flag are Graph *properties*, so their read side
//! rides the same delta and their write side the same `PATCH`.
//!
//! Due dates are date-only in Lookout (UTC midnight of the date). Outlook
//! shows a due date in the mailbox's zone, so a flag's date is written as local
//! midnight in the caller's display zone and read back the same way; `None`
//! reads the wall time as UTC (the app passes its resolved zone, so this is
//! only the fallback for an undetermined system zone).

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
pub mod client;

pub use client::MailClient;

use chrono::{DateTime, Duration, NaiveDateTime, Offset, TimeZone, Utc};
use lookout_core::{EmailAddress, FlagStatus, FollowUpFlag, Importance};
use serde_json::{json, Value};

/// Graph's per-message `inferenceClassification` and an override's `classifyAs`
/// (M-03). Focused is the default for a message with no explicit value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum InferenceClassification {
    #[default]
    Focused,
    Other,
}

impl InferenceClassification {
    /// Parses Graph's lowercase value; unknown values yield `None` so callers
    /// can fall back to the default.
    pub fn from_graph(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "focused" => Some(InferenceClassification::Focused),
            "other" => Some(InferenceClassification::Other),
            _ => None,
        }
    }

    /// Graph's value for writing.
    pub fn as_graph_str(self) -> &'static str {
        match self {
            InferenceClassification::Focused => "focused",
            InferenceClassification::Other => "other",
        }
    }
}

/// One message's Outlook properties as read from a delta page or a `GET`
/// (item 5.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageProperties {
    /// Graph's message id - the write target (`PATCH /me/messages/{id}`).
    pub graph_id: String,
    /// The RFC 822 `Message-ID` Graph reports (angle brackets included) - the
    /// key the IMAP cache joins on. `None` for a message Graph carries without
    /// one, which can't be mapped back to a cached summary.
    pub message_id: Option<String>,
    /// Category display names (matched to Lookout's master list by name).
    pub categories: Vec<String>,
    /// The follow-up flag, or `None` when the message isn't flagged.
    pub flag: Option<FollowUpFlag>,
    pub importance: Importance,
    pub classification: InferenceClassification,
}

/// One page of the messages delta, split into live records and tombstones
/// (item 5.4). `removals` are Graph ids of messages deleted or moved out.
#[derive(Debug, Clone, Default)]
pub struct MessageDelta {
    pub upserts: Vec<MessageProperties>,
    pub removals: Vec<String>,
    pub delta_link: Option<String>,
}

/// One Focused / Other sender override
/// (`/me/inferenceClassification/overrides`, M-03).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InferenceOverride {
    /// The override's Graph id - the delete target.
    pub id: String,
    pub classify_as: InferenceClassification,
    pub sender: EmailAddress,
}

/// The Graph id of a `/delta` tombstone (`{"id": "…", "@removed": {…}}`), or
/// `None` for a live record.
pub fn removed_id(value: &Value) -> Option<String> {
    value.get("@removed")?;
    value.get("id").and_then(Value::as_str).map(str::to_string)
}

/// The OData `$filter` matching one RFC 822 Message-ID. The only character an
/// OData string literal needs escaped is the single quote (doubled); the
/// angle brackets and `@` in a Message-ID are fine inside a quoted literal.
pub fn message_id_filter(message_id: &str) -> String {
    format!("internetMessageId eq '{}'", message_id.replace('\'', "''"))
}

/// Maps a Graph `message` to its synced properties, or `None` for a delta
/// tombstone or a record with no Graph id. `zone` interprets a flag's due date
/// (see the module notes).
pub fn message_properties_from_graph(value: &Value, zone: Option<chrono_tz::Tz>) -> Option<MessageProperties> {
    if value.get("@removed").is_some() {
        return None;
    }
    let graph_id = value.get("id").and_then(Value::as_str).filter(|id| !id.is_empty())?.to_string();
    Some(MessageProperties {
        graph_id,
        message_id: non_empty_str(value, "internetMessageId"),
        categories: string_array(value, "categories"),
        flag: value.get("flag").and_then(|flag| flag_from_graph(flag, zone)),
        importance: value.get("importance").and_then(Value::as_str).and_then(Importance::from_graph).unwrap_or_default(),
        classification: value
            .get("inferenceClassification")
            .and_then(Value::as_str)
            .and_then(InferenceClassification::from_graph)
            .unwrap_or_default(),
    })
}

/// The `PATCH` body for a message's categories (an empty list clears them).
pub fn categories_body(categories: &[String]) -> Value {
    json!({ "categories": categories })
}

/// The `PATCH` body for a message's importance.
pub fn importance_body(importance: Importance) -> Value {
    json!({ "importance": importance.as_graph_str() })
}

/// The `PATCH` body for a message's Focused / Other classification (M-03).
pub fn classification_body(classification: InferenceClassification) -> Value {
    json!({ "inferenceClassification": classification.as_graph_str() })
}

/// The Graph `flag` (`followupFlag`) body for `flag` (M-10). Outlook won't take
/// a due date without a start date, so a dated flag starts on its due date.
pub fn flag_body(flag: &FollowUpFlag, zone: Option<chrono_tz::Tz>) -> Value {
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

/// Parses a Graph `flag` object, or `None` for a malformed one or `notFlagged`.
pub fn flag_from_graph(value: &Value, zone: Option<chrono_tz::Tz>) -> Option<FollowUpFlag> {
    let status = FlagStatus::from_graph(value.get("flagStatus")?.as_str()?)?;
    if status == FlagStatus::NotFlagged {
        return None;
    }
    let date = |key: &str| value.get(key).and_then(|value| date_from_graph(value, zone));
    Some(FollowUpFlag {
        status,
        start: None,
        due: (status == FlagStatus::Flagged).then(|| date("dueDateTime")).flatten(),
        completed: date("completedDateTime"),
    })
}

/// The Graph request body for a sender override (M-03).
pub fn override_body(classify_as: InferenceClassification, sender: &EmailAddress) -> Value {
    let mut address = json!({ "address": sender.address });
    if let Some(name) = sender.name.as_deref().filter(|name| !name.trim().is_empty()) {
        address["name"] = json!(name.trim());
    }
    json!({ "classifyAs": classify_as.as_graph_str(), "senderEmailAddress": address })
}

/// Maps a Graph `inferenceClassificationOverride`, or `None` when it lacks an
/// id, a classification or a sender address.
pub fn override_from_graph(value: &Value) -> Option<InferenceOverride> {
    let id = value.get("id").and_then(Value::as_str).filter(|id| !id.is_empty())?.to_string();
    let classify_as = value.get("classifyAs").and_then(Value::as_str).and_then(InferenceClassification::from_graph)?;
    let sender = value.get("senderEmailAddress")?;
    let address = sender.get("address").and_then(Value::as_str).filter(|address| !address.trim().is_empty())?.to_string();
    Some(InferenceOverride {
        id,
        classify_as,
        sender: EmailAddress {
            name: non_empty_str(sender, "name"),
            address,
        },
    })
}

/// Graph's `dateTimeTimeZone` for a Lookout due date: local midnight of the
/// date in `zone`, expressed in UTC (matching the flag sync's existing form).
fn graph_date(at: DateTime<Utc>, zone: Option<chrono_tz::Tz>) -> Value {
    let midnight = at.date_naive().and_hms_opt(0, 0, 0).expect("midnight is valid");
    let instant = resolve_local(midnight, zone);
    json!({ "dateTime": instant.naive_utc().format("%Y-%m-%dT%H:%M:%S").to_string(), "timeZone": "UTC" })
}

/// Reads a Graph `dateTimeTimeZone` back to a Lookout due date: the date the
/// instant falls on in `zone`, as UTC midnight. Graph answers in UTC unless
/// asked otherwise; any other zone is read as the wall date it names.
fn date_from_graph(value: &Value, zone: Option<chrono_tz::Tz>) -> Option<DateTime<Utc>> {
    let raw = value.get("dateTime")?.as_str()?;
    let naive = NaiveDateTime::parse_from_str(raw.split('.').next()?, "%Y-%m-%dT%H:%M:%S").ok()?;
    let in_utc = value.get("timeZone").and_then(Value::as_str).is_none_or(|tz| tz.eq_ignore_ascii_case("UTC"));
    let date = if in_utc { to_local(naive.and_utc(), zone).date() } else { naive.date() };
    Some(lookout_core::flag_due_for_date(date))
}

/// `at` as wall-clock time in `zone` (`None` = UTC).
fn to_local(at: DateTime<Utc>, zone: Option<chrono_tz::Tz>) -> NaiveDateTime {
    match zone {
        Some(tz) => at.with_timezone(&tz).naive_local(),
        None => at.naive_utc(),
    }
}

/// Resolves a wall time in `zone` to an instant without panicking: an ambiguous
/// time (a DST fall-back) takes the earlier offset, and a time in a gap (a
/// spring-forward) is read with the offset in effect just before it. `None`
/// reads the wall time as UTC.
fn resolve_local(naive: NaiveDateTime, zone: Option<chrono_tz::Tz>) -> DateTime<Utc> {
    let Some(tz) = zone else {
        return naive.and_utc();
    };
    match tz.from_local_datetime(&naive) {
        chrono::LocalResult::Single(dt) => dt.with_timezone(&Utc),
        chrono::LocalResult::Ambiguous(earlier, _) => earlier.with_timezone(&Utc),
        chrono::LocalResult::None => {
            let before = tz
                .offset_from_local_datetime(&(naive - Duration::hours(6)))
                .earliest()
                .map(|offset| offset.fix().local_minus_utc())
                .unwrap_or(0);
            (naive - Duration::seconds(i64::from(before))).and_utc()
        }
    }
}

fn non_empty_str(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).filter(|text| !text.is_empty()).map(str::to_string)
}

fn string_array(value: &Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn sydney() -> Option<chrono_tz::Tz> {
        Some(chrono_tz::Australia::Sydney)
    }

    fn due(y: i32, m: u32, d: u32) -> DateTime<Utc> {
        lookout_core::flag_due_for_date(NaiveDate::from_ymd_opt(y, m, d).unwrap())
    }

    #[test]
    fn classification_round_trips() {
        for classification in [InferenceClassification::Focused, InferenceClassification::Other] {
            assert_eq!(InferenceClassification::from_graph(classification.as_graph_str()), Some(classification));
        }
        assert_eq!(InferenceClassification::from_graph("FOCUSED"), Some(InferenceClassification::Focused));
        assert_eq!(InferenceClassification::from_graph("nonsense"), None);
        assert_eq!(InferenceClassification::default(), InferenceClassification::Focused);
    }

    #[test]
    fn message_id_filter_escapes_the_quote_only() {
        assert_eq!(message_id_filter("<a@b.example>"), "internetMessageId eq '<a@b.example>'");
        assert_eq!(message_id_filter("a'b"), "internetMessageId eq 'a''b'");
    }

    #[test]
    fn a_full_message_maps_every_property() {
        let value = json!({
            "id": "AAMkAD-1",
            "internetMessageId": "<abc@contoso.com>",
            "categories": ["Work", "Projects"],
            "importance": "high",
            "inferenceClassification": "other",
            "flag": {
                "flagStatus": "flagged",
                "startDateTime": { "dateTime": "2026-10-13T07:00:00.0000000", "timeZone": "UTC" },
                "dueDateTime": { "dateTime": "2026-10-16T07:00:00.0000000", "timeZone": "UTC" }
            }
        });
        let props = message_properties_from_graph(&value, None).unwrap();
        assert_eq!(props.graph_id, "AAMkAD-1");
        assert_eq!(props.message_id.as_deref(), Some("<abc@contoso.com>"));
        assert_eq!(props.categories, vec!["Work".to_string(), "Projects".to_string()]);
        assert_eq!(props.importance, Importance::High);
        assert_eq!(props.classification, InferenceClassification::Other);
        assert_eq!(props.flag.unwrap().due, Some(due(2026, 10, 16)));
    }

    #[test]
    fn defaults_apply_when_properties_are_absent() {
        let props = message_properties_from_graph(&json!({ "id": "m1" }), None).unwrap();
        assert_eq!(props.message_id, None);
        assert!(props.categories.is_empty());
        assert_eq!(props.flag, None);
        assert_eq!(props.importance, Importance::Normal);
        assert_eq!(props.classification, InferenceClassification::Focused);
    }

    #[test]
    fn tombstones_and_id_less_records_are_skipped() {
        let removed = json!({ "id": "gone", "@removed": { "reason": "deleted" } });
        assert_eq!(removed_id(&removed).as_deref(), Some("gone"));
        assert!(message_properties_from_graph(&removed, None).is_none());
        assert!(removed_id(&json!({ "id": "live" })).is_none());
        assert!(message_properties_from_graph(&json!({ "internetMessageId": "no id" }), None).is_none());
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
        // A complete flag carries only its completion date.
        let complete = FollowUpFlag {
            status: FlagStatus::Complete,
            ..Default::default()
        };
        assert!(flag_body(&complete, None)["completedDateTime"].is_object());
    }

    #[test]
    fn due_dates_round_trip_through_graph_in_any_zone() {
        for zone in [sydney(), Some(chrono_tz::America::Los_Angeles), Some(chrono_tz::Pacific::Kiritimati), None] {
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
        let flag = flag_from_graph(&outlook, Some(chrono_tz::America::Los_Angeles)).unwrap();
        assert_eq!(flag.due, Some(due(2026, 10, 16)));
        // A flag with no dates, a completed one, and an unflagged one.
        assert_eq!(flag_from_graph(&json!({ "flagStatus": "flagged" }), None).unwrap().due, None);
        assert_eq!(flag_from_graph(&json!({ "flagStatus": "complete" }), None).unwrap().status, FlagStatus::Complete);
        assert_eq!(flag_from_graph(&json!({ "flagStatus": "notFlagged" }), None), None);
        // A wall time in a named zone keeps its date.
        let named = json!({ "flagStatus": "flagged", "dueDateTime": { "dateTime": "2026-10-16T00:00:00", "timeZone": "Pacific Standard Time" } });
        assert_eq!(flag_from_graph(&named, sydney()).unwrap().due, Some(due(2026, 10, 16)));
    }

    #[test]
    fn override_body_and_parse_round_trip() {
        let sender = EmailAddress {
            name: Some("Samantha Booth".to_string()),
            address: "samanthab@contoso.com".to_string(),
        };
        let body = override_body(InferenceClassification::Focused, &sender);
        assert_eq!(body["classifyAs"], "focused");
        assert_eq!(body["senderEmailAddress"]["name"], "Samantha Booth");
        assert_eq!(body["senderEmailAddress"]["address"], "samanthab@contoso.com");

        let parsed = override_from_graph(&json!({
            "id": "98f5bdef",
            "classifyAs": "focused",
            "senderEmailAddress": { "name": "Samantha Booth", "address": "samanthab@contoso.com" }
        }))
        .unwrap();
        assert_eq!(parsed.id, "98f5bdef");
        assert_eq!(parsed.classify_as, InferenceClassification::Focused);
        assert_eq!(parsed.sender.address, "samanthab@contoso.com");
        // A nameless sender is fine; a missing address is not.
        assert!(override_from_graph(&json!({ "id": "x", "classifyAs": "other", "senderEmailAddress": { "address": "a@b" } })).is_some());
        assert!(override_from_graph(&json!({ "id": "x", "classifyAs": "other" })).is_none());
        assert!(override_from_graph(&json!({ "classifyAs": "other", "senderEmailAddress": { "address": "a@b" } })).is_none());
    }
}
