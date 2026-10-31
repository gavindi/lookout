//! Reads and writes a Microsoft 365 mailbox's automatic replies
//! (Out-of-Office) through Graph (item 2.11 / M-24; §4.4).
//!
//! Microsoft 365 keeps the setting in `/me/mailboxSettings` →
//! `automaticRepliesSetting` (`{ status, scheduledStartDateTime,
//! scheduledEndDateTime, internalReplyMessage, externalReplyMessage,
//! externalAudience }`), and Graph is the only way to see or change it for an
//! M365 account - IMAP exposes nothing and ManageSieve needs a password
//! sign-in. The client follows the pin/flag/category pattern: built on the
//! account's [`GraphAccount`], gated by `consented()` (Config's "Sync with
//! Outlook"), mapping in pure functions covered by a captured fixture.
//!
//! Graph documents the two reply messages as HTML; Lookout's editors are plain
//! text, so reads strip the wrapper's markup back to text and writes wrap the
//! text with `<br>` line breaks. Schedule instants are written as UTC
//! `dateTimeTimeZone`s (the same unambiguous form `graph_mail` uses for flag
//! dates); reads honour whatever zone Graph names, resolving Windows names
//! through `lookout_core::windows_to_iana`.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use lookout_core::{windows_to_iana, AutomaticReplies, AutomaticReplyStatus, ExternalAudience};
use lookout_graph::GraphError;
use serde_json::{json, Value};

use crate::graph_account::{GraphAccount, GraphFeatureClient};

/// One Microsoft 365 account's automatic-replies client.
pub struct GraphAutomaticRepliesClient {
    account: GraphAccount,
}

impl GraphAutomaticRepliesClient {
    pub fn new(account: GraphAccount) -> Self {
        GraphAutomaticRepliesClient { account }
    }

    /// Runs the interactive Graph consent flow (Config's "Sync with Outlook"
    /// path) for an account that hasn't opted in yet, so the Automatic replies
    /// dialog can enable the feature without a trip through Config.
    pub async fn ensure_consent(&self) -> Result<(), String> {
        self.account.ensure_outlook_consent().await
    }

    /// The mailbox's current automatic replies.
    pub async fn get(&self) -> Result<AutomaticReplies, GraphError> {
        let value = self.account.graph().get("me/mailboxSettings").await?;
        Ok(automatic_replies_from_graph(&value))
    }

    /// Writes `settings` to the mailbox. Outlook sends while Lookout is
    /// closed, so this is the whole point of the Graph backend.
    pub async fn set(&self, settings: &AutomaticReplies) -> Result<(), GraphError> {
        self.account.graph().patch("me/mailboxSettings", &automatic_replies_body(settings)).await?;
        Ok(())
    }
}

impl GraphFeatureClient for GraphAutomaticRepliesClient {
    fn account(&self) -> &GraphAccount {
        &self.account
    }
}

/// The Graph `mailboxSettings` PATCH body for `settings`. A schedule is only
/// sent for `Scheduled`; every other status explicitly nulls the two bounds,
/// so switching a mailbox off can never leave a stale range behind.
pub fn automatic_replies_body(settings: &AutomaticReplies) -> Value {
    let mut replies = json!({
        "status": settings.status.as_graph_str(),
        "externalAudience": settings.external_audience.as_graph_str(),
        "internalReplyMessage": text_to_html(&settings.internal_message),
        "externalReplyMessage": text_to_html(&settings.external_message),
        "scheduledStartDateTime": Value::Null,
        "scheduledEndDateTime": Value::Null,
    });
    if settings.status == AutomaticReplyStatus::Scheduled {
        if let (Some(start), Some(end)) = (settings.start, settings.end) {
            replies["scheduledStartDateTime"] = graph_instant(start);
            replies["scheduledEndDateTime"] = graph_instant(end);
        }
    }
    json!({ "automaticRepliesSetting": replies })
}

/// Parses the relevant part of a `mailboxSettings` response. A missing
/// `automaticRepliesSetting` (or an unknown status) means disabled.
pub fn automatic_replies_from_graph(value: &Value) -> AutomaticReplies {
    let Some(replies) = value.get("automaticRepliesSetting") else {
        return AutomaticReplies::default();
    };
    AutomaticReplies {
        status: replies.get("status").and_then(Value::as_str).map(AutomaticReplyStatus::from_graph_str).unwrap_or_default(),
        start: replies.get("scheduledStartDateTime").and_then(instant_from_graph),
        end: replies.get("scheduledEndDateTime").and_then(instant_from_graph),
        internal_message: replies.get("internalReplyMessage").and_then(Value::as_str).map(html_to_text).unwrap_or_default(),
        external_message: replies.get("externalReplyMessage").and_then(Value::as_str).map(html_to_text).unwrap_or_default(),
        external_audience: replies
            .get("externalAudience")
            .and_then(Value::as_str)
            .map(ExternalAudience::from_graph_str)
            .unwrap_or_default(),
    }
}

/// Graph's `dateTimeTimeZone` for an instant: the UTC wall time with an
/// explicit UTC zone, so no Windows/IANA ambiguity is sent either way.
fn graph_instant(at: DateTime<Utc>) -> Value {
    json!({ "dateTime": at.naive_utc().format("%Y-%m-%dT%H:%M:%S").to_string(), "timeZone": "UTC" })
}

/// Reads a Graph `dateTimeTimeZone` back to an instant. Graph may answer in
/// UTC (what Lookout writes) or in the mailbox's zone, named either Windows
/// (`W. Europe Standard Time`) or IANA; an unrecognised zone falls back to
/// reading the wall time as UTC.
fn instant_from_graph(value: &Value) -> Option<DateTime<Utc>> {
    let raw = value.get("dateTime")?.as_str()?;
    let naive = NaiveDateTime::parse_from_str(raw.split('.').next()?, "%Y-%m-%dT%H:%M:%S").ok()?;
    let zone = value.get("timeZone").and_then(Value::as_str).unwrap_or("UTC");
    if zone.eq_ignore_ascii_case("UTC") {
        return Some(naive.and_utc());
    }
    let iana = windows_to_iana(zone).unwrap_or(zone);
    match iana.parse::<chrono_tz::Tz>() {
        Ok(tz) => Some(
            tz.from_local_datetime(&naive)
                .earliest()
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|| naive.and_utc()),
        ),
        Err(_) => Some(naive.and_utc()),
    }
}

/// Plain text as the minimal HTML Graph expects for a reply message.
fn text_to_html(text: &str) -> String {
    let mut out = String::from("<html><body>");
    for (index, line) in text.replace("\r\n", "\n").replace('\r', "\n").split('\n').enumerate() {
        if index > 0 {
            out.push_str("<br>");
        }
        for ch in line.chars() {
            match ch {
                '&' => out.push_str("&amp;"),
                '<' => out.push_str("&lt;"),
                '>' => out.push_str("&gt;"),
                _ => out.push(ch),
            }
        }
    }
    out.push_str("</body></html>");
    out
}

/// One HTML entity back to text; an unknown entity round-trips verbatim.
fn decode_entity(entity: &str) -> String {
    match entity.to_ascii_lowercase().as_str() {
        "amp" => "&".to_string(),
        "lt" => "<".to_string(),
        "gt" => ">".to_string(),
        "quot" => "\"".to_string(),
        "apos" | "#39" => "'".to_string(),
        "nbsp" => " ".to_string(),
        "ndash" => "–".to_string(),
        "mdash" => "—".to_string(),
        other => format!("&{other};"),
    }
}

/// Graph's reply HTML back to plain text: block tags and `<br>` end a line,
/// entities are decoded, everything else is stripped.
fn html_to_text(html: &str) -> String {
    let mut out = String::new();
    let mut tag = String::new();
    let mut entity = String::new();
    let mut in_tag = false;
    let mut in_entity = false;
    for ch in html.chars() {
        if in_tag {
            if ch == '>' {
                in_tag = false;
                let name = tag.trim().trim_start_matches('/').split([' ', '\t', '\r', '\n']).next().unwrap_or("").to_ascii_lowercase();
                let block = matches!(name.as_str(), "p" | "div" | "li" | "tr" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "blockquote");
                if name == "br" {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                } else if block && !out.is_empty() && !out.ends_with('\n') {
                    out.push('\n');
                }
                tag.clear();
            } else {
                tag.push(ch);
            }
            continue;
        }
        if in_entity {
            if ch == ';' {
                in_entity = false;
                out.push_str(&decode_entity(&entity));
                entity.clear();
            } else if entity.len() >= 12 || ch == '&' || ch.is_whitespace() {
                // Not an entity after all - keep what was read.
                in_entity = false;
                out.push('&');
                out.push_str(&entity);
                entity.clear();
                if ch == '&' {
                    in_entity = true;
                } else {
                    out.push(ch);
                }
            } else {
                entity.push(ch);
            }
            continue;
        }
        match ch {
            '<' => in_tag = true,
            '&' => in_entity = true,
            '\r' => {}
            _ => out.push(ch),
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Value {
        serde_json::from_str(include_str!("../../../test-fixtures/outlook/automatic-replies.json")).unwrap()
    }

    #[test]
    fn reads_a_captured_mailbox_settings_response() {
        let replies = automatic_replies_from_graph(&fixture());
        assert_eq!(replies.status, AutomaticReplyStatus::Scheduled);
        assert_eq!(replies.external_audience, ExternalAudience::ContactsOnly);
        // 09:00 and 17:00 on 2026-10-19/23 in "W. Europe Standard Time"
        // (Europe/Berlin, still CEST at the end of October) are 07:00/15:00Z.
        assert_eq!(replies.start, Some("2026-10-19T07:00:00Z".parse::<DateTime<Utc>>().unwrap()));
        assert_eq!(replies.end, Some("2026-10-23T15:00:00Z".parse::<DateTime<Utc>>().unwrap()));
        assert_eq!(replies.internal_message, "I'm away until Friday.\n\nBack Monday.");
        assert_eq!(replies.external_message, "Out of office.");
    }

    #[test]
    fn missing_settings_mean_disabled() {
        let replies = automatic_replies_from_graph(&json!({ "timeZone": "UTC" }));
        assert_eq!(replies, AutomaticReplies::default());
    }

    #[test]
    fn body_schedules_only_scheduled_replies() {
        let always = AutomaticReplies {
            status: AutomaticReplyStatus::AlwaysEnabled,
            start: Some("2026-10-19T07:00:00Z".parse().unwrap()),
            end: Some("2026-10-23T15:00:00Z".parse().unwrap()),
            internal_message: "Away".into(),
            external_message: "Away".into(),
            external_audience: ExternalAudience::All,
        };
        let body = automatic_replies_body(&always);
        let replies = &body["automaticRepliesSetting"];
        assert_eq!(replies["status"], "alwaysEnabled");
        assert_eq!(replies["scheduledStartDateTime"], Value::Null, "a stale range must be cleared");
        assert_eq!(replies["scheduledEndDateTime"], Value::Null);
        assert_eq!(replies["internalReplyMessage"], "<html><body>Away</body></html>");

        let scheduled = AutomaticReplies {
            status: AutomaticReplyStatus::Scheduled,
            ..always
        };
        let body = automatic_replies_body(&scheduled);
        let replies = &body["automaticRepliesSetting"];
        assert_eq!(replies["scheduledStartDateTime"], json!({ "dateTime": "2026-10-19T07:00:00", "timeZone": "UTC" }));
        assert_eq!(replies["scheduledEndDateTime"], json!({ "dateTime": "2026-10-23T15:00:00", "timeZone": "UTC" }));
    }

    #[test]
    fn messages_round_trip_plain_text_through_html() {
        for message in ["Away", "Line one\nLine two", "Away <today> & tomorrow", "First\n\n-- \nGavin"] {
            assert_eq!(html_to_text(&text_to_html(message)), message, "message: {message:?}");
        }
        assert_eq!(text_to_html("a<b&c"), "<html><body>a&lt;b&amp;c</body></html>");
        assert_eq!(html_to_text("<html><body>a<br>b</body></html>"), "a\nb");
    }

    #[test]
    fn utc_and_unknown_zones_parse() {
        let utc = json!({ "dateTime": "2026-10-19T07:00:00.0000000", "timeZone": "UTC" });
        assert_eq!(instant_from_graph(&utc), Some("2026-10-19T07:00:00Z".parse().unwrap()));
        // A missing zone is read as UTC; an unknown one falls back to it too.
        assert_eq!(
            instant_from_graph(&json!({ "dateTime": "2026-10-19T07:00:00" })),
            Some("2026-10-19T07:00:00Z".parse().unwrap())
        );
        assert_eq!(
            instant_from_graph(&json!({ "dateTime": "2026-10-19T07:00:00", "timeZone": "Mars/Phobos" })),
            Some("2026-10-19T07:00:00Z".parse().unwrap())
        );
    }
}
