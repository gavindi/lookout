use lookout_core::{BodyPart, EmailBody, Uid};
use mail_parser::{MessageParser, MimeHeaders};

/// Parses a raw RFC 5322 message (as returned by `UID FETCH ... BODY.PEEK[]`)
/// into an [`EmailBody`]. This is the *fallback* body path, used when a
/// message's summary carries no `BODYSTRUCTURE`-derived part structure (the
/// normal path is `assemble_body_from_parts`, which downloads only the text
/// parts by number and never fetches attachments).
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
                charset: part.content_type().and_then(|ct| ct.attribute("charset")).map(str::to_string),
                transfer_encoding: part.content_transfer_encoding().map(str::to_string),
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

/// Parses the raw bytes of a `BODY.PEEK[HEADER]` section into the
/// `(name, value)` pairs `EmailBody::headers` is expected to hold - the same
/// shape `parse_body`'s whole-message path produces via `headers_raw`.
pub fn parse_headers_section(raw: &[u8]) -> Vec<(String, String)> {
    MessageParser::default()
        .parse_headers(raw)
        .map(|m| m.headers_raw().map(|(name, value)| (name.to_string(), value.trim().to_string())).collect())
        .unwrap_or_default()
}

/// Reassembles an [`EmailBody`] from the partial-fetch pieces: the raw
/// `BODY.PEEK[HEADER]` bytes and the raw (still transfer-encoded) bytes of
/// the message's text parts, each fetched by its `BODY[<part>]` section path.
/// `all_parts` is the message's full `BODYSTRUCTURE`-derived part list and
/// becomes `EmailBody::parts` minus the text leaves (attachments and inline
/// images only, matching `parse_body`'s attachment-only list).
///
/// Each text part is decoded the way `parse_body` decodes a whole message:
/// wrapped in a minimal single-part message carrying the part's own
/// Content-Type/charset/transfer-encoding (taken from the body structure) and
/// run through `mail_parser`, so its robust transfer-decoding and charset
/// handling are reused rather than reimplemented.
pub fn assemble_body_from_parts(uid: Uid, headers: Vec<(String, String)>, all_parts: &[BodyPart], fetched: &[(String, Vec<u8>)]) -> EmailBody {
    // Per part: (is_html, decoded-text-body, decoded-html-body). A text/plain
    // part yields both (the html half is mail_parser's synthesized rendering,
    // same as the whole-message path); a text/html part yields both too (the
    // text half is its converted-to-text rendering).
    let mut decoded: Vec<(bool, Option<String>, Option<String>)> = Vec::new();
    for (part_number, bytes) in fetched {
        let Some(part) = all_parts.iter().find(|p| p.part_number == *part_number) else {
            continue;
        };
        if !part.is_text() {
            continue;
        }
        let (text, html) = decode_text_part(part, bytes);
        decoded.push((part.content_type == "text/html", text, html));
    }
    // Mirrors the whole-message path's alternative handling: the visible text
    // is the first plain part's text (or the first html part's converted text
    // when there's no plain part), and the visible html is the first real
    // html part (or a plain part's synthesized rendering when there's none).
    let text_body = decoded
        .iter()
        .find(|(is_html, text, _)| !*is_html && text.is_some())
        .and_then(|(_, text, _)| text.clone())
        .or_else(|| decoded.iter().find_map(|(_, text, _)| text.clone()));
    let html_body = decoded
        .iter()
        .find(|(is_html, _, html)| *is_html && html.is_some())
        .and_then(|(_, _, html)| html.clone())
        .or_else(|| decoded.iter().find_map(|(_, _, html)| html.clone()));

    EmailBody {
        uid,
        text_body,
        html_body,
        parts: all_parts.iter().filter(|p| !p.is_text()).cloned().collect(),
        headers,
        auth_results: None,
    }
}

/// Decodes one text part's raw bytes (as returned by `BODY.PEEK[<part>]`)
/// into its text and html renderings. The part is wrapped in a minimal
/// single-part message - `Content-Type` (with its charset parameter) and
/// `Content-Transfer-Encoding` from the body structure - and parsed with
/// `mail_parser`, which handles base64/quoted-printable decoding and charset
/// conversion. Returns `(text, html)` in `mail_parser`'s sense: the html half
/// of a plain part is its synthesized rendering, the text half of an html
/// part is its converted text.
fn decode_text_part(part: &BodyPart, bytes: &[u8]) -> (Option<String>, Option<String>) {
    let mut headers = String::from("Content-Type: ");
    headers.push_str(&part.content_type);
    if let Some(charset) = &part.charset {
        headers.push_str(&format!("; charset={charset}"));
    }
    if let Some(encoding) = &part.transfer_encoding {
        headers.push_str(&format!("\r\nContent-Transfer-Encoding: {encoding}"));
    }
    let mut raw = headers.into_bytes();
    raw.extend_from_slice(b"\r\n\r\n");
    raw.extend_from_slice(bytes);
    let Some(message) = MessageParser::default().parse(&raw) else { return (None, None) };
    (message.body_text(0).map(|c| c.into_owned()), message.body_html(0).map(|c| c.into_owned()))
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

    /// Splits a fixture's raw bytes into (header block, body) at the first
    /// blank line, whatever the line ending style.
    fn split_fixture(name: &str) -> (Vec<u8>, Vec<u8>) {
        let raw = fixture(name);
        let sep = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .or_else(|| raw.windows(2).position(|w| w == b"\n\n"))
            .expect("fixture has a header/body separator");
        let at = if raw[sep..].starts_with(b"\r\n\r\n") { sep + 4 } else { sep + 2 };
        (raw[..sep].to_vec(), raw[at..].to_vec())
    }

    fn text_part(part_number: &str, content_type: &str, charset: Option<&str>, transfer_encoding: &str) -> BodyPart {
        BodyPart {
            part_number: part_number.to_string(),
            content_type: content_type.to_string(),
            charset: charset.map(str::to_string),
            transfer_encoding: Some(transfer_encoding.to_string()),
            filename: None,
            cid: None,
            size: 0,
            is_attachment: false,
        }
    }

    /// The partial-fetch assembly must produce the same body text as parsing
    /// the whole message: the plain-text fixture is a single 7bit text/plain
    /// part, exactly what `BODY.PEEK[1]` would return.
    #[test]
    fn assembling_the_plain_text_fixture_matches_whole_message_parsing() {
        let full = fixture("plain-text.eml");
        let (headers, body) = split_fixture("plain-text.eml");
        let whole = parse_body(Uid(7), &full).expect("whole message parses");

        let part = text_part("1", "text/plain", Some("utf-8"), "7bit");
        let parts = vec![part.clone()];
        let assembled = assemble_body_from_parts(Uid(7), parse_headers_section(&headers), &parts, &[("1".to_string(), body)]);
        assert_eq!(assembled.text_body, whole.text_body);
        assert_eq!(assembled.html_body, whole.html_body);
        assert_eq!(assembled.headers, whole.headers);
    }

    /// Same parity check for the HTML-only fixture (a single 7bit text/html
    /// part). The html must come through verbatim, and the body must still
    /// render from the html half even though there's no plain part.
    #[test]
    fn assembling_the_html_fixture_matches_whole_message_parsing() {
        let full = fixture("html-inline-css.eml");
        let (headers, body) = split_fixture("html-inline-css.eml");
        let whole = parse_body(Uid(8), &full).expect("whole message parses");

        let part = text_part("1", "text/html", Some("utf-8"), "7bit");
        let assembled = assemble_body_from_parts(Uid(8), parse_headers_section(&headers), &[part], &[("1".to_string(), body)]);
        assert_eq!(assembled.text_body, whole.text_body);
        assert_eq!(assembled.html_body, whole.html_body);
    }

    #[test]
    fn assembles_a_base64_utf8_plain_part() {
        use base64::Engine;
        let text = "Hello Caffè ☕";
        let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
        let part = text_part("1", "text/plain", Some("utf-8"), "base64");
        let body = assemble_body_from_parts(Uid(1), vec![("Subject".to_string(), "hi".to_string())], &[part], &[("1".to_string(), encoded.into_bytes())]);
        assert_eq!(body.text_body.as_deref(), Some(text));
        // A plain-only message still yields an html rendering (mail_parser's
        // synthesized fallback), matching the whole-message path.
        assert!(body.html_body.is_some());
    }

    #[test]
    fn assembles_a_quoted_printable_latin1_html_part() {
        // "café <b>vitrine</b>" in ISO-8859-1, quoted-printable encoded.
        let raw = b"caf=E9 <b>vitrine</b>".to_vec();
        let part = text_part("1", "text/html", Some("iso-8859-1"), "quoted-printable");
        let body = assemble_body_from_parts(Uid(2), Vec::new(), &[part], &[("1".to_string(), raw)]);
        assert_eq!(body.html_body.as_deref(), Some("café <b>vitrine</b>"));
    }

    /// multipart/alternative [text/plain (1), text/html (2)]: the html body
    /// must be the *real* html part, not the plain part's synthesized
    /// rendering - the exact failure a naive "first non-empty html wins" loop
    /// would produce.
    #[test]
    fn alternative_keeps_the_real_html_over_a_synthesized_rendering() {
        use base64::Engine;
        let plain = base64::engine::general_purpose::STANDARD.encode("Hello in plain text".as_bytes());
        let html = b"<p>Hello in <b>html</b></p>".to_vec();
        let parts = vec![text_part("1", "text/plain", Some("utf-8"), "base64"), text_part("2", "text/html", Some("utf-8"), "7bit")];
        let body = assemble_body_from_parts(Uid(3), Vec::new(), &parts, &[("1".to_string(), plain.into_bytes()), ("2".to_string(), html)]);
        assert_eq!(body.text_body.as_deref(), Some("Hello in plain text"));
        assert_eq!(body.html_body.as_deref(), Some("<p>Hello in <b>html</b></p>"));
    }

    /// Only the text parts' bytes are fetched; `EmailBody::parts` must still
    /// describe the attachments (metadata from the body structure), exactly
    /// like the whole-message path's attachment list.
    #[test]
    fn assembled_parts_list_is_the_attachments_only() {
        use base64::Engine;
        let plain = base64::engine::general_purpose::STANDARD.encode("Body text".as_bytes());
        let mut attachment = text_part("2", "application/pdf", None, "base64");
        attachment.is_attachment = true;
        attachment.filename = Some("doc.pdf".to_string());
        let all_parts = vec![text_part("1", "text/plain", Some("utf-8"), "base64"), attachment.clone()];
        let body = assemble_body_from_parts(Uid(4), Vec::new(), &all_parts, &[("1".to_string(), plain.into_bytes())]);
        assert_eq!(body.parts, vec![attachment]);
    }

    #[test]
    fn header_section_parses_to_name_value_pairs() {
        let raw = b"From: a@b.c\r\nReferences: <x@y.z>\r\nX-Custom:  padded  \r\n";
        let headers = parse_headers_section(raw);
        assert_eq!(
            headers,
            vec![
                ("From".to_string(), "a@b.c".to_string()),
                ("References".to_string(), "<x@y.z>".to_string()),
                ("X-Custom".to_string(), "padded".to_string())
            ]
        );
    }

    #[test]
    fn empty_or_garbage_header_section_degrades_gracefully() {
        // An empty section is a clean "no headers"; garbage bytes must be
        // tolerated (mail_parser is deliberately lenient) rather than panic.
        assert!(parse_headers_section(b"").is_empty());
        let _ = parse_headers_section(b"\x00\x01\x02");
    }
}
