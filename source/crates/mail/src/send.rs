use lettre::address::Envelope;
use lettre::transport::smtp::authentication::{Credentials, Mechanism};
use lettre::{Address as SmtpAddress, AsyncSmtpTransport, AsyncTransport, Tokio1Executor};
use mail_builder::headers::address::Address as BuilderAddress;
use mail_builder::headers::content_type::ContentType;
use mail_builder::mime::{BodyPart as MimeBodyPart, MimePart};
use mail_builder::MessageBuilder;

use crate::config::{Credential, EndpointConfig};
use crate::error::{Error, Result};

/// A message the user has composed, ready to be built into a raw RFC 5322
/// document and sent. `from` is the sending identity's address (an account
/// can send as several identities); `display_name`, when set, goes into the
/// `From:` header alongside it. `text_body` is always set (the plain-text
/// fallback); `html_body`, when present, is emitted alongside it as a
/// `multipart/alternative` so HTML-capable clients get the rich version and
/// everything else the text.
#[derive(Debug)]
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
    pub text_body: String,
    /// Optional HTML rendering of the same body. Both parts are sent when
    /// set; recipients that only handle plain text fall back to `text_body`.
    pub html_body: Option<String>,
    /// An RFC 6047 iMIP payload: the raw iCalendar document (which itself
    /// carries its `METHOD` - see `lookout-dav`'s `build_imip_vcalendar`).
    /// When set, the message is built as `multipart/alternative`
    /// [text/plain, text/calendar; method=...] instead of the text/html
    /// form - a calendar reply has nothing to gain from an HTML rendering.
    pub calendar_part: Option<String>,
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

/// Generates a fresh globally-unique bare `Message-ID` (no angle brackets -
/// `mail_builder`'s header writer adds them).
pub fn new_message_id() -> String {
    format!("{}@lookout.local", uuid::Uuid::new_v4())
}

/// Builds the raw RFC 5322 message bytes for `msg`, generating a fresh
/// `Message-ID` unless the message carries a pre-generated one (drafts, see
/// `ComposedMessage::message_id`). Returns the bytes plus the Message-ID in
/// use (the caller may want it for thread-tracking) and the flat recipient
/// list (to/cc/bcc combined) needed for the SMTP envelope, since MIME
/// headers alone aren't necessarily what the envelope's `RCPT TO` list
/// should be (bcc must be in the envelope but never in a header).
pub fn build_raw_message(msg: &ComposedMessage) -> (Vec<u8>, String, Vec<String>) {
    let message_id = msg.message_id.clone().unwrap_or_else(new_message_id);

    let mut builder = MessageBuilder::new()
        .message_id(message_id.clone())
        .from(BuilderAddress::new_address(msg.display_name.clone(), msg.from.clone()))
        .subject(msg.subject.clone());
    if let Some(ics) = msg.calendar_part.as_deref().filter(|ics| !ics.is_empty()) {
        // iMIP payload: `multipart/alternative` [text/plain, text/calendar]
        // per RFC 6047 §3.3. The `method=` Content-Type parameter tells the
        // recipient's calendar client how to treat the payload; it is derived
        // from the document's own `METHOD` property, which the
        // `build_imip_vcalendar` caller has already set.
        let method = match lookout_core::parse_imip_method(ics) {
            lookout_core::ImipMethod::Request => "REQUEST",
            lookout_core::ImipMethod::Reply => "REPLY",
            lookout_core::ImipMethod::Cancel => "CANCEL",
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
    }

    if !msg.to.is_empty() {
        builder = builder.to(BuilderAddress::new_list(
            msg.to.iter().map(|a| BuilderAddress::new_address(None::<String>, a.clone())).collect(),
        ));
    }
    if !msg.cc.is_empty() {
        builder = builder.cc(BuilderAddress::new_list(
            msg.cc.iter().map(|a| BuilderAddress::new_address(None::<String>, a.clone())).collect(),
        ));
    }
    if !msg.reply_to.is_empty() {
        builder = builder.reply_to(BuilderAddress::new_list(
            msg.reply_to.iter().map(|a| BuilderAddress::new_address(None::<String>, a.clone())).collect(),
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
    let to_addrs: Vec<SmtpAddress> = recipients
        .iter()
        .map(|r| r.parse::<SmtpAddress>().map_err(|_| Error::LoginFailed(format!("invalid recipient address: {r}"))))
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
            calendar_part: None,
            in_reply_to: None,
            references: vec![],
            message_id: None,
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
    fn bcc_still_reaches_only_the_envelope() {
        let mut msg = sample_message(None);
        msg.bcc = vec!["hidden@example.com".to_string()];
        let (_, _, recipients) = build_raw_message(&msg);
        assert!(recipients.contains(&"hidden@example.com".to_string()));
        let raw = raw_to_string(&msg);
        assert!(!raw.to_lowercase().contains("bcc"), "bcc leaked into headers:\n{raw}");
    }
}
