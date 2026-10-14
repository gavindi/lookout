/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use lettre::address::Envelope;
use lettre::transport::smtp::authentication::{Credentials, Mechanism};
use lettre::{Address as SmtpAddress, AsyncSmtpTransport, AsyncTransport, Tokio1Executor};
use mail_builder::headers::address::Address as BuilderAddress;
use mail_builder::headers::content_type::ContentType;
use mail_builder::headers::raw::Raw;
use mail_builder::headers::HeaderType;
use mail_builder::mime::{BodyPart as MimeBodyPart, MimePart};
use mail_builder::MessageBuilder;

use crate::config::{Credential, EndpointConfig};
use crate::error::{Error, Result};

/// An RFC 8098 read-receipt (MDN) to attach to a message: the receipt body is
/// a `multipart/report; report-type=disposition-notification` document, not a
/// composed text. The app fills this in when the user (or the
/// send-receipts-automatically policy) responds to a displayed message's
/// `Disposition-Notification-To` request.
#[derive(Debug, Clone)]
pub struct ReadReceipt {
    /// The original message's `Message-ID`, bare or angle-bracketed - echoed
    /// into the report's `Original-Message-ID` field and the envelope's
    /// `In-Reply-To`/`References`, so the sender's client threads the
    /// receipt to the message it answers.
    pub original_message_id: String,
    /// The original message's `From` address - the address that requested
    /// the receipt, shown in the human-readable part.
    pub original_from: String,
    /// The original message's `Subject`, recycled into the receipt's own
    /// subject line ("Read receipt: <subject>") and the human part.
    pub original_subject: String,
    /// The original message's `Date` value, verbatim - emitted as the
    /// report's `Received-Date` field when present.
    pub original_date: Option<String>,
    /// The address the receipt is sent *for* (the receiving account's own
    /// address) - the report's `Original-Recipient`/`Final-Recipient`.
    pub final_recipient: String,
    /// Whether the receipt was sent automatically by policy rather than by
    /// an explicit user click - picks the RFC 8098 §2.2.8 disposition mode:
    /// `automatic-action/MDN-sent-automatically` vs
    /// `manual-action/MDN-sent-manually`.
    pub automatic: bool,
    /// The local time the message was displayed, formatted for the
    /// human-readable part's prose.
    pub displayed_at: String,
    /// The original message's headers, re-emitted verbatim as the report's
    /// third (`message/rfc822`) part - RFC 8098 §3.2's "just the headers"
    /// form, which is all a receipt consumer needs to identify the message.
    pub original_headers: Vec<(String, String)>,
}

/// A regular (non-inline) file attached to a composed message. `content_type`
/// is always supplied by the caller - the app crate resolves it from the
/// picked file before building this; nothing here guesses it.
#[derive(Debug, Clone, PartialEq)]
pub struct Attachment {
    pub filename: String,
    pub content_type: String,
    pub bytes: Vec<u8>,
}

/// An inline, `cid:`-referenced image embedded in an HTML body - extracted
/// from a `data:` URI at compose time (see the app crate's `read_content`).
/// `cid` is the bare Content-ID (no angle brackets) the HTML body's
/// `<img src="cid:...">` already references; `mail_builder`'s `.inline()`
/// adds the brackets itself when writing the `Content-ID` header.
#[derive(Debug, Clone, PartialEq)]
pub struct InlineImage {
    pub cid: String,
    pub content_type: String,
    pub bytes: Vec<u8>,
}

/// A message the user has composed, ready to be built into a raw RFC 5322
/// document and sent. `from` is the sending identity's address (an account
/// can send as several identities); `display_name`, when set, goes into the
/// `From:` header alongside it. `text_body` is always set (the plain-text
/// fallback); `html_body`, when present, is emitted alongside it as a
/// `multipart/alternative` so HTML-capable clients get the rich version and
/// everything else the text.
#[derive(Debug, Clone)]
pub struct ComposedMessage {
    pub from: String,
    /// The sending identity's display name, if any. Never sent bare: the
    /// `From:` header is `Name <address>` when set, plain `<address>`
    /// otherwise.
    pub display_name: Option<String>,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    /// RFC 5322 `Reply-To` addresses, from the sending identity.
    pub reply_to: Vec<String>,
    pub subject: String,
    /// Message importance (Outlook's High/Normal/Low). `Normal` is the
    /// default and emits no header; `High`/`Low` are written by
    /// `build_raw_message` as the three Outlook headers (`Importance`,
    /// `X-Priority`, `X-MSMail-Priority`) via the Phase 0.1 interop layer.
    pub importance: lookout_core::Importance,
    /// Message sensitivity (RFC 2156 `Sensitivity`). `Normal` is the default
    /// and leaves the header off the wire, matching Outlook; `Personal`,
    /// `Private` and `CompanyConfidential` write it.
    pub sensitivity: lookout_core::MessageSensitivity,
    pub text_body: String,
    /// Optional HTML rendering of the same body. Both parts are sent when
    /// set; recipients that only handle plain text fall back to `text_body`.
    pub html_body: Option<String>,
    /// Regular (non-inline) file attachments. Ignored by the calendar-reply
    /// and read-receipt branches below, which build their own MIME body via
    /// `.body(...)` and so never see `mail_builder`'s attachment assembly.
    pub attachments: Vec<Attachment>,
    /// Inline, `cid:`-referenced images embedded in `html_body`. Same
    /// calendar-reply/read-receipt exclusion as `attachments`.
    pub inline_images: Vec<InlineImage>,
    /// An RFC 6047 iMIP payload: the raw iCalendar document (which itself
    /// carries its `METHOD` - see `lookout-dav`'s `build_imip_vcalendar`).
    /// When set, the message is built as `multipart/alternative`
    /// [text/plain, text/calendar; method=...] instead of the text/html
    /// form - a calendar reply has nothing to gain from an HTML rendering.
    pub calendar_part: Option<String>,
    /// An RFC 8098 read-receipt payload (see [`ReadReceipt`]). When set, the
    /// message is built as `multipart/report` [text/plain,
    /// message/disposition-notification, message/rfc822] instead of the
    /// text/html form - the receipt is a report document, not prose.
    pub read_receipt: Option<ReadReceipt>,
    /// Ask the recipients for a read receipt: adds RFC 8098's
    /// `Disposition-Notification-To: <from>` header to the built message.
    pub request_read_receipt: bool,
    /// RFC 5322 `In-Reply-To`, when replying to a message.
    pub in_reply_to: Option<String>,
    /// RFC 5322 `References`, when replying to a message.
    pub references: Vec<String>,
    /// Pre-generated `Message-ID` (bare, no angle brackets). Drafts set this
    /// to a stable per-compose-session id so every autosave carries the same
    /// `Message-ID` - that's how a later save finds and replaces the earlier
    /// one server-side. `None` for ordinary sends, which get a fresh id each
    /// time (see `new_message_id`).
    pub message_id: Option<String>,
}

/// A queued outgoing message persisted in the account's local outbox
/// (OUTLOOK_PARITY.md §2.4 decision 2, item 0.3). This is the "single
/// mechanism" behind undo send, schedule send and the offline outbox: a
/// `ComposedMessage` is turned into this payload at enqueue time, sent when
/// its `not_before` time arrives, and retried with backoff on failure.
///
/// The raw RFC 5322 bytes are built once, at enqueue, so a later send is a
/// pure SMTP delivery with no re-rendering; they are stored base64-encoded in
/// the cache's `outbox.payload` BLOB. The SMTP envelope (`from` plus the flat
/// `recipients` list) travels alongside because it can't be recovered from
/// the headers - bcc especially must be in the envelope but never in a
/// header. `draft_message_id` records the Drafts copy to delete *after* a
/// successful send, so a queued message that fails or is scheduled leaves the
/// draft in place ("drafts stay in Drafts until sent").
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OutboxMessage {
    pub from: String,
    pub recipients: Vec<String>,
    /// Base64 (standard alphabet) of the raw RFC 5322 message bytes.
    pub raw_base64: String,
    pub draft_message_id: Option<String>,
    /// The subject, kept for the outbox's own UI (a scheduled-send banner,
    /// item 1.14) without having to parse the raw message.
    pub subject: String,
    /// The blind-copy list as composed. The raw message never carries a
    /// `Bcc` header (it would leak on the wire), so this is how a withdrawn
    /// scheduled send gets its Bcc back when it reopens in the composer
    /// (item 1.14). Defaulted so rows queued before it existed still decode.
    #[serde(default)]
    pub bcc: Vec<String>,
}

impl OutboxMessage {
    /// Builds the outbox payload for `msg`. A pre-set `message_id` (a saved
    /// draft's stable id) is reused, so the sent copy and the draft it
    /// replaces share a `Message-ID`.
    pub fn from_composed(msg: &ComposedMessage) -> Self {
        let (raw, _message_id, recipients) = build_raw_message(msg);
        OutboxMessage {
            from: msg.from.clone(),
            recipients,
            raw_base64: base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &raw),
            draft_message_id: msg.message_id.clone(),
            subject: msg.subject.clone(),
            bcc: msg.bcc.clone(),
        }
    }

    /// The raw RFC 5322 message bytes, or `None` if the stored base64 is
    /// corrupt - a row to drop rather than retry.
    pub fn raw(&self) -> Option<Vec<u8>> {
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &self.raw_base64).ok()
    }
}

/// Generates a fresh globally-unique bare `Message-ID` (no angle brackets -
/// `mail_builder`'s header writer adds them).
pub fn new_message_id() -> String {
    format!("{}@lookout.local", uuid::Uuid::new_v4())
}

/// Builds the raw RFC 5322 message a *draft* save should store. Identical to
/// [`build_raw_message`] except the `Bcc` header is written: it is the only
/// place a saved draft records its blind-copy list, so reopening the draft
/// can restore it. Send must keep using `build_raw_message` - a `Bcc` header
/// on the wire would leak every blind recipient's address.
pub fn build_draft_raw_message(msg: &ComposedMessage) -> (Vec<u8>, String, Vec<String>) {
    build_raw_message_inner(msg, true)
}

/// Builds the raw RFC 5322 message bytes for `msg`, generating a fresh
/// `Message-ID` unless the message carries a pre-generated one (drafts, see
/// `ComposedMessage::message_id`). Bcc is never written as a header - it only
/// reaches the SMTP envelope (RFC 5322 §3.6.3: the destination addresses are
/// the envelope's `RCPT TO` list, not a header); drafts that must round-trip
/// their Bcc through a save use `build_draft_raw_message`. Returns the bytes
/// plus the Message-ID in use (the caller may want it for thread-tracking)
/// and the flat recipient list (to/cc/bcc combined) needed for the SMTP
/// envelope, since MIME headers alone aren't necessarily what the envelope's
/// `RCPT TO` list should be (bcc must be in the envelope but never in a
/// header).
pub fn build_raw_message(msg: &ComposedMessage) -> (Vec<u8>, String, Vec<String>) {
    build_raw_message_inner(msg, false)
}

fn build_raw_message_inner(msg: &ComposedMessage, include_bcc: bool) -> (Vec<u8>, String, Vec<String>) {
    let message_id = msg.message_id.clone().unwrap_or_else(new_message_id);

    let mut builder = MessageBuilder::new()
        .message_id(message_id.clone())
        .from(BuilderAddress::new_address(msg.display_name.clone(), msg.from.clone()))
        .subject(msg.subject.clone());
    if msg.request_read_receipt {
        // RFC 8098 §2.1.1: the address to send the receipt to - the sending
        // identity's own address, so a recipient's client knows whom to
        // answer. Sent bare (no display name), as the RFC's examples do.
        builder = builder.header("Disposition-Notification-To", raw_header(msg.from.clone()));
    }
    // Outlook's importance headers (RFC 2156 `Importance`, `X-Priority`,
    // `X-MSMail-Priority`). Normal is left off the wire, matching Outlook.
    if msg.importance != lookout_core::Importance::Normal {
        for (name, value) in lookout_core::importance_headers(msg.importance) {
            builder = builder.header(name, raw_header(value));
        }
    }
    // RFC 2156 `Sensitivity`. Normal is left off the wire, matching Outlook.
    if let Some((name, value)) = lookout_core::sensitivity_header(msg.sensitivity) {
        builder = builder.header(name, raw_header(value.to_string()));
    }
    if let Some(receipt) = &msg.read_receipt {
        // RFC 8098 §3: a `multipart/report; report-type=disposition-notification`
        // document - text/plain explanation, the disposition-notification
        // fields, and the original message's headers.
        let report_type = ContentType::new("multipart/report").attribute("report-type", "disposition-notification");
        let human_part = MimePart::new("text/plain", MimeBodyPart::Text(read_receipt_text(receipt).into()));
        let report_part = MimePart::new("message/disposition-notification", MimeBodyPart::Text(disposition_fields(receipt).into()));
        let original_part = MimePart::new("message/rfc822", MimeBodyPart::Text(read_receipt_original_headers(receipt).into()));
        builder = builder.body(MimePart::new(report_type, vec![human_part, report_part, original_part]));
        // RFC 3834: machine-generated mail must say so, or an automated
        // responder on the sender's side would loop answering our answer.
        builder = builder.header("Auto-Submitted", raw_header("auto-generated"));
    } else if let Some(ics) = msg.calendar_part.as_deref().filter(|ics| !ics.is_empty()) {
        // iMIP payload: `multipart/alternative` [text/plain, text/calendar]
        // per RFC 6047 §3.3. The `method=` Content-Type parameter tells the
        // recipient's calendar client how to treat the payload; it is derived
        // from the document's own `METHOD` property, which the
        // `build_imip_vcalendar` caller has already set.
        let method = match lookout_core::parse_imip_method(ics) {
            lookout_core::ImipMethod::Request => "REQUEST",
            lookout_core::ImipMethod::Reply => "REPLY",
            lookout_core::ImipMethod::Cancel => "CANCEL",
            lookout_core::ImipMethod::Counter => "COUNTER",
            lookout_core::ImipMethod::DeclineCounter => "DECLINECOUNTER",
        };
        let calendar_type = ContentType::new("text/calendar").attribute("method", method);
        let text_part = MimePart::new("text/plain", MimeBodyPart::Text(msg.text_body.clone().into()));
        let calendar_part = MimePart::new(calendar_type, MimeBodyPart::Text(ics.to_string().into()));
        // `body()` replaces the builder's own text/html auto-assembly
        // (see `write_body`), which is exactly what we want here.
        builder = builder.body(MimePart::new("multipart/alternative", vec![text_part, calendar_part]));
    } else {
        // `mail_builder` turns a message with both text and html bodies into a
        // `multipart/alternative` (text/plain first, text/html second), so the
        // HTML body is strictly an enhancement over the plain text part.
        builder = builder.text_body(msg.text_body.clone());
        if let Some(html) = msg.html_body.as_deref().filter(|h| !h.is_empty()) {
            builder = builder.html_body(html.to_string());
        }
        // `.inline()`/`.attachment()` both push into the same internal list;
        // `write_body()` wraps everything in one `multipart/mixed` alongside
        // the text/html `multipart/alternative` - no custom MIME tree needed.
        for image in &msg.inline_images {
            builder = builder.inline(image.content_type.clone(), image.cid.clone(), image.bytes.clone());
        }
        for attachment in &msg.attachments {
            builder = builder.attachment(attachment.content_type.clone(), attachment.filename.clone(), attachment.bytes.clone());
        }
    }

    // `to`/`cc`/`reply_to` entries are whatever the composer's recipient
    // chips produced - a bare address, or `Name <address>` when picked from
    // the address book - so each is split before use: the name (if any)
    // goes into the header's display-name slot, never smuggled into the
    // `email` field verbatim (that would nest a second `<...>` around the
    // whole token in the written header).
    if !msg.to.is_empty() {
        builder = builder.to(BuilderAddress::new_list(
            msg.to
                .iter()
                .map(|a| {
                    let (name, address) = split_mailbox(a);
                    BuilderAddress::new_address(name, address)
                })
                .collect(),
        ));
    }
    if !msg.cc.is_empty() {
        builder = builder.cc(BuilderAddress::new_list(
            msg.cc
                .iter()
                .map(|a| {
                    let (name, address) = split_mailbox(a);
                    BuilderAddress::new_address(name, address)
                })
                .collect(),
        ));
    }
    if include_bcc && !msg.bcc.is_empty() {
        // Drafts only (see `build_draft_raw_message`): a `Bcc` header is how
        // a saved draft remembers its blind-copy list. Send never writes it -
        // the header would leak every bcc recipient's address to the To/Cc
        // recipients and to servers along the path.
        builder = builder.bcc(BuilderAddress::new_list(
            msg.bcc
                .iter()
                .map(|a| {
                    let (name, address) = split_mailbox(a);
                    BuilderAddress::new_address(name, address)
                })
                .collect(),
        ));
    }
    if !msg.reply_to.is_empty() {
        builder = builder.reply_to(BuilderAddress::new_list(
            msg.reply_to
                .iter()
                .map(|a| {
                    let (name, address) = split_mailbox(a);
                    BuilderAddress::new_address(name, address)
                })
                .collect(),
        ));
    }
    // Bcc is deliberately not added as a header (that would leak it to
    // every recipient) - it only affects the SMTP envelope's RCPT TO list,
    // built separately below.
    if let Some(irt) = &msg.in_reply_to {
        builder = builder.in_reply_to(irt.clone());
    }
    if !msg.references.is_empty() {
        builder = builder.references(msg.references.clone());
    }

    let raw = builder.write_to_vec().expect("writing to an in-memory Vec cannot fail");
    let mut recipients = msg.to.clone();
    recipients.extend(msg.cc.iter().cloned());
    recipients.extend(msg.bcc.iter().cloned());
    (raw, message_id, recipients)
}

/// The human-readable `text/plain` part of a read receipt (RFC 8098 §3.1.1):
/// plain prose saying what happened to the message, plus the disposition
/// fields inlined for clients that can't render the report part.
fn read_receipt_text(receipt: &ReadReceipt) -> String {
    format!(
        "Your message titled \"{}\" sent to {} was displayed on {}.\r\n\r\nIf your client cannot render this report, the disposition follows:\r\n{}",
        receipt.original_subject,
        receipt.final_recipient,
        receipt.displayed_at,
        disposition_fields(receipt),
    )
}

/// The `message/disposition-notification` body part (RFC 8098 §2.2): a
/// header-field-style block of MDN fields, terminated by a blank line per
/// RFC 8098 §3.1.2 (the fields are a message body, not a header section).
fn disposition_fields(receipt: &ReadReceipt) -> String {
    let mut fields = format!(
        "Reporting-UA: lookout/{}\r\n\
         Original-Recipient: rfc822;<{}>\r\n\
         Final-Recipient: rfc822;<{}>\r\n\
         Original-Message-ID: {}\r\n\
         Disposition: {}; displayed\r\n",
        env!("CARGO_PKG_VERSION"),
        receipt.final_recipient,
        receipt.final_recipient,
        angle_wrap(&receipt.original_message_id),
        if receipt.automatic {
            "automatic-action/MDN-sent-automatically"
        } else {
            "manual-action/MDN-sent-manually"
        },
    );
    if let Some(date) = &receipt.original_date {
        fields.push_str(&format!("Received-Date: {date}\r\n"));
    }
    fields
}

/// The third body part of a read receipt: the original message's headers as
/// a `message/rfc822` document (RFC 8098 §3.2's headers-only form - all a
/// receipt consumer needs to identify the message being answered).
fn read_receipt_original_headers(receipt: &ReadReceipt) -> String {
    receipt
        .original_headers
        .iter()
        .map(|(name, value)| format!("{name}: {value}"))
        .collect::<Vec<_>>()
        .join("\r\n")
}

/// Wraps a Message-ID in angle brackets if it isn't already (the report's
/// `Original-Message-ID` and the message's `In-Reply-To`/`References` expect
/// the RFC 5322 form; the cache stores whatever the wire carried).
fn angle_wrap(message_id: &str) -> String {
    let trimmed = message_id.trim();
    if trimmed.starts_with('<') && trimmed.ends_with('>') {
        trimmed.to_string()
    } else {
        format!("<{trimmed}>")
    }
}

/// Splits an RFC 5322 mailbox string into its display name (if any) and bare
/// address. `ComposedMessage::to`/`cc`/`reply_to` carry whatever the
/// composer's recipient chips produced - a bare address, or `Name <address>`
/// when picked from the address book - and both the MIME header writer
/// (which wants the name and address as separate fields) and the SMTP
/// envelope (whose `Address` parser only accepts a bare address, and rejects
/// the `Name <address>` form outright) need the split, not the raw token.
fn split_mailbox(token: &str) -> (Option<String>, String) {
    let token = token.trim();
    if let Some(start) = token.rfind('<') {
        if let Some(address) = token[start + 1..].strip_suffix('>') {
            let name = token[..start].trim().trim_matches('"').trim();
            return ((!name.is_empty()).then(|| name.to_string()), address.trim().to_string());
        }
    }
    (None, token.to_string())
}

/// Builds a `HeaderType` from a raw value - `mail_builder`'s `.header`
/// wants a typed `HeaderType`, and `Raw` is the escape hatch for values
/// that have no dedicated type (unencoded, line-wrapped only).
fn raw_header(value: impl Into<String>) -> HeaderType<'static> {
    HeaderType::Raw(Raw::new(value.into()))
}

/// Submits `raw` over SMTP. `port == 465` is treated as implicit TLS
/// (SMTPS); anything else uses STARTTLS, which covers the common 587/25
/// configurations - GOA's `SmtpUseSsl`/`SmtpUseTls` flags don't map cleanly
/// enough onto lettre's implicit-vs-STARTTLS split to trust directly (Gmail
/// reports both as true), so the port is the more reliable signal here.
pub async fn send_smtp(endpoint: &EndpointConfig, credential: Credential, from: &str, recipients: &[String], raw: &[u8]) -> Result<()> {
    crate::connection::ensure_crypto_provider_installed();
    let builder = if endpoint.port == 465 {
        AsyncSmtpTransport::<Tokio1Executor>::relay(&endpoint.host)
    } else {
        AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&endpoint.host)
    }
    .map_err(|e| Error::LoginFailed(e.to_string()))?
    .port(endpoint.port);

    let builder = match credential {
        Credential::OAuth2AccessToken(token) => builder
            .credentials(Credentials::new(endpoint.username.clone(), token))
            .authentication(vec![Mechanism::Xoauth2]),
        Credential::Password(password) => builder.credentials(Credentials::new(endpoint.username.clone(), password)),
    };

    let transport = builder.build();

    let from_addr: SmtpAddress = from.parse().map_err(|_| Error::LoginFailed(format!("invalid From address: {from}")))?;
    // `recipients` (built from `ComposedMessage::to`/`cc`/`bcc`) may carry a
    // display name alongside the address (`Name <address>`) - the envelope
    // only wants the bare address, `Address`'s parser has no notion of the
    // `Name <address>` form.
    let to_addrs: Vec<SmtpAddress> = recipients
        .iter()
        .map(|r| {
            let address = split_mailbox(r).1;
            address.parse::<SmtpAddress>().map_err(|_| Error::LoginFailed(format!("invalid recipient address: {r}")))
        })
        .collect::<Result<_>>()?;
    let envelope = Envelope::new(Some(from_addr), to_addrs).map_err(|e| Error::LoginFailed(e.to_string()))?;

    transport.send_raw(&envelope, raw).await.map_err(|e| Error::LoginFailed(e.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_message(html: Option<String>) -> ComposedMessage {
        ComposedMessage {
            from: "me@example.com".to_string(),
            display_name: None,
            to: vec!["you@example.com".to_string()],
            cc: vec![],
            bcc: vec![],
            reply_to: vec![],
            subject: "test".to_string(),
            text_body: "plain part".to_string(),
            html_body: html,
            attachments: vec![],
            inline_images: vec![],
            calendar_part: None,
            read_receipt: None,
            request_read_receipt: false,
            importance: lookout_core::Importance::Normal,
            sensitivity: lookout_core::MessageSensitivity::Normal,
            in_reply_to: None,
            references: vec![],
            message_id: None,
        }
    }

    #[test]
    fn outbox_message_round_trips_raw_and_envelope() {
        let mut msg = sample_message(None);
        msg.bcc = vec!["hidden@example.com".to_string()];
        msg.message_id = Some("draft-1@lookout.local".to_string());
        let outbox = OutboxMessage::from_composed(&msg);

        // The stored bytes are exactly what a direct build produces, and the
        // envelope carries the bcc that never appears in a header.
        let (raw, message_id, recipients) = build_raw_message(&msg);
        assert_eq!(outbox.raw().as_deref(), Some(raw.as_slice()));
        assert_eq!(message_id, "draft-1@lookout.local");
        assert_eq!(outbox.recipients, recipients);
        assert!(outbox.recipients.contains(&"hidden@example.com".to_string()));
        assert_eq!(outbox.draft_message_id.as_deref(), Some("draft-1@lookout.local"));
        assert_eq!(outbox.subject, "test");
        assert_eq!(outbox.bcc, vec!["hidden@example.com".to_string()]);

        // The cache stores this as JSON in a BLOB; it must round-trip intact.
        let encoded = serde_json::to_vec(&outbox).unwrap();
        let decoded: OutboxMessage = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded, outbox);
        assert_eq!(decoded.raw(), outbox.raw());
    }

    #[test]
    fn outbox_message_raw_is_none_when_base64_is_corrupt() {
        let outbox = OutboxMessage {
            from: "a@example.com".into(),
            recipients: vec!["b@example.com".into()],
            raw_base64: "not base64!!".into(),
            draft_message_id: None,
            subject: String::new(),
            bcc: vec![],
        };
        assert_eq!(outbox.raw(), None);
    }

    /// Rows queued before `bcc` existed (v0.9.166) still decode, with no Bcc.
    #[test]
    fn outbox_message_without_bcc_field_still_decodes() {
        let json = br#"{"from":"a@example.com","recipients":["b@example.com"],"raw_base64":"","draft_message_id":null,"subject":"old"}"#;
        let decoded: OutboxMessage = serde_json::from_slice(json).unwrap();
        assert_eq!(decoded.subject, "old");
        assert!(decoded.bcc.is_empty());
    }

    /// Draft saves write the Bcc header - it is the only place a draft
    /// records its blind-copy list, so reopening can restore it - while a
    /// send must never put it on the wire, where the header would leak every
    /// blind recipient's address to the To/Cc recipients and servers.
    #[test]
    fn draft_raw_includes_bcc_but_send_raw_does_not() {
        let mut msg = sample_message(None);
        msg.bcc = vec!["Ada <hidden@example.com>".to_string()];

        let (draft_raw, _, draft_recipients) = build_draft_raw_message(&msg);
        let draft = String::from_utf8(draft_raw).expect("raw message is valid UTF-8");
        assert!(
            draft.to_lowercase().contains("bcc:") && draft.contains("hidden@example.com"),
            "draft raw missing Bcc: {draft}"
        );
        assert!(draft_recipients.contains(&"Ada <hidden@example.com>".to_string()), "bcc stays in the envelope");

        let (send_raw, _, send_recipients) = build_raw_message(&msg);
        let sent = String::from_utf8(send_raw).expect("raw message is valid UTF-8");
        assert!(!sent.to_lowercase().contains("bcc:"), "send raw must not carry a Bcc header: {sent}");
        assert!(send_recipients.contains(&"Ada <hidden@example.com>".to_string()), "bcc still reaches the envelope");
    }

    fn sample_receipt() -> ReadReceipt {
        ReadReceipt {
            original_message_id: "<orig-1@example.com>".to_string(),
            original_from: "sender@example.com".to_string(),
            original_subject: "Hello there".to_string(),
            original_date: Some("Sun, 09 Aug 2026 10:00:00 +0200".to_string()),
            final_recipient: "me@example.com".to_string(),
            automatic: false,
            displayed_at: "Sun, 09 Aug 2026 12:30:00 +0200".to_string(),
            original_headers: vec![
                ("Message-ID".to_string(), "<orig-1@example.com>".to_string()),
                ("From".to_string(), "sender@example.com".to_string()),
                ("Subject".to_string(), "Hello there".to_string()),
            ],
        }
    }

    fn raw_to_string(msg: &ComposedMessage) -> String {
        let (raw, _, _) = build_raw_message(msg);
        String::from_utf8(raw).expect("raw message is valid UTF-8")
    }

    #[test]
    fn plain_text_only_message_has_no_html_part() {
        let raw = raw_to_string(&sample_message(None));
        assert!(!raw.to_lowercase().contains("multipart/alternative"));
        assert!(raw.contains("plain part"));
        assert!(!raw.contains("text/html"));
    }

    #[test]
    fn html_body_produces_multipart_alternative_with_both_parts() {
        let raw = raw_to_string(&sample_message(Some("<p>html <b>part</b></p>".to_string())));
        let lower = raw.to_lowercase();
        assert!(lower.contains("multipart/alternative"), "expected multipart/alternative in:\n{raw}");
        assert!(lower.contains("text/plain"));
        assert!(lower.contains("text/html"));
        assert!(raw.contains("<p>html <b>part</b></p>"));
        assert!(raw.contains("plain part"));
    }

    #[test]
    fn empty_html_body_is_skipped() {
        let raw = raw_to_string(&sample_message(Some(String::new())));
        assert!(!raw.to_lowercase().contains("text/html"));
        assert!(raw.contains("plain part"));
    }

    fn sample_attachment() -> Attachment {
        Attachment {
            filename: "report.pdf".to_string(),
            content_type: "application/pdf".to_string(),
            bytes: b"%PDF-fake".to_vec(),
        }
    }

    fn sample_inline_image() -> InlineImage {
        InlineImage {
            cid: "img-1@lookout.local".to_string(),
            content_type: "image/png".to_string(),
            bytes: vec![0x89, 0x50, 0x4e, 0x47],
        }
    }

    #[test]
    fn attachment_produces_multipart_mixed_with_content_disposition_and_filename() {
        let mut msg = sample_message(None);
        msg.attachments = vec![sample_attachment()];
        let raw = raw_to_string(&msg);
        let lower = raw.to_lowercase();
        assert!(lower.contains("multipart/mixed"), "raw:\n{raw}");
        assert!(lower.contains("content-disposition: attachment"), "raw:\n{raw}");
        assert!(lower.contains("filename=\"report.pdf\""), "raw:\n{raw}");
        assert!(lower.contains("application/pdf"), "raw:\n{raw}");
        assert!(raw.contains("plain part"));
    }

    #[test]
    fn inline_image_carries_content_id_and_inline_disposition() {
        let mut msg = sample_message(Some("<p>see <img src=\"cid:img-1@lookout.local\"></p>".to_string()));
        msg.inline_images = vec![sample_inline_image()];
        let raw = raw_to_string(&msg);
        let lower = raw.to_lowercase();
        assert!(lower.contains("content-disposition: inline"), "raw:\n{raw}");
        assert!(raw.contains("Content-ID: <img-1@lookout.local>"), "raw:\n{raw}");
        assert!(lower.contains("image/png"), "raw:\n{raw}");
    }

    #[test]
    fn text_html_attachment_and_inline_image_together_assemble_the_full_nested_structure() {
        let mut msg = sample_message(Some("<p>hi</p>".to_string()));
        msg.attachments = vec![sample_attachment()];
        msg.inline_images = vec![sample_inline_image()];
        let raw = raw_to_string(&msg);
        let lower = raw.to_lowercase();
        assert!(lower.contains("multipart/mixed"), "raw:\n{raw}");
        assert!(lower.contains("multipart/alternative"), "raw:\n{raw}");
        assert!(lower.contains("text/plain"));
        assert!(lower.contains("text/html"));
        assert!(lower.contains("content-disposition: inline"));
        assert!(lower.contains("filename=\"report.pdf\""));
    }

    #[test]
    fn no_attachments_or_inline_images_leaves_output_unchanged() {
        let raw = raw_to_string(&sample_message(Some("<p>x</p>".to_string())));
        assert!(!raw.to_lowercase().contains("multipart/mixed"), "raw:\n{raw}");
    }

    #[test]
    fn calendar_reply_ignores_attachments() {
        let mut msg = sample_message(None);
        msg.calendar_part = Some(imip_ics("REPLY"));
        msg.attachments = vec![sample_attachment()];
        let raw = raw_to_string(&msg);
        assert!(!raw.to_lowercase().contains("report.pdf"), "raw:\n{raw}");
    }

    #[test]
    fn pregenerated_message_id_is_used_verbatim() {
        let mut msg = sample_message(None);
        msg.message_id = Some("stable-draft-id@lookout.local".to_string());
        let (raw, message_id, _) = build_raw_message(&msg);
        let raw = String::from_utf8(raw).unwrap();
        assert_eq!(message_id, "stable-draft-id@lookout.local");
        assert!(raw.contains("Message-ID: <stable-draft-id@lookout.local>"), "raw message:\n{raw}");
    }

    #[test]
    fn generated_message_ids_are_unique() {
        assert_ne!(new_message_id(), new_message_id());
    }

    fn imip_ics(method: &str) -> String {
        format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\nMETHOD:{method}\r\nBEGIN:VEVENT\r\nUID:x@y\r\nDTSTAMP:20260101T000000Z\r\nDTSTART:20260715T140000Z\r\nDTEND:20260715T150000Z\r\nSUMMARY:Hi\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n")
    }

    /// An iMIP reply must be a `multipart/alternative` carrying the plain-text
    /// part first and the `text/calendar` part second, with the method echoed
    /// into the part's Content-Type parameter.
    #[test]
    fn calendar_part_produces_an_alternative_with_method_parameter() {
        let mut msg = sample_message(None);
        msg.calendar_part = Some(imip_ics("REPLY"));
        let raw = raw_to_string(&msg);
        let lower = raw.to_lowercase();
        assert!(lower.contains("multipart/alternative"), "expected alternative in:\n{raw}");
        // mail_builder writes Content-Type attributes with RFC 2047 quoting.
        assert!(lower.contains("text/calendar; method=\"reply\""), "raw:\n{raw}");
        assert!(raw.contains("plain part"));
        assert!(raw.contains("METHOD:REPLY"));
        // The HTML auto-assembly must not happen for iMIP payloads.
        assert!(!lower.contains("text/html"));
    }

    #[test]
    fn calendar_part_method_is_derived_from_the_document() {
        let mut msg = sample_message(None);
        msg.calendar_part = Some(imip_ics("REQUEST"));
        let raw = raw_to_string(&msg);
        assert!(raw.to_lowercase().contains("method=\"request\""), "raw:\n{raw}");
    }

    #[test]
    fn empty_calendar_part_is_skipped() {
        let msg = ComposedMessage {
            calendar_part: Some(String::new()),
            ..sample_message(None)
        };
        let raw = raw_to_string(&msg);
        assert!(!raw.to_lowercase().contains("text/calendar"), "raw:\n{raw}");
    }

    #[test]
    fn display_name_goes_into_the_from_header() {
        let mut msg = sample_message(None);
        msg.display_name = Some("Ada Lovelace".to_string());
        let raw = raw_to_string(&msg);
        // `mail_builder` RFC 2047-quotes display names containing spaces.
        assert!(raw.contains("From: \"Ada Lovelace\" <me@example.com>"), "raw:\n{raw}");
    }

    #[test]
    fn without_a_display_name_the_from_header_is_the_bare_address() {
        let raw = raw_to_string(&sample_message(None));
        assert!(raw.contains("From: <me@example.com>"), "raw:\n{raw}");
    }

    #[test]
    fn reply_to_is_emitted_when_set_and_skipped_when_empty() {
        let mut msg = sample_message(None);
        msg.reply_to = vec!["replies@example.com".to_string(), "alt@example.com".to_string()];
        let raw = raw_to_string(&msg);
        assert!(raw.contains("Reply-To: <replies@example.com>, <alt@example.com>"), "raw:\n{raw}");

        let raw = raw_to_string(&sample_message(None));
        assert!(!raw.to_lowercase().contains("reply-to"), "raw:\n{raw}");
    }

    #[test]
    fn importance_headers_are_emitted_for_high_and_low_but_not_normal() {
        // Normal is left off the wire, matching Outlook.
        let raw = raw_to_string(&sample_message(None));
        assert!(!raw.contains("Importance:"), "raw:\n{raw}");
        assert!(!raw.contains("X-Priority:"), "raw:\n{raw}");
        assert!(!raw.contains("X-MSMail-Priority:"), "raw:\n{raw}");

        let mut msg = sample_message(None);
        msg.importance = lookout_core::Importance::High;
        let raw = raw_to_string(&msg);
        assert!(raw.contains("Importance: High"), "raw:\n{raw}");
        assert!(raw.contains("X-Priority: 1 (Highest)"), "raw:\n{raw}");
        assert!(raw.contains("X-MSMail-Priority: High"), "raw:\n{raw}");

        msg.importance = lookout_core::Importance::Low;
        let raw = raw_to_string(&msg);
        assert!(raw.contains("Importance: Low"), "raw:\n{raw}");
        assert!(raw.contains("X-Priority: 5 (Lowest)"), "raw:\n{raw}");
        assert!(raw.contains("X-MSMail-Priority: Low"), "raw:\n{raw}");
    }

    #[test]
    fn split_mailbox_separates_display_name_from_address() {
        assert_eq!(split_mailbox("gavindi@gmail.com"), (None, "gavindi@gmail.com".to_string()));
        assert_eq!(
            split_mailbox("Gavin Graham <gavindi@gmail.com>"),
            (Some("Gavin Graham".to_string()), "gavindi@gmail.com".to_string())
        );
        assert_eq!(split_mailbox("  spaced@example.com  "), (None, "spaced@example.com".to_string()));
        assert_eq!(
            split_mailbox("\"Ada Lovelace\" <ada@example.com>"),
            (Some("Ada Lovelace".to_string()), "ada@example.com".to_string())
        );
    }

    #[test]
    fn to_header_carries_a_picked_contacts_display_name_and_stays_well_formed() {
        // A recipient picked from the address book arrives as a full
        // `Name <address>` token (see `RecipientEntry`/`address_of`) - it
        // must not get nested in a second pair of angle brackets, and the
        // SMTP envelope this drives must still parse to the bare address
        // rather than erroring "invalid recipient address".
        let mut msg = sample_message(None);
        msg.to = vec!["Gavin Graham <gavindi@gmail.com>".to_string()];
        let raw = raw_to_string(&msg);
        // `mail_builder` RFC 2047-quotes display names containing spaces,
        // same as the `From:` header (see `display_name_goes_into_the_from_header`).
        assert!(raw.contains("To: \"Gavin Graham\" <gavindi@gmail.com>"), "raw:\n{raw}");
        assert!(!raw.contains("<Gavin Graham"), "display name leaked inside the address angle brackets:\n{raw}");
        let (_, _, recipients) = build_raw_message(&msg);
        assert_eq!(recipients, vec!["Gavin Graham <gavindi@gmail.com>".to_string()]);
    }

    #[test]
    fn to_header_without_a_display_name_is_unchanged() {
        let raw = raw_to_string(&sample_message(None));
        assert!(raw.contains("To: <you@example.com>"), "raw:\n{raw}");
    }

    #[test]
    fn bcc_still_reaches_only_the_envelope() {
        let mut msg = sample_message(None);
        msg.bcc = vec!["hidden@example.com".to_string()];
        let (_, _, recipients) = build_raw_message(&msg);
        assert!(recipients.contains(&"hidden@example.com".to_string()));
        let raw = raw_to_string(&msg);
        assert!(!raw.to_lowercase().contains("bcc"), "bcc leaked into headers:\n{raw}");
    }

    #[test]
    fn request_read_receipt_emits_disposition_notification_to() {
        let mut msg = sample_message(None);
        msg.request_read_receipt = true;
        let raw = raw_to_string(&msg);
        assert!(raw.contains("Disposition-Notification-To: me@example.com"), "raw:\n{raw}");

        let raw = raw_to_string(&sample_message(None));
        assert!(!raw.to_lowercase().contains("disposition-notification-to"), "raw:\n{raw}");
    }

    /// A read receipt is a `multipart/report` with the report-type parameter,
    /// the three RFC 8098 §3.1 parts, and no HTML auto-assembly.
    #[test]
    fn read_receipt_produces_a_disposition_report() {
        let mut msg = sample_message(None);
        msg.subject = "Read receipt: Hello there".to_string();
        msg.in_reply_to = Some("orig-1@example.com".to_string());
        msg.references = vec!["orig-1@example.com".to_string()];
        msg.read_receipt = Some(sample_receipt());
        let raw = raw_to_string(&msg);
        let lower = raw.to_lowercase();
        assert!(lower.contains("multipart/report"), "expected a report in:\n{raw}");
        assert!(lower.contains("report-type=\"disposition-notification\""), "raw:\n{raw}");
        assert!(lower.contains("message/disposition-notification"), "raw:\n{raw}");
        assert!(lower.contains("message/rfc822"), "raw:\n{raw}");
        assert!(!lower.contains("text/html"), "raw:\n{raw}");
    }

    /// The report's fields must carry the disposition, both recipients, the
    /// original Message-ID, and the machine-generated marker (RFC 3834).
    #[test]
    fn read_receipt_carries_the_disposition_fields() {
        let mut msg = sample_message(None);
        msg.in_reply_to = Some("orig-1@example.com".to_string());
        msg.references = vec!["orig-1@example.com".to_string()];
        msg.read_receipt = Some(sample_receipt());
        let raw = raw_to_string(&msg);
        assert!(raw.contains("Disposition: manual-action/MDN-sent-manually; displayed"), "raw:\n{raw}");
        assert!(raw.contains("Final-Recipient: rfc822;<me@example.com>"), "raw:\n{raw}");
        assert!(raw.contains("Original-Recipient: rfc822;<me@example.com>"), "raw:\n{raw}");
        assert!(raw.contains("Original-Message-ID: <orig-1@example.com>"), "raw:\n{raw}");
        assert!(raw.contains("Received-Date: Sun, 09 Aug 2026 10:00:00 +0200"), "raw:\n{raw}");
        assert!(raw.contains("Reporting-UA: lookout/"), "raw:\n{raw}");
        assert!(raw.contains("Auto-Submitted: auto-generated"), "raw:\n{raw}");
        assert!(raw.contains("In-Reply-To: <orig-1@example.com>"), "raw:\n{raw}");
        // A receipt asks for no receipt of its own.
        assert!(!raw.to_lowercase().contains("disposition-notification-to"), "raw:\n{raw}");
    }

    /// An automatic receipt (the policy path) picks the automatic-action
    /// disposition mode; the third part carries the original headers.
    #[test]
    fn read_receipt_automatic_mode_and_original_headers_part() {
        let mut receipt = sample_receipt();
        receipt.automatic = true;
        let mut msg = sample_message(None);
        msg.read_receipt = Some(receipt);
        let raw = raw_to_string(&msg);
        assert!(raw.contains("Disposition: automatic-action/MDN-sent-automatically; displayed"), "raw:\n{raw}");
        assert!(raw.contains("Message-ID: <orig-1@example.com>"), "third part should re-emit the original headers:\n{raw}");
        assert!(raw.contains("From: sender@example.com"), "raw:\n{raw}");
    }

    /// A bare (unbracketed) original Message-ID is wrapped for the report.
    #[test]
    fn read_receipt_wraps_a_bare_original_message_id() {
        let mut receipt = sample_receipt();
        receipt.original_message_id = "bare-id@example.com".to_string();
        let mut msg = sample_message(None);
        msg.read_receipt = Some(receipt);
        let raw = raw_to_string(&msg);
        assert!(raw.contains("Original-Message-ID: <bare-id@example.com>"), "raw:\n{raw}");
    }
}
