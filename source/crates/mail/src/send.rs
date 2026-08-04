use lettre::address::Envelope;
use lettre::transport::smtp::authentication::{Credentials, Mechanism};
use lettre::{Address as SmtpAddress, AsyncSmtpTransport, AsyncTransport, Tokio1Executor};
use mail_builder::headers::address::Address as BuilderAddress;
use mail_builder::MessageBuilder;

use crate::config::{Credential, EndpointConfig};
use crate::error::{Error, Result};

/// A message the user has composed, ready to be built into a raw RFC 5322
/// document and sent. Deliberately simple for Phase 1: a single `From`
/// address (the account's own address - no sending-identity selection yet).
/// `text_body` is always set (the plain-text fallback); `html_body`, when
/// present, is emitted alongside it as a `multipart/alternative` so
/// HTML-capable clients get the rich version and everything else the text.
#[derive(Debug)]
pub struct ComposedMessage {
    pub from: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub text_body: String,
    /// Optional HTML rendering of the same body. Both parts are sent when
    /// set; recipients that only handle plain text fall back to `text_body`.
    pub html_body: Option<String>,
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
        .from(BuilderAddress::new_address(None::<String>, msg.from.clone()))
        .subject(msg.subject.clone())
        .text_body(msg.text_body.clone());
    // `mail_builder` turns a message with both text and html bodies into a
    // `multipart/alternative` (text/plain first, text/html second), so the
    // HTML body is strictly an enhancement over the plain text part.
    if let Some(html) = msg.html_body.as_deref().filter(|h| !h.is_empty()) {
        builder = builder.html_body(html.to_string());
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
            to: vec!["you@example.com".to_string()],
            cc: vec![],
            bcc: vec![],
            subject: "test".to_string(),
            text_body: "plain part".to_string(),
            html_body: html,
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
}
