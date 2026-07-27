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

    let headers = message.headers_raw().map(|(name, value)| (name.to_string(), value.trim().to_string())).collect();

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

    Some(EmailBody {
        uid,
        text_body,
        html_body,
        parts,
        headers,
        auth_results: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<u8> {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-fixtures/").to_string() + name;
        std::fs::read(&path).unwrap_or_else(|e| panic!("reading fixture {path}: {e}"))
    }

    #[test]
    fn plain_text_fixture_has_text_body() {
        let body = parse_body(Uid(0), &fixture("plain-text.eml")).expect("parses");
        assert!(body.text_body.is_some());
        // Note: mail_parser's body_html(0) synthesizes an HTML rendering of
        // the plain-text body as a convenience fallback (wraps it in
        // <html><body>...<br/> tags), so html_body is Some here too - a
        // genuinely plain-text-only message doesn't actually leave
        // html_body unset. render_body() in the app crate prefers
        // html_body when present, so this fixture in practice renders via
        // the WebKit view, not the Gtk.TextView fallback path.
    }

    #[test]
    fn html_inline_css_fixture_parses_html() {
        let body = parse_body(Uid(0), &fixture("html-inline-css.eml")).expect("parses");
        let html = body.html_body.expect("has html body");
        assert!(html.contains("highlight"));
    }

    #[test]
    fn html_cid_image_fixture_parses_without_panicking() {
        let body = parse_body(Uid(0), &fixture("html-cid-image.eml")).expect("parses");
        assert!(body.html_body.expect("has html body").contains("cid:logo123"));
        // Documents current behavior for the still-open "inline cid: image
        // resolution" TODO item, rather than asserting a specific outcome:
        // mail_parser's `attachments()` may or may not surface a
        // multipart/related inline part as a `BodyPart` with `cid` set.
        let _ = body.parts.iter().find(|p| p.cid.as_deref() == Some("logo123"));
    }

    #[test]
    fn html_external_image_fixture_parses_html() {
        let body = parse_body(Uid(0), &fixture("html-external-image.eml")).expect("parses");
        assert!(body.html_body.expect("has html body").contains("example.com/tracker.png"));
    }

    #[test]
    fn malformed_html_fixture_does_not_panic() {
        let body = parse_body(Uid(0), &fixture("html-malformed.eml")).expect("parses");
        assert!(body.html_body.is_some());
    }
}
