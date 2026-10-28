//! Delivery status notifications (OUTLOOK_PARITY.md §4.1, M-27 / C-07).
//!
//! Outlook's "Request a delivery receipt" is the SMTP service extension of
//! RFC 3461: the request rides the *envelope*, not the message. `MAIL FROM`
//! may carry `RET` (how much of the message a report echoes) and each
//! `RCPT TO` carries `NOTIFY` (which outcomes to report). Lookout asks for
//! `NOTIFY=SUCCESS,FAILURE`, matching Outlook; it does not send `RET` or
//! `ENVID` (Microsoft's published DSN description names only `NOTIFY`, and
//! Outlook's own choice of `RET` is marked **verify** in §4.1).
//!
//! The report that comes back is a `multipart/report; report-type=
//! delivery-status` document (RFC 3464): a human-readable explanation, a
//! `message/delivery-status` body carrying one per-message block followed by
//! one block per recipient, and optionally the original message's headers.
//! This module owns that wire vocabulary and the parser; the SMTP transport
//! (`lookout-mail`'s `send.rs`) and the reading-pane card (`lookout-app`)
//! only consume it.
//!
//! A saved draft cannot carry an envelope parameter, so the composer's
//! choice is written into the draft as a private header and stripped by
//! omission from the sent message (see [`DELIVERY_RECEIPT_DRAFT_HEADER`]).

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use crate::email::header_value;

/// The `report-type` parameter RFC 3464 gives a delivery status notification.
pub const DELIVERY_STATUS_REPORT_TYPE: &str = "delivery-status";
/// The MIME type of a delivery status notification's machine-readable part.
pub const DELIVERY_STATUS_PART_CONTENT_TYPE: &str = "message/delivery-status";
/// RFC 8098's `report-type` for a read receipt, the other report Outlook's
/// receipt requests produce. Kept here so the two are never confused.
pub const DISPOSITION_NOTIFICATION_REPORT_TYPE: &str = "disposition-notification";
/// The RFC 3461 `NOTIFY` parameter keyword (per `RCPT TO`).
pub const NOTIFY_PARAMETER: &str = "NOTIFY";
/// RFC 3461 `RET` parameter keyword (per `MAIL FROM`); not used today.
pub const RET_PARAMETER: &str = "RET";
/// The `NOTIFY` value Outlook asks for: both successful and failed delivery.
pub const NOTIFY_SUCCESS_FAILURE: &str = "SUCCESS,FAILURE";
/// Draft-only header recording that the composer wants a delivery receipt.
/// It is never written to a sent message - the request is an envelope
/// parameter (RFC 3461 §4), and a header claiming otherwise would be noise
/// on the wire. Written by `build_draft_raw_message`, read by
/// `parse_draft_prefill`, so reopening a draft restores the toggle.
pub const DELIVERY_RECEIPT_DRAFT_HEADER: &str = "X-Lookout-Delivery-Receipt";
/// The value [`DELIVERY_RECEIPT_DRAFT_HEADER`] carries when it is set.
pub const DELIVERY_RECEIPT_DRAFT_VALUE: &str = "requested";

/// The `(keyword, value)` pair a receipt-requesting recipient gets on its
/// `RCPT TO` line, or `None` when no receipt was requested.
pub fn rcpt_notify_parameter(requested: bool) -> Option<(&'static str, &'static str)> {
    requested.then_some((NOTIFY_PARAMETER, NOTIFY_SUCCESS_FAILURE))
}

/// Whether a saved draft's headers ask for a delivery receipt.
pub fn draft_requests_delivery_receipt(headers: &[(String, String)]) -> bool {
    header_value(headers, DELIVERY_RECEIPT_DRAFT_HEADER)
        .map(|value| value.trim().eq_ignore_ascii_case(DELIVERY_RECEIPT_DRAFT_VALUE))
        .unwrap_or(false)
}

/// The `report-type` parameter of a message's top-level `Content-Type`, if
/// it is a report at all.
pub fn report_type(headers: &[(String, String)]) -> Option<String> {
    let value = header_value(headers, "content-type")?;
    let lower = value.to_ascii_lowercase();
    if !lower.contains("multipart/report") {
        return None;
    }
    let start = lower.find("report-type")?;
    let rest = value[start + "report-type".len()..].trim_start();
    let rest = rest.strip_prefix('=')?.trim_start();
    if let Some(quoted) = rest.strip_prefix('"') {
        return quoted.split('"').next().map(|kind| kind.trim().to_ascii_lowercase());
    }
    rest.split([';', ' ', '\t']).next().map(|kind| kind.trim().to_ascii_lowercase())
}

/// Whether the message is an RFC 3464 delivery status notification (as
/// opposed to a read receipt, also a `multipart/report`).
pub fn is_delivery_report(headers: &[(String, String)]) -> bool {
    report_type(headers).as_deref() == Some(DELIVERY_STATUS_REPORT_TYPE)
}

/// What happened to one recipient, from the report's `Action` field
/// (RFC 3464 §2.3.3), with the status code as the fallback when a report
/// omits `Action`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ReportAction {
    Delivered,
    Failed,
    Delayed,
    Relayed,
    Expanded,
    Unknown,
}

impl ReportAction {
    /// Parses an RFC 3464 `Action` value, case-insensitively.
    pub fn from_field(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "delivered" => Some(ReportAction::Delivered),
            "failed" => Some(ReportAction::Failed),
            "delayed" => Some(ReportAction::Delayed),
            "relayed" => Some(ReportAction::Relayed),
            "expanded" => Some(ReportAction::Expanded),
            _ => None,
        }
    }

    /// Infers the action from an RFC 3463 status code's class digit, for the
    /// reports that omit `Action`: `2.x.x` delivered, `4.x.x` delayed,
    /// `5.x.x` failed.
    pub fn from_status(status: &str) -> Option<Self> {
        match status.trim().as_bytes().first()? {
            b'2' => Some(ReportAction::Delivered),
            b'4' => Some(ReportAction::Delayed),
            b'5' => Some(ReportAction::Failed),
            _ => None,
        }
    }
}

/// One recipient's block of a delivery status notification.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReportedRecipient {
    /// RFC 822 `Final-Recipient` (the generic type stripped).
    pub final_recipient: String,
    /// RFC 822 `Original-Recipient`, when the reporting MTA kept it.
    pub original_recipient: Option<String>,
    pub action: ReportAction,
    /// The RFC 3463 status code, e.g. `5.1.1`.
    pub status: Option<String>,
    /// The server's own text, e.g. `smtp; 550 5.1.1 no such user`.
    pub diagnostic_code: Option<String>,
    /// The MTA that reported the outcome.
    pub remote_mta: Option<String>,
    /// `Last-Attempt-Date` verbatim.
    pub last_attempt: Option<String>,
}

/// The overall verdict a report carries, which the reading pane's card uses
/// for its headline: one failed recipient makes the report a failure even if
/// another recipient was delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DeliveryReportKind {
    Delivered,
    Failed,
    Delayed,
    Mixed,
}

impl DeliveryReportKind {
    /// Derives the verdict from the per-recipient actions.
    pub fn from_recipients(recipients: &[ReportedRecipient]) -> Self {
        let mut delivered = false;
        let mut failed = false;
        let mut delayed = false;
        for recipient in recipients {
            match recipient.action {
                ReportAction::Delivered => delivered = true,
                ReportAction::Failed => failed = true,
                ReportAction::Delayed => delayed = true,
                ReportAction::Relayed | ReportAction::Expanded | ReportAction::Unknown => {}
            }
        }
        if failed {
            DeliveryReportKind::Failed
        } else if delayed {
            DeliveryReportKind::Delayed
        } else if delivered {
            DeliveryReportKind::Delivered
        } else {
            DeliveryReportKind::Mixed
        }
    }
}

/// A parsed RFC 3464 delivery status notification.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DeliveryReport {
    /// `Reporting-MTA` (generic type stripped), the server that wrote it.
    pub reporting_mta: Option<String>,
    /// The original message's `Message-ID`, from the report's third part
    /// (`text/rfc822-headers` or `message/rfc822`) when it carries one.
    pub original_message_id: Option<String>,
    /// The original message's subject, from the same part.
    pub original_subject: Option<String>,
    /// One entry per `Final-Recipient` block, in report order.
    pub recipients: Vec<ReportedRecipient>,
    /// The headline verdict.
    pub kind: DeliveryReportKind,
    /// The report's own human-readable explanation (`text/plain` part).
    pub human_text: Option<String>,
}

impl DeliveryReport {
    /// The recipients whose delivery failed, for the card's detail rows.
    pub fn failed_recipients(&self) -> impl Iterator<Item = &ReportedRecipient> {
        self.recipients.iter().filter(|recipient| recipient.action == ReportAction::Failed)
    }
}

/// Parses a delivery status notification: the message's headers (to confirm
/// the report type), the decoded `message/delivery-status` part, and
/// optionally the original message's headers (the report's third part).
/// Returns `None` when the message isn't a DSN or the part carries no
/// recipient block.
pub fn parse_delivery_report(headers: &[(String, String)], delivery_status: &[u8], original_headers: Option<&[u8]>, human_text: Option<String>) -> Option<DeliveryReport> {
    if !is_delivery_report(headers) {
        return None;
    }
    let text = String::from_utf8_lossy(delivery_status);
    let blocks = split_fields_into_blocks(&text);
    let mut blocks = blocks.into_iter();
    let per_message = blocks.next().unwrap_or_default();
    let reporting_mta = field_value(&per_message, "Reporting-MTA").map(strip_generic_type);

    let mut recipients = Vec::new();
    for block in blocks {
        let final_recipient = field_value(&block, "Final-Recipient").map(strip_generic_type).filter(|value| !value.is_empty());
        let action = field_value(&block, "Action")
            .and_then(ReportAction::from_field)
            .or_else(|| field_value(&block, "Status").and_then(ReportAction::from_status));
        let (Some(final_recipient), Some(action)) = (final_recipient, action) else {
            continue;
        };
        recipients.push(ReportedRecipient {
            final_recipient,
            original_recipient: field_value(&block, "Original-Recipient").map(strip_generic_type).filter(|value| !value.is_empty()),
            action,
            status: field_value(&block, "Status").map(str::trim).filter(|value| !value.is_empty()).map(str::to_string),
            diagnostic_code: field_value(&block, "Diagnostic-Code").map(str::trim).filter(|value| !value.is_empty()).map(str::to_string),
            remote_mta: field_value(&block, "Remote-MTA").map(strip_generic_type).filter(|value| !value.is_empty()),
            last_attempt: field_value(&block, "Last-Attempt-Date")
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string),
        });
    }
    if recipients.is_empty() {
        return None;
    }
    let kind = DeliveryReportKind::from_recipients(&recipients);
    let (original_message_id, original_subject) = original_headers.map(original_message_fields).unwrap_or((None, None));

    Some(DeliveryReport {
        reporting_mta,
        original_message_id,
        original_subject,
        kind,
        human_text: human_text.filter(|text| !text.trim().is_empty()),
        recipients,
    })
}

/// Splits a `message/delivery-status` body into RFC 3464 blocks: field
/// groups separated by blank lines, with folded continuation lines unfolded.
fn split_fields_into_blocks(text: &str) -> Vec<Vec<(String, String)>> {
    let mut blocks: Vec<Vec<(String, String)>> = Vec::new();
    let mut current: Vec<(String, String)> = Vec::new();
    for raw_line in text.split('\n') {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        if line.trim().is_empty() {
            if !current.is_empty() {
                blocks.push(std::mem::take(&mut current));
            }
            continue;
        }
        // RFC 5322 unfolding: a line starting with whitespace continues the
        // previous field. Delivery-status fields are defined as 7-bit text
        // and can legally wrap the same way message headers do.
        if line.starts_with([' ', '\t']) {
            if let Some((_, value)) = current.last_mut() {
                value.push(' ');
                value.push_str(line.trim());
                continue;
            }
        }
        if let Some((name, value)) = line.split_once(':') {
            current.push((name.trim().to_string(), value.trim().to_string()));
        }
    }
    if !current.is_empty() {
        blocks.push(current);
    }
    blocks
}

/// Case-insensitive lookup of one field in a parsed block.
fn field_value<'a>(block: &'a [(String, String)], name: &str) -> Option<&'a str> {
    block.iter().find(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, value)| value.as_str())
}

/// Strips an RFC 3464 address/type prefix (`rfc822; user@example.com`,
/// `dns; mx.example.com`) down to the value.
fn strip_generic_type(value: &str) -> String {
    value.split_once(';').map_or(value, |(_, rest)| rest).trim().to_string()
}

/// Reads the original message's `Message-ID` and `Subject` out of a report's
/// third part (the original headers, either `text/rfc822-headers` or the
/// enclosed `message/rfc822`).
fn original_message_fields(raw: &[u8]) -> (Option<String>, Option<String>) {
    let text = String::from_utf8_lossy(raw);
    let headers: Vec<(String, String)> = text
        .split('\n')
        .take_while(|line| !line.trim().is_empty())
        .filter_map(|line| {
            let line = line.strip_suffix('\r').unwrap_or(line);
            line.split_once(':').map(|(name, value)| (name.trim().to_string(), value.trim().to_string()))
        })
        .collect();
    let message_id = header_value(&headers, "message-id")
        .map(|value| value.trim().trim_start_matches('<').trim_end_matches('>').to_string())
        .filter(|value| !value.is_empty());
    let subject = header_value(&headers, "subject").map(str::to_string).filter(|value| !value.trim().is_empty());
    (message_id, subject)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(content_type: &str) -> Vec<(String, String)> {
        vec![("Content-Type".to_string(), content_type.to_string())]
    }

    fn failed_report() -> Vec<u8> {
        b"Reporting-MTA: dns; mx.lookout.test\r\n\
Arrival-Date: Mon, 12 Jan 2026 10:00:00 +0000\r\n\
\r\n\
Final-Recipient: rfc822; nobody@example.test\r\n\
Original-Recipient: rfc822; nobody@example.test\r\n\
Action: failed\r\n\
Status: 5.1.1\r\n\
Remote-MTA: dns; mx.example.test\r\n\
Diagnostic-Code: smtp; 550 5.1.1 <nobody@example.test>: Recipient address rejected\r\n\
Last-Attempt-Date: Mon, 12 Jan 2026 10:00:02 +0000\r\n"
            .to_vec()
    }

    #[test]
    fn report_type_is_read_with_and_without_quotes() {
        assert_eq!(
            report_type(&headers("multipart/report; report-type=delivery-status; boundary=\"x\"")),
            Some("delivery-status".to_string())
        );
        assert_eq!(
            report_type(&headers("Multipart/Report; Report-Type=\"Disposition-Notification\"")),
            Some("disposition-notification".to_string())
        );
        assert_eq!(report_type(&headers("multipart/mixed; boundary=x")), None);
        assert_eq!(report_type(&[]), None);
    }

    #[test]
    fn a_disposition_report_is_not_a_delivery_report() {
        assert!(!is_delivery_report(&headers("multipart/report; report-type=disposition-notification; boundary=x")));
        assert!(is_delivery_report(&headers("multipart/report; report-type=delivery-status; boundary=x")));
    }

    #[test]
    fn failed_report_parses_every_field() {
        let report = parse_delivery_report(
            &headers("multipart/report; report-type=delivery-status; boundary=\"b\""),
            &failed_report(),
            None,
            Some("Your message couldn't be delivered.".to_string()),
        )
        .expect("a failed DSN parses");
        assert_eq!(report.kind, DeliveryReportKind::Failed);
        assert_eq!(report.reporting_mta.as_deref(), Some("mx.lookout.test"));
        assert_eq!(report.recipients.len(), 1);
        let recipient = &report.recipients[0];
        assert_eq!(recipient.final_recipient, "nobody@example.test");
        assert_eq!(recipient.action, ReportAction::Failed);
        assert_eq!(recipient.status.as_deref(), Some("5.1.1"));
        assert_eq!(recipient.remote_mta.as_deref(), Some("mx.example.test"));
        assert!(recipient.diagnostic_code.as_deref().is_some_and(|code| code.contains("550 5.1.1")));
        assert!(recipient.last_attempt.is_some());
        assert_eq!(report.human_text.as_deref(), Some("Your message couldn't be delivered."));
    }

    #[test]
    fn a_report_can_carry_several_recipients_with_mixed_outcomes() {
        let status = b"Reporting-MTA: dns; mx.lookout.test\r\n\r\n\
Final-Recipient: rfc822; ok@example.test\r\nAction: delivered\r\nStatus: 2.0.0\r\n\r\n\
Final-Recipient: rfc822; bad@example.test\r\nAction: failed\r\nStatus: 5.0.0\r\n"
            .to_vec();
        let report = parse_delivery_report(&headers("multipart/report; report-type=delivery-status"), &status, None, None).expect("parses");
        assert_eq!(report.recipients.len(), 2);
        assert_eq!(report.kind, DeliveryReportKind::Failed);
        assert_eq!(report.failed_recipients().count(), 1);
        assert_eq!(report.failed_recipients().next().unwrap().final_recipient, "bad@example.test");
    }

    #[test]
    fn delivered_and_delayed_reports_get_their_own_kind() {
        let delivered = b"Reporting-MTA: dns; mx\r\n\r\nFinal-Recipient: rfc822; a@b.test\r\nAction: delivered\r\nStatus: 2.0.0\r\n".to_vec();
        let delayed = b"Reporting-MTA: dns; mx\r\n\r\nFinal-Recipient: rfc822; a@b.test\r\nAction: delayed\r\nStatus: 4.4.1\r\n".to_vec();
        assert_eq!(
            parse_delivery_report(&headers("multipart/report; report-type=delivery-status"), &delivered, None, None)
                .unwrap()
                .kind,
            DeliveryReportKind::Delivered
        );
        assert_eq!(
            parse_delivery_report(&headers("multipart/report; report-type=delivery-status"), &delayed, None, None)
                .unwrap()
                .kind,
            DeliveryReportKind::Delayed
        );
    }

    #[test]
    fn a_missing_action_falls_back_to_the_status_class() {
        let status = b"Reporting-MTA: dns; mx\r\n\r\nFinal-Recipient: rfc822; a@b.test\r\nStatus: 5.7.1\r\n".to_vec();
        let report = parse_delivery_report(&headers("multipart/report; report-type=delivery-status"), &status, None, None).expect("parses");
        assert_eq!(report.recipients[0].action, ReportAction::Failed);
        assert_eq!(report.kind, DeliveryReportKind::Failed);
    }

    #[test]
    fn folded_fields_are_unfolded() {
        let status = b"Reporting-MTA: dns; mx\r\n\r\nFinal-Recipient: rfc822;\r\n a@b.test\r\nAction: failed\r\n".to_vec();
        let report = parse_delivery_report(&headers("multipart/report; report-type=delivery-status"), &status, None, None).expect("parses");
        assert_eq!(report.recipients[0].final_recipient, "a@b.test");
    }

    #[test]
    fn the_original_headers_supply_the_subject_and_message_id() {
        let original = b"From: me@lookout.test\r\nSubject: Quarterly report\r\nMessage-ID: <abc123@lookout.test>\r\n\r\nbody ignored\r\n";
        let report = parse_delivery_report(&headers("multipart/report; report-type=delivery-status"), &failed_report(), Some(original), None).expect("parses");
        assert_eq!(report.original_subject.as_deref(), Some("Quarterly report"));
        assert_eq!(report.original_message_id.as_deref(), Some("abc123@lookout.test"));
    }

    #[test]
    fn a_report_without_recipient_blocks_parses_to_none() {
        let empty = b"Reporting-MTA: dns; mx\r\n".to_vec();
        assert_eq!(parse_delivery_report(&headers("multipart/report; report-type=delivery-status"), &empty, None, None), None);
        // And a read receipt is never mistaken for one, even with the same body.
        assert_eq!(
            parse_delivery_report(&headers("multipart/report; report-type=disposition-notification"), &failed_report(), None, None),
            None
        );
    }

    #[test]
    fn the_draft_header_is_recognised_case_insensitively() {
        let present = vec![(DELIVERY_RECEIPT_DRAFT_HEADER.to_string(), "Requested".to_string())];
        assert!(draft_requests_delivery_receipt(&present));
        assert!(!draft_requests_delivery_receipt(&[]));
        assert!(!draft_requests_delivery_receipt(&[(DELIVERY_RECEIPT_DRAFT_HEADER.to_string(), "no".to_string())]));
    }

    #[test]
    fn the_rcpt_parameter_matches_outlooks_request() {
        assert_eq!(rcpt_notify_parameter(true), Some((NOTIFY_PARAMETER, NOTIFY_SUCCESS_FAILURE)));
        assert_eq!(rcpt_notify_parameter(false), None);
    }

    #[test]
    fn report_action_parsing_is_case_insensitive() {
        assert_eq!(ReportAction::from_field("DELIVERED"), Some(ReportAction::Delivered));
        assert_eq!(ReportAction::from_field("bogus"), None);
        assert_eq!(ReportAction::from_status("4.2.0"), Some(ReportAction::Delayed));
    }
}
