use lookout_core::{BodyPart, EmailBody, Uid};
use mail_parser::{MessageParser, MimeHeaders};

/// Parses a raw RFC 5322 message (as returned by `UID FETCH ... BODY.PEEK[]`)
/// into an [`EmailBody`]. Phase 1 fetches the whole message rather than
/// doing `BODYSTRUCTURE`-driven partial fetches of individual MIME parts -
/// simpler and correct for typical message sizes; streaming/partial fetch
/// for large attachments is a Phase 2 refinement (see the crate's module docs).
pub fn parse_body(uid: Uid, raw: &[u8]) -> Option<EmailBody> {
    let message = MessageParser::default().parse(raw)?;

    let text_body = message.body_text(0).map(|c| c.into_owned());
    let html_body = message.body_html(0).map(|c| c.into_owned());

    let headers = message
        .headers_raw()
        .map(|(name, value)| (name.to_string(), value.trim().to_string()))
        .collect();

    let parts = message
        .attachments()
        .enumerate()
        .map(|(i, part)| {
            let content_type = part
                .content_type()
                .map(|ct| match ct.subtype() {
                    Some(sub) => format!("{}/{sub}", ct.ctype()),
                    None => ct.ctype().to_string(),
                })
                .unwrap_or_else(|| "application/octet-stream".to_string());
            BodyPart {
                part_number: i.to_string(),
                content_type,
                filename: part.attachment_name().map(|s| s.to_string()),
                cid: part.content_id().map(|s| s.to_string()),
                size: part.contents().len() as u32,
                is_attachment: true,
            }
        })
        .collect();

    Some(EmailBody { uid, text_body, html_body, parts, headers, auth_results: None })
}
