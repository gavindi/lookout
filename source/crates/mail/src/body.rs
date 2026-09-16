/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use base64::Engine;
use lookout_core::{BodyPart, EmailBody, Uid};
use mail_parser::{Message, MessageParser, MimeHeaders, PartType};
use std::collections::HashMap;

/// Parses a raw RFC 5322 message (as returned by `UID FETCH ... BODY.PEEK[]`)
/// into an [`EmailBody`]. This is the *fallback* body path, used when a
/// message's summary carries no `BODYSTRUCTURE`-derived part structure (the
/// normal path is `assemble_body_from_parts`, which downloads only the text
/// parts by number and never fetches attachments).
pub fn parse_body(uid: Uid, raw: &[u8]) -> Option<EmailBody> {
    let message = MessageParser::default().parse(raw)?;

    let text_body = message.body_text(0).map(|c| c.into_owned());
    // `body_html(0)` returns either a genuine `text/html` part or, for a
    // plain-text-only message, mail_parser's own bare-bones synthesis of the
    // text part (escapes `<`, turns `\n` into `<br/>`, never linkifies).
    // Telling the two apart by inspecting the part itself picks the matching
    // linkifier: a genuine part's own markup must stay intact except for
    // bare links in its text nodes (`linkify_html_text_nodes`), while a
    // plain-text-only message is free to be rebuilt from scratch
    // (`linkify_to_html`) since there's no markup to preserve.
    let html_body = match message.html_part(0).map(|p| &p.body) {
        Some(PartType::Html(html)) => Some(linkify_html_text_nodes(html.as_ref())),
        _ => text_body.as_deref().map(linkify_to_html),
    };

    // The iMIP payload: the first `text/calendar` leaf in the part tree
    // (invitations carry exactly one). `part.contents()` is already
    // transfer-decoded by mail_parser, so the document is captured as-is.
    let calendar_ics = message.parts.iter().find_map(|part| {
        let is_calendar = part
            .content_type()
            .map(|ct| format!("{}/{}", ct.ctype(), ct.subtype().unwrap_or("")).eq_ignore_ascii_case("text/calendar"))
            .unwrap_or(false);
        is_calendar.then(|| String::from_utf8_lossy(part.contents()).into_owned())
    });

    let headers = message.headers_raw().map(|(name, value)| (name.to_string(), value.trim().to_string())).collect();

    // mail-parser numbers its parts by flat index, not by IMAP section path;
    // compute the `BODY[<n>]` path for every part so `EmailBody::parts`
    // carries numbers an on-demand `FetchAttachment` can actually fetch with.
    let paths = part_paths(&message);
    let parts = message
        .attachments
        .iter()
        .filter_map(|id| {
            let part = message.parts.get(*id as usize)?;
            let part_number = paths.get(id)?.clone();
            let content_type = part
                .content_type()
                .map(|ct| match ct.subtype() {
                    Some(sub) => format!("{}/{sub}", ct.ctype()),
                    None => ct.ctype().to_string(),
                })
                .unwrap_or_else(|| "application/octet-stream".to_string())
                .to_ascii_lowercase();
            // The iMIP payload is body content, not an attachment: it never
            // surfaces in the strip's metadata list (matching the
            // partial-fetch assembly path's filter below).
            if content_type.eq_ignore_ascii_case("text/calendar") {
                return None;
            }
            // Same heuristic as the BODYSTRUCTURE path (`structure.rs`): a
            // part is an attachment when it declares `Content-Disposition:
            // attachment`, or when it carries a filename without an explicit
            // `inline` disposition. An inline `cid:` image - even one with a
            // filename, as senders commonly emit - stays in the HTML body and
            // must not surface in the attachment strip.
            let disposition = part.content_disposition();
            let is_attachment = disposition.as_ref().is_some_and(|d| d.is_attachment()) || part.attachment_name().is_some() && disposition.as_ref().is_none_or(|d| !d.is_inline());
            Some(BodyPart {
                part_number,
                content_type,
                charset: part.content_type().and_then(|ct| ct.attribute("charset")).map(str::to_string),
                transfer_encoding: part.content_transfer_encoding().map(str::to_string),
                filename: part.attachment_name().map(|s| s.to_string()),
                cid: part.content_id().map(|s| s.to_string()),
                size: part.contents().len() as u32,
                is_attachment,
            })
        })
        .collect();

    Some(EmailBody {
        uid,
        text_body,
        html_body,
        calendar_ics,
        parts,
        headers,
        auth_results: None,
    })
}

/// Finds the part of a raw RFC 5322 message that corresponds to a requested
/// part whose `BODYSTRUCTURE`-derived part number went stale - the message
/// was rewritten server-side after its body or summary was cached
/// (mailing-list/gateway/proxy rewrites), so the cached structure's section
/// paths no longer match the live message while the parts themselves are
/// still there. Matches by `cid` when the request carries one (inline
/// images: a Content-ID survives a re-wrap), else by filename, else by
/// content type, and returns the part's transfer-decoded bytes - the same
/// form `transfer_part_bytes` produces for a fresh section fetch. `None`
/// when no part matches.
pub fn find_part_in_raw(raw: &[u8], requested: &BodyPart) -> Option<Vec<u8>> {
    let message = MessageParser::default().parse(raw)?;
    let found = message.attachments().find(|part| match requested.cid.as_deref() {
        Some(cid) => part.content_id().is_some_and(|candidate| lookout_core::cid_matches(cid, candidate)),
        None => match requested.filename.as_deref() {
            Some(name) => part.attachment_name().is_some_and(|candidate| candidate.eq_ignore_ascii_case(name)),
            None => part.content_type().is_some_and(|candidate| {
                let candidate_type = match candidate.subtype() {
                    Some(sub) => format!("{}/{}", candidate.ctype(), sub),
                    None => candidate.ctype().to_string(),
                };
                candidate_type.eq_ignore_ascii_case(&requested.content_type)
            }),
        },
    })?;
    Some(found.contents().to_vec())
}

/// Dev-only helper for the debug ".eml viewer": rewrites every `cid:` image
/// reference in `html` to a `data:` URI carrying the referenced part's
/// already-transfer-decoded bytes from `raw`. The debug viewer has no
/// account session behind it, so the reading pane's scheme handler (which
/// fetches cid parts over IMAP) can't serve these - this lets a fixture be
/// verified in-app without a server. Compiled out of release builds.
#[cfg(debug_assertions)]
pub fn rewrite_cid_refs_to_data_uris(html: &str, raw: &[u8]) -> String {
    use mail_parser::MimeHeaders;
    let Some(message) = MessageParser::default().parse(raw) else { return html.to_string() };
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(start) = rest.find("cid:") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 4..];
        // The reference runs until whitespace or an HTML delimiter - enough
        // for a dev-only helper (a robust parser would be overkill here).
        let end = after.find(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == '>').unwrap_or(after.len());
        let reference = &after[..end];
        let resolved = message
            .attachments()
            .find(|p| p.content_id().is_some_and(|cid| lookout_core::cid_matches(reference, cid)))
            .map(|part| {
                let content_type = part
                    .content_type()
                    .map(|ct| match ct.subtype() {
                        Some(sub) => format!("{}/{sub}", ct.ctype()),
                        None => ct.ctype().to_string(),
                    })
                    .unwrap_or_else(|| "application/octet-stream".to_string());
                let data = base64::engine::general_purpose::STANDARD.encode(part.contents());
                format!("data:{content_type};base64,{data}")
            });
        match resolved {
            Some(data_uri) => out.push_str(&data_uri),
            // Unresolvable reference: leave the original `cid:` text in
            // place, the way the sandboxed viewer renders a missing image.
            None => out.push_str(&rest[start..start + 4 + end]),
        }
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

/// Computes every MIME part's RFC 3501 section path - the `BODY[<n>]` number
/// a partial fetch must target - by walking the parsed message's part tree the
/// same way `structure.rs` flattens a `BODYSTRUCTURE` response:
///
/// - A root single part is section `1`.
/// - A multipart's children are numbered `1..N` in document order; the
///   multipart wrapper itself is never a fetchable section.
/// - A `message/rfc822` part is a leaf (an attached email is fetched whole,
///   never descended into).
///
/// Returns a map from mail-parser's flat part index to the dotted path, e.g.
/// `"2"` or `"1.3"`.
//
#[allow(dead_code)]
fn part_paths(message: &Message<'_>) -> HashMap<u32, String> {
    fn walk(message: &Message<'_>, part_id: u32, prefix: &mut Vec<u32>, out: &mut HashMap<u32, String>) {
        let Some(part) = message.parts.get(part_id as usize) else { return };
        if let PartType::Multipart(children) = &part.body {
            for (i, child) in children.iter().enumerate() {
                prefix.push(i as u32 + 1);
                walk(message, *child, prefix, out);
                // The child's own number must be popped again before the next
                // sibling - `truncate`-style cleanup here would also pop the
                // *parent's* number for this branch.
                prefix.pop();
            }
        } else {
            // A leaf (text, binary, or an embedded message - never descended
            // into). A root single part is section "1".
            if prefix.is_empty() {
                prefix.push(1);
            }
            out.insert(part_id, prefix.iter().map(u32::to_string).collect::<Vec<_>>().join("."));
            prefix.pop();
        }
    }
    let mut out = HashMap::new();
    walk(message, 0, &mut Vec::new(), &mut out);
    out
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
    // The message's iMIP payload (at most one `text/calendar` part). Its bytes
    // are transfer-decoded with the same decoder the attachment path uses and
    // kept as-is - iCalendar is already text, so no charset conversion.
    let mut calendar_ics: Option<String> = None;
    for (part_number, bytes) in fetched {
        let Some(part) = all_parts.iter().find(|p| p.part_number == *part_number) else {
            continue;
        };
        if part.is_calendar() {
            calendar_ics = Some(String::from_utf8_lossy(&transfer_part_bytes(part, bytes)).into_owned());
            continue;
        }
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
        calendar_ics,
        // The calendar part is body content, not an attachment - it must not
        // surface in the strip's metadata list alongside the actual
        // attachments (same treatment as the text leaves).
        parts: all_parts.iter().filter(|p| !p.is_text() && !p.is_calendar()).cloned().collect(),
        headers,
        auth_results: None,
    }
}

/// One piece of `scan_links`'s walk over a text run: a literal stretch with
/// no link in it, a matched URL, or a matched email address (still just the
/// bare address - turning it into a `mailto:` href is the caller's job).
enum Segment<'a> {
    Plain(&'a str),
    Url(&'a str),
    Email(&'a str),
}

/// Shared trigger-detection walk (`match_url`/`match_email`) over `text`,
/// reporting each piece to `on_segment` in order. Escaping is entirely the
/// caller's job via `on_segment` - this just finds the boundaries, since the
/// two callers need opposite treatment of the text: `linkify_to_html` is
/// building HTML from raw plain text and must escape everything as it goes,
/// while `linkify_html_text_nodes` is walking text nodes that are already
/// valid (already-escaped) HTML and must copy them through verbatim.
fn scan_links<'a>(text: &'a str, mut on_segment: impl FnMut(Segment<'a>)) {
    let mut plain_start = 0usize;
    let mut i = 0usize;
    while i < text.len() {
        if let Some(end) = match_url(text, i) {
            if plain_start < i {
                on_segment(Segment::Plain(&text[plain_start..i]));
            }
            let url = &text[i..end];
            on_segment(Segment::Url(url));
            i = end;
            plain_start = end;
            continue;
        }
        if text.as_bytes()[i] == b'@' {
            if let Some((start, end)) = match_email(text, i) {
                if plain_start < start {
                    on_segment(Segment::Plain(&text[plain_start..start]));
                }
                let email = &text[start..end];
                on_segment(Segment::Email(email));
                i = end;
                plain_start = end;
                continue;
            }
        }
        i += text[i..].chars().next().map_or(1, char::len_utf8);
    }
    if plain_start < text.len() {
        on_segment(Segment::Plain(&text[plain_start..]));
    }
}

/// Wraps a plain-text body as minimal HTML - escaping `&`/`<`/`>` and
/// turning `\n` into `<br/>`, the same shape as mail_parser's own fallback
/// synthesis - but additionally turns bare `http://`/`https://` URLs and
/// bare email addresses into clickable `<a>` tags. Callers (`parse_body`,
/// `decode_text_part`) only ever reach for this when the message has no
/// genuine `text/html` part, so this never runs over sender-authored markup
/// (that goes through `linkify_html_text_nodes` instead).
///
/// URL matching deliberately does not require whitespace (or any other word
/// boundary) before the scheme, so a URL glued directly onto preceding
/// punctuation (`Info:http://example.com`, `Click here!http://example.com`)
/// is still recognized; the scan just looks for the scheme substring
/// wherever it occurs.
fn linkify_to_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 32);
    scan_links(text, |segment| match segment {
        Segment::Plain(s) => out.push_str(&escape_text(s)),
        Segment::Url(url) => push_link(&mut out, url, url),
        Segment::Email(email) => push_link(&mut out, &format!("mailto:{email}"), email),
    });
    out
}

fn push_link(out: &mut String, href: &str, display: &str) {
    out.push_str("<a href=\"");
    out.push_str(&escape_attr(href));
    out.push_str("\">");
    out.push_str(&escape_text(display));
    out.push_str("</a>");
}

/// Turns bare `http(s)` URLs and email addresses found in a genuine
/// sender-authored HTML body's *text nodes* into clickable `<a>` tags,
/// leaving all markup - tags, attributes, comments, and the content of any
/// existing `<a>`, `<script>` or `<style>` element - completely untouched.
///
/// Unlike `linkify_to_html`, this never escapes anything: a text node here
/// is already valid HTML (entities and all), so a matched span is copied
/// straight into both the link's `href` and its display text without
/// modification, and re-escaping it would double-encode existing entities.
///
/// Tag boundaries are found the same lightweight way
/// `trust::html_remote_content_scan` scans for subresource URLs - a plain
/// `<...>` walk, not a real parser - which is enough to keep out of
/// attribute values and skip elements whose content must never be touched.
fn linkify_html_text_nodes(html: &str) -> String {
    let mut out = String::with_capacity(html.len() + 64);
    let mut skip_stack: Vec<String> = Vec::new();
    let mut i = 0usize;
    let mut text_start = 0usize;

    while i < html.len() {
        if html[i..].starts_with("<!--") {
            flush_html_text(&mut out, &html[text_start..i], &skip_stack);
            let end = html[i..].find("-->").map_or(html.len(), |p| i + p + 3);
            out.push_str(&html[i..end]);
            i = end;
            text_start = end;
            continue;
        }
        if html.as_bytes()[i] == b'<' {
            match html[i..].find('>') {
                Some(rel) => {
                    let tag_end = i + rel + 1;
                    flush_html_text(&mut out, &html[text_start..i], &skip_stack);
                    let tag = &html[i..tag_end];
                    out.push_str(tag);
                    update_skip_stack(&mut skip_stack, tag);
                    i = tag_end;
                    text_start = i;
                }
                // An unterminated tag: nothing sensible to do but stop
                // scanning and copy the remainder through untouched.
                None => {
                    flush_html_text(&mut out, &html[text_start..i], &skip_stack);
                    out.push_str(&html[i..]);
                    return out;
                }
            }
            continue;
        }
        i += html[i..].chars().next().map_or(1, char::len_utf8);
    }
    flush_html_text(&mut out, &html[text_start..], &skip_stack);
    out
}

fn flush_html_text(out: &mut String, text: &str, skip_stack: &[String]) {
    if text.is_empty() {
        return;
    }
    if !skip_stack.is_empty() {
        out.push_str(text);
        return;
    }
    scan_links(text, |segment| match segment {
        Segment::Plain(s) => out.push_str(s),
        Segment::Url(url) => {
            out.push_str("<a href=\"");
            out.push_str(url);
            out.push_str("\">");
            out.push_str(url);
            out.push_str("</a>");
        }
        Segment::Email(email) => {
            out.push_str("<a href=\"mailto:");
            out.push_str(email);
            out.push_str("\">");
            out.push_str(email);
            out.push_str("</a>");
        }
    });
}

/// Pushes or pops `tag`'s name onto `skip_stack` when it's an opening or
/// closing `<a>`, `<script>` or `<style>` tag - the elements whose content
/// must never be linkified (an existing link's own text, or non-markup code
/// that merely happens to contain "http"/"@").
fn update_skip_stack(skip_stack: &mut Vec<String>, tag: &str) {
    let Some(inner) = tag.strip_prefix('<').and_then(|t| t.strip_suffix('>')) else { return };
    let is_close = inner.starts_with('/');
    let name_src = inner.strip_prefix('/').unwrap_or(inner);
    let name: String = name_src.chars().take_while(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase();
    if !matches!(name.as_str(), "a" | "script" | "style") {
        return;
    }
    if is_close {
        if let Some(pos) = skip_stack.iter().rposition(|n| n == &name) {
            skip_stack.remove(pos);
        }
    } else if !inner.trim_end().ends_with('/') {
        skip_stack.push(name);
    }
}

/// Escapes text for use as element content, and folds in the same
/// `\n`-to-`<br/>` conversion mail_parser's synthesized rendering used, so
/// non-link text keeps rendering exactly as it did before linkification.
fn escape_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\r' => {}
            '\n' => out.push_str("<br/>"),
            _ => out.push(c),
        }
    }
    out
}

/// Escapes text for use inside a double-quoted HTML attribute value.
fn escape_attr(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

/// If `text[start..]` begins with `http://`/`https://` (case-insensitively),
/// returns the end of the URL: the following run of non-whitespace
/// characters, with trailing sentence punctuation and unbalanced closing
/// brackets trimmed off (so "(see https://example.com)" doesn't pull the
/// sentence's closing paren into the link, but a URL whose own path
/// legitimately contains balanced parens keeps them).
fn match_url(text: &str, start: usize) -> Option<usize> {
    let rest = &text[start..];
    let scheme_len = ["http://", "https://"].iter().find_map(|s| rest.get(..s.len()).filter(|r| r.eq_ignore_ascii_case(s)).map(|_| s.len()))?;
    let after_scheme = start + scheme_len;
    let mut end = after_scheme;
    for (offset, c) in text[after_scheme..].char_indices() {
        if c.is_whitespace() {
            break;
        }
        end = after_scheme + offset + c.len_utf8();
    }
    if end <= after_scheme {
        return None;
    }
    end = trim_trailing_punctuation(text, after_scheme, end);
    (end > after_scheme).then_some(end)
}

fn trim_trailing_punctuation(text: &str, match_start: usize, mut end: usize) -> usize {
    while let Some(last) = text[match_start..end].chars().next_back() {
        let span = &text[match_start..end];
        let strip = match last {
            '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '"' | '*' => true,
            ')' => span.matches('(').count() < span.matches(')').count(),
            ']' => span.matches('[').count() < span.matches(']').count(),
            '}' => span.matches('{').count() < span.matches('}').count(),
            _ => false,
        };
        if !strip {
            break;
        }
        end -= last.len_utf8();
    }
    end
}

/// If `text[at]` is the `@` of what looks like an email address, returns its
/// `(start, end)` byte range - scanning backward over local-part characters
/// and forward over domain-label characters, requiring the final label to
/// look like a TLD (2+ letters) and at least one earlier `.` in the domain.
fn match_email(text: &str, at: usize) -> Option<(usize, usize)> {
    let mut start = at;
    for (idx, c) in text[..at].char_indices().rev() {
        if !is_local_part_char(c) {
            break;
        }
        start = idx;
    }
    if start == at {
        return None;
    }

    let mut domain_end = at + 1;
    for (idx, c) in text[at + 1..].char_indices() {
        if !is_domain_char(c) {
            break;
        }
        domain_end = at + 1 + idx + c.len_utf8();
    }
    // '.' is a domain char, so a greedy scan swallows a trailing sentence
    // period too - shed those before validating the domain shape.
    while domain_end > at + 1 && text[..domain_end].ends_with('.') {
        domain_end -= 1;
    }
    let domain = &text[at + 1..domain_end];
    is_valid_domain(domain).then_some((start, domain_end))
}

fn is_local_part_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '%' | '+' | '-')
}

fn is_domain_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '-')
}

fn is_valid_domain(domain: &str) -> bool {
    let labels: Vec<&str> = domain.split('.').collect();
    labels.len() >= 2 && labels.iter().all(|l| !l.is_empty()) && labels.last().is_some_and(|tld| tld.len() >= 2 && tld.chars().all(|c| c.is_ascii_alphabetic()))
}

/// Decodes one text part's raw bytes (as returned by `BODY.PEEK[<part>]`)
/// into its text and html renderings. The part is wrapped in a minimal
/// single-part message - `Content-Type` (with its charset parameter) and
/// `Content-Transfer-Encoding` from the body structure - and parsed with
/// `mail_parser`, which handles base64/quoted-printable decoding and charset
/// conversion. Returns `(text, html)`: the html half of a `text/html` part is
/// its own markup with bare links in its text nodes linkified
/// (`linkify_html_text_nodes`), while a `text/plain` part's html half is
/// built from scratch via `linkify_to_html` from its converted text.
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
    let text = message.body_text(0).map(|c| c.into_owned());
    // A genuine `text/html` part's own markup only gets its text nodes
    // linkified; a `text/plain` part's html half is mail_parser's
    // unlinkified synthesis (see `parse_body`), so it's rebuilt via
    // `linkify_to_html` instead.
    let html = if part.content_type == "text/html" {
        message.body_html(0).map(|c| linkify_html_text_nodes(&c))
    } else {
        text.as_deref().map(linkify_to_html)
    };
    (text, html)
}

/// Decodes a fetched MIME part's *wire* bytes (as returned by
/// `BODY.PEEK[<part>]`) into the content bytes, undoing the part's declared
/// transfer encoding (`BodyPart::transfer_encoding`, learned from
/// `BODYSTRUCTURE`): `base64` and `quoted-printable` are decoded, while
/// `7bit`/`8bit`/`binary` (and anything unrecognized) are content already.
///
/// This is the attachment counterpart of `decode_text_part`: text parts are
/// decoded by wrapping them in a single-part message and re-parsing with
/// `mail_parser` (so their charset conversion reuses the battle-tested path),
/// but attachment bytes must come out as *bytes*, not text - and a wrapping
/// re-parse would mis-handle `message/rfc822` attachments (whose inner
/// message would be descended into instead of saved whole). Decoding the
/// transfer encoding directly keeps the original content intact regardless of
/// content type.
pub fn transfer_part_bytes(part: &BodyPart, bytes: &[u8]) -> Vec<u8> {
    match part.transfer_encoding.as_deref().unwrap_or("7bit") {
        "base64" => {
            // IMAP servers may fold base64 across CRLF line breaks; strip all
            // ASCII whitespace before decoding, per RFC 2045 §6.8.
            let clean: Vec<u8> = bytes.iter().copied().filter(|b| !b.is_ascii_whitespace()).collect();
            match base64::engine::general_purpose::STANDARD.decode(&clean) {
                Ok(decoded) => decoded,
                // A server that lied about the encoding (or a truncated
                // fetch) must degrade to the raw bytes, never panic.
                Err(_) => bytes.to_vec(),
            }
        }
        "quoted-printable" => decode_quoted_printable(bytes),
        // 7bit, 8bit, binary, or an unrecognized encoding name: the bytes
        // are the content itself.
        _ => bytes.to_vec(),
    }
}

/// RFC 2045 quoted-printable decoder: `=XX` hex escapes become a byte, and a
/// trailing `=` before a line break is a *soft* break that encodes nothing
/// (it exists only to keep encoded lines short and is dropped). Anything that
/// isn't a well-formed escape is passed through literally.
fn decode_quoted_printable(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i] == b'=' {
            // Soft line break: "=\r\n" or "=\n" collapses to nothing.
            if i + 1 < input.len() && (input[i + 1] == b'\r' || input[i + 1] == b'\n') {
                i += if input[i + 1] == b'\r' && i + 2 < input.len() && input[i + 2] == b'\n' { 3 } else { 2 };
                continue;
            }
            // `=XX` hex escape.
            if i + 2 < input.len() {
                let high = (input[i + 1] as char).to_digit(16);
                let low = (input[i + 2] as char).to_digit(16);
                if let (Some(high), Some(low)) = (high, low) {
                    out.push((high << 4 | low) as u8);
                    i += 3;
                    continue;
                }
            }
            // A lone `=` that isn't part of a valid escape is passed through.
            out.push(b'=');
            i += 1;
        } else {
            out.push(input[i]);
            i += 1;
        }
    }
    out
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

/// Extracts the one-line snippet from an already-decoded [`EmailBody`], for
/// the new-email path in `sync_mailbox` where the body is fetched before the
/// preview pass runs. Same normalization as `preview_from_raw`; the caller
/// patches the summary's `preview` with the result so the separate preview
/// fetch round trip for that message is skipped entirely.
pub fn preview_from_body(body: &EmailBody) -> Option<String> {
    let text = match (&body.text_body, &body.html_body) {
        (Some(t), _) => t.as_str(),
        (None, Some(h)) => &strip_html(h),
        (None, None) => return None,
    };
    let preview = normalize_preview(text);
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

    fn has_attachment(parts: &[BodyPart]) -> bool {
        parts.iter().any(|p| p.is_attachment)
    }

    fn fixture(name: &str) -> Vec<u8> {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-fixtures/").to_string() + name;
        std::fs::read(&path).unwrap_or_else(|e| panic!("reading fixture {path}: {e}"))
    }

    #[test]
    fn plain_text_fixture_has_text_body() {
        let body = parse_body(Uid(0), &fixture("plain-text.eml")).expect("parses");
        assert!(body.text_body.is_some());
        // A genuinely plain-text-only message doesn't leave html_body unset:
        // parse_body synthesizes one via linkify_to_html (see that
        // function's doc comment), so render_body() in the app crate - which
        // prefers html_body when present - renders this fixture via the
        // WebKit view, not the Gtk.TextView fallback path.
        assert!(body.html_body.is_some());
    }

    #[test]
    fn plain_text_fixture_linkifies_urls_and_email_addresses() {
        let body = parse_body(Uid(0), &fixture("plain-text-with-link.eml")).expect("parses");
        let html = body.html_body.expect("has synthesized html body");
        // "Details here!http://..." - no whitespace before the scheme - is
        // exactly the reported case; the URL is recognized anyway, and its
        // query string's `&` is escaped in the emitted href.
        assert!(html.contains(r#"<a href="http://example.com/path?a=1&amp;b=2">http://example.com/path?a=1&amp;b=2</a>"#), "html was: {html}");
        assert!(html.contains(r#"<a href="mailto:carol@example.org">carol@example.org</a>"#), "html was: {html}");
    }

    #[test]
    fn linkify_to_html_recognizes_a_url_glued_to_punctuation() {
        assert_eq!(linkify_to_html("Info:http://example.com end"), r#"Info:<a href="http://example.com">http://example.com</a> end"#);
        assert_eq!(linkify_to_html("Click here!https://example.com end"), r#"Click here!<a href="https://example.com">https://example.com</a> end"#);
    }

    #[test]
    fn linkify_to_html_trims_trailing_sentence_punctuation() {
        assert_eq!(linkify_to_html("see http://example.com."), r#"see <a href="http://example.com">http://example.com</a>."#);
        assert_eq!(linkify_to_html("see http://example.com, ok"), r#"see <a href="http://example.com">http://example.com</a>, ok"#);
        assert_eq!(linkify_to_html("(see http://example.com)"), r#"(see <a href="http://example.com">http://example.com</a>)"#);
    }

    #[test]
    fn linkify_to_html_keeps_a_urls_own_balanced_parens() {
        assert_eq!(
            linkify_to_html("http://example.com/wiki/Rust_(programming_language)"),
            r#"<a href="http://example.com/wiki/Rust_(programming_language)">http://example.com/wiki/Rust_(programming_language)</a>"#
        );
    }

    #[test]
    fn linkify_to_html_escapes_ampersand_in_link_and_plain_text() {
        assert_eq!(
            linkify_to_html("Terms & Conditions: http://example.com?a=1&b=2"),
            r#"Terms &amp; Conditions: <a href="http://example.com?a=1&amp;b=2">http://example.com?a=1&amp;b=2</a>"#
        );
    }

    #[test]
    fn linkify_to_html_links_a_bare_email_address() {
        assert_eq!(linkify_to_html("reach carol@example.org today"), r#"reach <a href="mailto:carol@example.org">carol@example.org</a> today"#);
    }

    #[test]
    fn linkify_to_html_leaves_plain_text_unchanged_besides_escaping() {
        assert_eq!(linkify_to_html("Hello Bob,\nno links here.\n\n- Alice"), "Hello Bob,<br/>no links here.<br/><br/>- Alice");
    }

    #[test]
    fn linkify_html_text_nodes_links_a_bare_url_in_a_text_node() {
        let html = linkify_html_text_nodes("<p>Info:http://example.com/a?x=1&amp;y=2 for details.</p>");
        assert_eq!(html, r#"<p>Info:<a href="http://example.com/a?x=1&amp;y=2">http://example.com/a?x=1&amp;y=2</a> for details.</p>"#);
    }

    #[test]
    fn linkify_html_text_nodes_does_not_touch_an_existing_link() {
        let html = linkify_html_text_nodes(r#"<a href="http://example.com/existing?x=1&amp;y=2">click here http://not-a-real-link.example</a>"#);
        assert_eq!(html, r#"<a href="http://example.com/existing?x=1&amp;y=2">click here http://not-a-real-link.example</a>"#);
    }

    #[test]
    fn linkify_html_text_nodes_does_not_touch_style_or_script_content() {
        let html = linkify_html_text_nodes(
            r#"<style>body { background: url(http://example.com/bg.png); }</style><script>var u = "http://example.com/js";</script>"#,
        );
        assert_eq!(
            html,
            r#"<style>body { background: url(http://example.com/bg.png); }</style><script>var u = "http://example.com/js";</script>"#
        );
    }

    #[test]
    fn linkify_html_text_nodes_does_not_touch_attribute_values() {
        let html = linkify_html_text_nodes(r#"<img src="http://example.com/pixel.png" alt="see http://example.com/info">"#);
        assert_eq!(html, r#"<img src="http://example.com/pixel.png" alt="see http://example.com/info">"#);
    }

    #[test]
    fn html_with_unlinked_url_fixture_gets_its_bare_url_linkified() {
        let body = parse_body(Uid(0), &fixture("html-with-unlinked-url.eml")).expect("parses");
        let html = body.html_body.expect("has html body");
        // The bug this fixture reproduces: a genuine sender text/html part
        // whose own markup never wraps its URL in <a> - "link:http://..."
        // with no space, exactly the reported case.
        assert!(html.contains(r#"link:<a href="http://example.com/track?a=1&amp;b=2">http://example.com/track?a=1&amp;b=2</a>"#), "html was: {html}");
        // The sender's own existing link and its display text must survive
        // untouched - no nested <a>, no altered href.
        assert!(html.contains(r#"<a href="http://example.com/existing?x=1&amp;y=2">click here http://not-a-real-link.example</a>"#), "html was: {html}");
        // A CSS `url(...)` inside <style> must never become a link.
        assert!(html.contains("background: url(http://example.com/bg.png)"), "html was: {html}");
        assert!(!html.contains(r#"url(<a href"#), "html was: {html}");
    }

    #[test]
    fn html_inline_css_fixture_parses_html() {
        let body = parse_body(Uid(0), &fixture("html-inline-css.eml")).expect("parses");
        let html = body.html_body.expect("has html body");
        assert!(html.contains("highlight"));
    }

    #[test]
    fn html_cid_image_fixture_lists_the_inline_image_part_with_its_cid() {
        let body = parse_body(Uid(0), &fixture("html-cid-image.eml")).expect("parses");
        let html = body.html_body.expect("has html body");
        assert!(html.contains("cid:logo123"));
        // The inline image is a part with its Content-ID and a real IMAP
        // section number, so the on-demand `FetchAttachment` path can target
        // it - and it carries no filename, so it must not count as an
        // attachment (the strip is attachments-only).
        let image = body.parts.iter().find(|p| p.cid.as_deref() == Some("logo123")).expect("inline image listed with its cid");
        assert_eq!(image.part_number, "2");
        assert_eq!(image.content_type, "image/png");
        assert!(!image.is_attachment);
        assert!(!has_attachment(&body.parts));
    }

    #[test]
    fn html_external_image_fixture_parses_html() {
        let body = parse_body(Uid(0), &fixture("html-external-image.eml")).expect("parses");
        assert!(body.html_body.expect("has html body").contains("example.com/tracker.png"));
    }

    /// A real-world HTML-only message whose body is a full-width
    /// presentation table with a 680px content card - the kind of layout the
    /// reading pane's width clamp (`MESSAGE_WIDTH_CLAMP_CSS` in the app
    /// crate's `window.rs`) exists for. The fixture proves the parse
    /// survives the quoted-printable body and keeps the wide markup intact.
    #[test]
    fn wide_email_fixture_parses_html() {
        let body = parse_body(Uid(0), &fixture("wide-email.eml")).expect("parses");
        let html = body.html_body.expect("has html body");
        // The heading survives quoted-printable decoding (utf-8, soft line
        // breaks) ...
        assert!(html.contains("An Update About Recent Order Delays"));
        // ... and the wide layout is preserved: the full-width presentation
        // table and the 680px content card are still in the markup.
        assert!(html.contains(r#"<table role="presentation" width="100%""#));
        assert!(html.contains("max-width:680px"));
    }

    #[test]
    fn malformed_html_fixture_does_not_panic() {
        let body = parse_body(Uid(0), &fixture("html-malformed.eml")).expect("parses");
        assert!(body.html_body.is_some());
    }

    /// The whole-message fallback path must produce *real* IMAP section paths,
    /// not enumeration counters: an on-demand `FetchAttachment` fetches by
    /// `BODY.PEEK[<part_number>]`, and "0" was never a valid section - the
    /// server would error (or return nothing) and the Save button would hang.
    /// A multipart/mixed [text/plain, pdf] message numbers the pdf `2`.
    #[test]
    fn fallback_parts_carry_real_section_numbers_for_a_mixed_message() {
        let body = parse_body(Uid(0), &fixture("with-attachment.eml")).expect("parses");
        assert!(body.text_body.is_some());
        let pdf = body.parts.iter().find(|p| p.filename.as_deref() == Some("doc.pdf")).expect("pdf attachment listed");
        assert_eq!(pdf.part_number, "2");
        assert_eq!(pdf.content_type, "application/pdf");
        assert_eq!(pdf.size, b"%PDF-1.4\n%fake pdf bytes\n".len() as u32);
        assert!(pdf.is_attachment);
    }

    /// A multipart/alternative nested inside multipart/mixed numbers the
    /// alternative's children `1.1`/`1.2` and the trailing attachment `2` -
    /// the same paths the server's `BODYSTRUCTURE` would report.
    #[test]
    fn fallback_parts_number_nested_structures_like_bodystructure() {
        let body = parse_body(Uid(0), &fixture("nested-parts.eml")).expect("parses");
        let blob = body.parts.iter().find(|p| p.filename.as_deref() == Some("blob.bin")).expect("blob attachment listed");
        assert_eq!(blob.part_number, "2");
        assert_eq!(blob.content_type, "application/octet-stream");
        // Only the octet-stream is an attachment here - the alternative's two
        // halves are the message body, not attachments.
        assert_eq!(body.parts.len(), 1, "nested alternative halves must not surface as attachments");
    }

    /// An attachments-only message (no text part at all - the case that sent
    /// the viewer down the whole-message fallback in the first place, because
    /// the partial-fetch path has nothing to fetch) is a root single part and
    /// its attachment is section `1`.
    #[test]
    fn fallback_parts_number_a_single_part_attachment_as_one() {
        let body = parse_body(Uid(0), &fixture("attachment-only.eml")).expect("parses");
        assert!(body.text_body.is_none());
        assert!(body.html_body.is_none());
        let fax = body.parts.iter().find(|p| p.filename.as_deref() == Some("fax.pdf")).expect("fax attachment listed");
        assert_eq!(fax.part_number, "1");
        assert_eq!(fax.content_type, "application/pdf");
    }

    #[test]
    fn html_cid_hosted_fixture_keeps_the_full_content_id() {
        let body = parse_body(Uid(0), &fixture("html-cid-hosted.eml")).expect("parses");
        let image = body.parts.iter().find(|p| p.content_type == "image/png").expect("inline image listed");
        // The Content-ID is a full msg-id (`<logo123@host.example>`, angle
        // brackets stripped by the parser); the HTML references it verbatim.
        assert_eq!(image.cid.as_deref(), Some("logo123@host.example"));
        assert_eq!(image.part_number, "2");
        assert!(!image.is_attachment);
    }

    #[test]
    fn html_cid_encoded_fixture_parses_without_panicking() {
        let body = parse_body(Uid(0), &fixture("html-cid-encoded.eml")).expect("parses");
        assert!(body.html_body.expect("has html body").contains("cid:logo%40123"));
        let image = body.parts.iter().find(|p| p.cid.as_deref() == Some("logo@123")).expect("inline image listed");
        assert_eq!(image.part_number, "2");
        assert!(!image.is_attachment);
    }

    #[cfg(debug_assertions)]
    #[test]
    fn cid_refs_rewrite_to_data_uris_from_the_raw_message() {
        let raw = fixture("html-cid-image.eml");
        let html = parse_body(Uid(0), &raw).expect("parses").html_body.expect("has html body");
        let rewritten = rewrite_cid_refs_to_data_uris(&html, &raw);
        // The reference is replaced by a data: URI carrying the decoded PNG.
        assert!(!rewritten.contains("cid:logo123"));
        assert!(rewritten.contains("data:image/png;base64,iVBORw0KGgo"), "rewritten: {rewritten}");
    }

    #[cfg(debug_assertions)]
    #[test]
    fn cid_refs_that_resolve_nowhere_survive_verbatim() {
        let raw = fixture("html-cid-image.eml");
        let html = "<img src=\"cid:missing123\">";
        let rewritten = rewrite_cid_refs_to_data_uris(html, &raw);
        assert_eq!(rewritten, html);
    }

    /// The iMIP invitation fixture: `parse_body` must surface the
    /// `text/calendar` part's decoded contents as `calendar_ics`, and the
    /// calendar part must not appear in the attachment metadata (it's body
    /// content, not an attachment).
    #[test]
    fn imip_request_fixture_carries_the_calendar_payload() {
        let body = parse_body(Uid(0), &fixture("imip-request.eml")).expect("parses");
        let ics = body.calendar_ics.expect("invitation has a calendar payload");
        assert!(ics.contains("METHOD:REQUEST"));
        assert!(ics.contains("UID:sync-2026-08-10@example.com"));
        assert!(ics.contains("mailto:ada@example.com"));
        assert!(body.text_body.is_some(), "the alternative's text half still renders");
        assert!(!has_attachment(&body.parts), "the calendar part is not an attachment");
        assert!(!body.parts.iter().any(|p| p.content_type == "text/calendar"));
    }

    #[test]
    fn imip_cancel_fixture_carries_the_calendar_payload() {
        let body = parse_body(Uid(0), &fixture("imip-cancel.eml")).expect("parses");
        let ics = body.calendar_ics.expect("cancellation has a calendar payload");
        assert!(ics.contains("METHOD:CANCEL"));
    }

    /// The Outlook/Teams invitation fixture - a base64 `text/calendar`
    /// attachment named `invite.ics` inside a `multipart/mixed` envelope -
    /// must surface its payload as `calendar_ics` (transfer-decoded) and stay
    /// out of the attachment list, exactly like the simpler `imip-request`
    /// fixture.
    #[test]
    fn imip_outlook_fixture_carries_the_calendar_payload() {
        let body = parse_body(Uid(0), &fixture("imip-outlook.eml")).expect("parses");
        let ics = body.calendar_ics.expect("Outlook invitation has a calendar payload");
        assert!(ics.contains("METHOD:REQUEST"));
        assert!(ics.contains("DTSTART;TZID=\"W. Europe Standard Time\":20260810T100000"));
        assert!(ics.contains("X-MICROSOFT-CDO-BUSYSTATUS:BUSY"));
        assert!(body.text_body.is_some(), "the alternative's text half still renders");
        assert!(!has_attachment(&body.parts), "the calendar part is not an attachment");
        assert!(!body.parts.iter().any(|p| p.content_type == "text/calendar"));
    }

    /// The read-receipt fixture: `parse_body` must carry the
    /// `Disposition-Notification-To` request through the headers (the
    /// reading pane's banner reads it with `parse_disposition_notification_to`).
    #[test]
    fn mdn_request_fixture_carries_the_receipt_request_header() {
        let body = parse_body(Uid(0), &fixture("mdn-request.eml")).expect("parses");
        let request = lookout_core::parse_disposition_notification_to(&body.headers);
        assert_eq!(request, vec!["alice@example.com".to_string()]);
        assert!(!lookout_core::is_auto_submitted(&body.headers));
        assert!(!lookout_core::is_report_message(&body.headers));
        assert!(body.text_body.is_some());
    }

    /// The partial-fetch assembly path: a fetched base64 `text/calendar` part
    /// is transfer-decoded into `calendar_ics`, and stays out of `parts`.
    #[test]
    fn assemble_body_from_parts_decodes_a_base64_calendar_part() {
        use base64::Engine;
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:x@y\r\nDTSTAMP:20260101T000000Z\r\nDTSTART:20260715T140000Z\r\nSUMMARY:Hi\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        let encoded = base64::engine::general_purpose::STANDARD.encode(ics.as_bytes());
        let mut calendar = text_part("2", "text/calendar", Some("utf-8"), "base64");
        calendar.is_attachment = false;
        let parts = vec![text_part("1", "text/plain", Some("utf-8"), "7bit"), calendar];
        let body = assemble_body_from_parts(
            Uid(5),
            Vec::new(),
            &parts,
            &[("1".to_string(), b"plain text".to_vec()), ("2".to_string(), encoded.into_bytes())],
        );
        assert_eq!(body.calendar_ics.as_deref(), Some(ics));
        assert_eq!(body.text_body.as_deref(), Some("plain text"));
        assert!(body.parts.is_empty(), "the calendar part must not surface in the attachment list");
    }

    /// An invitation-only message (calendar part, no text parts) still
    /// carries the payload - the caller decides whether to also fall back to a
    /// whole-message fetch for the prose.
    #[test]
    fn assemble_body_from_parts_captures_a_calendar_part_without_text_parts() {
        let mut calendar = text_part("1", "text/calendar", None, "7bit");
        calendar.is_attachment = false;
        let body = assemble_body_from_parts(
            Uid(6),
            Vec::new(),
            &[calendar],
            &[("1".to_string(), b"BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nEND:VCALENDAR\r\n".to_vec())],
        );
        assert!(body.calendar_ics.is_some());
        assert!(body.text_body.is_none());
        assert!(body.html_body.is_none());
    }

    /// Assembling a message with no calendar part leaves `calendar_ics` unset
    /// - the ordinary case.
    #[test]
    fn assemble_body_from_parts_without_a_calendar_part_leaves_it_unset() {
        let body = assemble_body_from_parts(
            Uid(7),
            Vec::new(),
            &[text_part("1", "text/plain", Some("utf-8"), "7bit")],
            &[("1".to_string(), b"hi".to_vec())],
        );
        assert_eq!(body.calendar_ics, None);
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

    fn attachment_part(part_number: &str, file_type: &str, transfer_encoding: &str) -> BodyPart {
        BodyPart {
            part_number: part_number.to_string(),
            content_type: file_type.to_string(),
            charset: None,
            transfer_encoding: Some(transfer_encoding.to_string()),
            filename: Some("doc.bin".to_string()),
            cid: None,
            size: 0,
            is_attachment: true,
        }
    }

    /// `BODY.PEEK[<part>]` returns the part still in its transfer encoding, so
    /// the decoder must undo base64 - including base64 folded across CRLF, as
    /// servers emit for long lines - to reveal the content bytes.
    #[test]
    fn transfer_part_bytes_decodes_base64() {
        let payload: &[u8] = b"PNG\x00\x01\x02\x03binary content";
        let encoded = base64::engine::general_purpose::STANDARD.encode(payload);
        let folded = encoded.as_bytes().chunks(8).map(|c| std::str::from_utf8(c).unwrap()).collect::<Vec<_>>().join("\r\n");
        let part = attachment_part("2", "image/png", "base64");
        assert_eq!(transfer_part_bytes(&part, folded.as_bytes()), payload);
    }

    /// A base64 part whose bytes aren't valid base64 (truncated fetch, or a
    /// server that lied about the encoding) must fall back to the raw bytes
    /// rather than panic or vanish.
    #[test]
    fn transfer_part_bytes_base64_failure_falls_back_to_raw() {
        let part = attachment_part("2", "application/pdf", "base64");
        assert_eq!(transfer_part_bytes(&part, b"%%%not-base64%%%"), b"%%%not-base64%%%");
    }

    /// Quoted-printable: `=XX` hex escapes and CRLF soft breaks must decode to
    /// the original bytes.
    #[test]
    fn transfer_part_bytes_decodes_quoted_printable() {
        let part = attachment_part("2", "text/plain", "quoted-printable");
        // "front=3Dline" across a soft break -> "front=line"; "caf=E9" -> "café".
        // Quoted-printable is byte-oriented: `=E9` is the Latin-1 byte 0xE9,
        // which the transfer decoder passes through untouched (charset
        // conversion is the text-part path's job, not this decoder's).
        let qp = b"caf=E9 =3D equals =\r\ncontinued";
        assert_eq!(transfer_part_bytes(&part, qp), b"caf\xe9 = equals continued");
    }

    #[test]
    fn transfer_part_bytes_passes_7bit_through_unmodified() {
        let part = attachment_part("2", "message/rfc822", "7bit");
        let raw = b"From: a@b.c\r\nSubject: hi\r\n\r\nbody";
        assert_eq!(transfer_part_bytes(&part, raw), raw);
    }

    #[test]
    fn transfer_part_bytes_with_no_encoding_is_passthrough() {
        let part = attachment_part("2", "application/octet-stream", "binary");
        assert_eq!(transfer_part_bytes(&part, b"\x00\x01\xff"), b"\x00\x01\xff");
    }

    #[test]
    fn empty_or_garbage_header_section_degrades_gracefully() {
        // An empty section is a clean "no headers"; garbage bytes must be
        // tolerated (mail_parser is deliberately lenient) rather than panic.
        assert!(parse_headers_section(b"").is_empty());
        let _ = parse_headers_section(b"\x00\x01\x02");
    }

    /// A stale structure's part number must not matter: the inline image is
    /// re-found by its `Content-ID` (which survives a server-side re-wrap)
    /// and returned transfer-decoded.
    #[test]
    fn find_part_in_raw_matches_a_stale_numbered_cid_part() {
        let raw = fixture("html-cid-image.eml");
        let mut part = text_part("9.9", "image/png", None, "base64");
        part.cid = Some("logo123".to_string());
        let bytes = find_part_in_raw(&raw, &part).expect("cid part re-found in the raw message");
        assert!(bytes.starts_with(&[0x89, b'P', b'N', b'G']), "decoded PNG magic, got: {bytes:?}");
    }

    /// The requested part's stored cid may be the `id@host` form of a raw
    /// message whose Content-ID is written `<id@host>` - `cid_matches`
    /// trims the angle brackets, so the re-derive must still hit.
    #[test]
    fn find_part_in_raw_tolerates_angle_bracketed_content_ids() {
        let raw = fixture("html-cid-image.eml");
        let mut part = text_part("2", "image/png", None, "base64");
        part.cid = Some("<logo123>".to_string());
        assert!(find_part_in_raw(&raw, &part).is_some());
    }

    /// A non-cid attachment (a user-clicked strip item) is re-found by its
    /// filename when the structure went stale.
    #[test]
    fn find_part_in_raw_matches_a_stale_numbered_named_attachment() {
        let raw = fixture("with-attachment.eml");
        let mut pdf = text_part("7", "application/pdf", None, "base64");
        pdf.filename = Some("doc.pdf".to_string());
        let bytes = find_part_in_raw(&raw, &pdf).expect("named part re-found in the raw message");
        assert!(bytes.starts_with(b"%PDF-"), "decoded pdf bytes, got: {bytes:?}");
    }

    /// A part that genuinely isn't in the message answers `None` - the
    /// caller then reports it as missing rather than serving a wrong part.
    #[test]
    fn find_part_in_raw_returns_none_for_a_foreign_part() {
        let raw = fixture("html-cid-image.eml");
        let part = text_part("3", "image/gif", None, "base64");
        assert!(find_part_in_raw(&raw, &part).is_none());
        let mut wrong_cid = text_part("2", "image/png", None, "base64");
        wrong_cid.cid = Some("some-other-id".to_string());
        assert!(find_part_in_raw(&raw, &wrong_cid).is_none());
    }
}
