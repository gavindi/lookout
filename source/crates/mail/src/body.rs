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

/// The longest snippet kept for a list row.
///
/// The row shows one ellipsized line and lets Pango cut it at whatever the
/// pane's current width allows - so this cap exists only to bound what gets
/// cached and serialized, and is set well past what even a maximised
/// full-width message pane can display. Trimming closer to the visible
/// length would leave a wide pane's line ending early in whitespace instead
/// of running to the edge.
const PREVIEW_MAX_CHARS: usize = 500;

/// Extracts the one-line snippet the message list shows under each row, from
/// the *leading bytes* of a raw message (`BODY.PEEK[]<0.N>`).
///
/// Tolerates truncated input by design: the caller deliberately fetches only
/// a prefix, so the closing MIME boundary is usually missing. `mail_parser`
/// is lenient enough to still surface the first text part, and a message it
/// can't make sense of just yields `None` and renders a blank preview line.
pub fn preview_from_raw(raw: &[u8]) -> Option<String> {
    let message = MessageParser::default().parse(raw)?;
    let text = message.body_text(0).map(|c| c.into_owned()).or_else(|| message.body_html(0).map(|c| strip_html(&c)))?;
    let preview = normalize_preview(&text);
    (!preview.is_empty()).then_some(preview)
}

/// Crude tag-stripper for the HTML fallback - enough to turn markup into
/// readable words, not a sanitizer. The reading pane renders real HTML
/// through WebKit; this is only ever a list row's worth of plain text.
fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// `strip_html`, exposed for the search index's whole-body indexing (a full
/// HTML message needs its text made searchable the same way a preview is).
pub fn strip_html_for_index(html: &str) -> String {
    strip_html(html)
}

/// Collapses a body's leading text into a single display line.
fn normalize_preview(text: &str) -> String {
    // Zero-width characters first: bulk senders pad the top of a message
    // with runs of them as "preheader" spacing, and left in place they'd
    // make the preview render as an apparently empty line.
    let cleaned: String = text.chars().filter(|c| !matches!(c, '\u{200b}' | '\u{200c}' | '\u{200d}' | '\u{feff}')).collect();
    let collapsed = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    match collapsed.char_indices().nth(PREVIEW_MAX_CHARS) {
        // Slice on a char boundary - a byte-index truncation would panic on
        // any multi-byte character straddling the cut.
        Some((byte_index, _)) => collapsed[..byte_index].trim_end().to_string(),
        None => collapsed,
    }
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

    #[test]
    fn preview_reads_a_single_line_from_a_plain_text_message() {
        let preview = preview_from_raw(&fixture("plain-text.eml")).expect("has a preview");
        assert!(!preview.is_empty());
        assert!(!preview.contains('\n'));
        assert!(preview.chars().count() <= PREVIEW_MAX_CHARS);
    }

    #[test]
    fn preview_falls_back_to_stripped_html() {
        let preview = preview_from_raw(&fixture("html-inline-css.eml")).expect("has a preview");
        assert!(!preview.contains('<'));
        assert!(!preview.contains("</"));
    }

    /// The fetch is deliberately a byte-prefix, so the closing MIME boundary
    /// is normally missing. That must degrade to a shorter (or absent)
    /// preview, never a panic.
    #[test]
    fn preview_survives_a_truncated_fetch() {
        let full = fixture("html-inline-css.eml");
        for cut in [64, 256, 1024, 4096] {
            let truncated = &full[..cut.min(full.len())];
            let _ = preview_from_raw(truncated);
        }
    }

    #[test]
    fn preview_collapses_whitespace_and_drops_preheader_padding() {
        let padded = "\u{200b}\u{200b}\u{feff}  Hello   there\n\n\tworld  ";
        assert_eq!(normalize_preview(padded), "Hello there world");
    }

    #[test]
    fn preview_truncates_on_a_character_boundary() {
        // Multi-byte characters straddling the cut would panic a byte slice.
        let long = "é".repeat(PREVIEW_MAX_CHARS + 50);
        let preview = normalize_preview(&long);
        assert_eq!(preview.chars().count(), PREVIEW_MAX_CHARS);
    }

    #[test]
    fn empty_body_yields_no_preview() {
        assert_eq!(normalize_preview("   \u{200b} \n "), "");
        assert!(preview_from_raw(b"").is_none());
    }
}
