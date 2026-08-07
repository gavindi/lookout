use std::collections::BTreeSet;

use chrono::{DateTime, Utc};

use crate::ids::{MailboxId, Uid};
use crate::thread::ThreadKey;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EmailAddress {
    pub name: Option<String>,
    pub address: String,
}

impl EmailAddress {
    pub fn new(address: impl Into<String>) -> Self {
        EmailAddress {
            name: None,
            address: address.into(),
        }
    }

    /// A display label suitable for list rows: the display name if present,
    /// otherwise the bare address.
    pub fn display_label(&self) -> &str {
        match &self.name {
            Some(name) if !name.trim().is_empty() => name,
            _ => &self.address,
        }
    }
}

/// A generic source of contact-address completions.
pub trait ContactsProvider {
    /// Returns matching addresses for `prefix`, in preference order.
    fn search_contacts(&self, prefix: &str, limit: usize) -> Vec<EmailAddress>;
}

/// The standard IMAP system flags (RFC 3501 §2.3.2), excluding `\Recent`
/// which is session-scoped and not meaningfully cacheable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub enum SystemFlagBit {
    Seen,
    Answered,
    Flagged,
    Deleted,
    Draft,
}

impl SystemFlagBit {
    /// Parses a raw IMAP flag atom (e.g. `\Seen`) into a system flag, if it is one.
    pub fn from_imap_flag(flag: &str) -> Option<Self> {
        match flag {
            "\\Seen" => Some(SystemFlagBit::Seen),
            "\\Answered" => Some(SystemFlagBit::Answered),
            "\\Flagged" => Some(SystemFlagBit::Flagged),
            "\\Deleted" => Some(SystemFlagBit::Deleted),
            "\\Draft" => Some(SystemFlagBit::Draft),
            _ => None,
        }
    }

    pub fn as_imap_flag(self) -> &'static str {
        match self {
            SystemFlagBit::Seen => "\\Seen",
            SystemFlagBit::Answered => "\\Answered",
            SystemFlagBit::Flagged => "\\Flagged",
            SystemFlagBit::Deleted => "\\Deleted",
            SystemFlagBit::Draft => "\\Draft",
        }
    }
}

/// The IMAP keyword namespace Lookout uses for color tags: a message tagged
/// with key `k` carries the keyword `$Lookout-tag-k`. Following the RFC 3501
/// convention (and Outlook's `$Category-*` precedent) that keywords beginning
/// with `$` are reserved for informational semantics rather than client
/// junk.
pub const TAG_KEYWORD_PREFIX: &str = "$Lookout-tag-";

/// The full IMAP keyword atom for a tag key.
pub fn tag_keyword(key: &str) -> String {
    format!("{TAG_KEYWORD_PREFIX}{key}")
}

/// The tag key a keyword atom refers to, if it is one of ours.
pub fn tag_key_from_keyword(keyword: &str) -> Option<&str> {
    keyword.strip_prefix(TAG_KEYWORD_PREFIX)
}

/// Converts a user-typed tag name into the key stored in the `$Lookout-tag-*`
/// keyword. Lowercases and keeps only `[a-z0-9]`, collapsing whitespace and
/// runs of other punctuation to a single `-`, so the result is always a legal
/// RFC 3501 keyword atom: no spaces, no control characters, and none of the
/// reserved characters `( ) { } % * " \`. An empty name (or one of only
/// punctuation) sanitizes to an empty string, which callers reject as
/// "invalid key".
pub fn sanitize_tag_key(name: &str) -> String {
    let mut out = String::new();
    let mut pending_hyphen = false;
    for ch in name.chars().flat_map(char::to_lowercase) {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
            if pending_hyphen && !out.is_empty() {
                out.push('-');
            }
            pending_hyphen = false;
            out.push(ch);
        } else {
            pending_hyphen = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// List-row weight projection of a message — cheap to fetch (`ENVELOPE` +
/// `FLAGS` + `RFC822.SIZE` + `BODYSTRUCTURE`, no body) and cache in SQLite.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EmailSummary {
    pub uid: Uid,
    pub mailbox: MailboxId,
    pub message_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub thread_key: ThreadKey,
    pub subject: Option<String>,
    pub from: Vec<EmailAddress>,
    pub to: Vec<EmailAddress>,
    pub cc: Vec<EmailAddress>,
    pub date: DateTime<Utc>,
    pub flags: BTreeSet<SystemFlagBit>,
    /// Custom IMAP keywords, including the `$Lookout-tag-*` color-tag
    /// namespace (see Phase 2 of the roadmap).
    pub keywords: BTreeSet<String>,
    pub size: u32,
    pub has_attachment: bool,
    pub preview: Option<String>,
    /// The message's MIME part tree, flattened into its leaf parts (with
    /// `BODYSTRUCTURE`-derived part numbers), or `None` if the server didn't
    /// report a body structure for this message (or its envelope was cached
    /// before BODYSTRUCTURE was fetched). `Some` enables the viewer's
    /// partial-fetch path: the text parts' bytes are fetched by number, and
    /// attachment parts are never downloaded. `has_attachment` is derived
    /// from this.
    #[serde(default)]
    pub structure: Option<Vec<BodyPart>>,
}

impl EmailSummary {
    pub fn is_unread(&self) -> bool {
        !self.flags.contains(&SystemFlagBit::Seen)
    }

    pub fn is_starred(&self) -> bool {
        self.flags.contains(&SystemFlagBit::Flagged)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BodyPart {
    /// IMAP body-part path, e.g. `"1.2"`, used for partial `BODY[]` fetches.
    pub part_number: String,
    /// `type/subtype` of the MIME part, e.g. `text/plain` or `image/png`.
    pub content_type: String,
    /// The part's declared charset (from the `charset` Content-Type parameter,
    /// e.g. `utf-8` or `iso-8859-1`), used to decode text parts' bytes.
    #[serde(default)]
    pub charset: Option<String>,
    /// The part's transfer encoding as the server reported it in
    /// `BODYSTRUCTURE` (`base64`, `quoted-printable`, `7bit`, ...), used to
    /// decode the bytes a partial `BODY[<part>]` fetch returns.
    #[serde(default)]
    pub transfer_encoding: Option<String>,
    pub filename: Option<String>,
    /// The `Content-ID` for inline `cid:` image resolution, if any.
    pub cid: Option<String>,
    pub size: u32,
    pub is_attachment: bool,
}

impl BodyPart {
    /// Whether this part is one of the two body-carrying text types the
    /// viewer renders (`text/plain` / `text/html`). This is the filter the
    /// partial-fetch path uses to decide which parts to download: everything
    /// else (images, attachments) is metadata-only until something asks for
    /// its bytes.
    pub fn is_text(&self) -> bool {
        matches!(self.content_type.as_str(), "text/plain" | "text/html")
    }
}

/// Matches a `cid:` reference from a message's HTML against a MIME part's
/// `Content-ID` (`BodyPart::cid`). The two rarely match verbatim: HTML
/// references may drop the angle brackets of the RFC 2392 msg-id syntax
/// (`<id@host>`), percent-encode characters (`logo%40123`), or reference
/// only the local part of a full `id@host`. Matching ladder:
///
/// 1. exact equality after trimming whitespace and surrounding `<>`,
/// 2. equality of the percent-decoded forms (WebKit's scheme-request path
///    keeps percent-encoding intact; HTML may or may not encode),
/// 3. equality of the local part (everything before the first `@`) - a
///    reference like `cid:logo123` must resolve a part whose Content-ID is
///    `<logo123@host.example>`.
pub fn cid_matches(request_ref: &str, part_cid: &str) -> bool {
    let request = trimmed_id(request_ref);
    let part = trimmed_id(part_cid);
    if request.is_empty() || part.is_empty() {
        return false;
    }
    if request == part {
        return true;
    }
    let decoded_request = percent_decode(request);
    let decoded_part = percent_decode(part);
    if decoded_request == decoded_part {
        return true;
    }
    // Local-part fallback: a reference that drops the host of a full
    // msg-id (`cid:logo123` for `<logo123@host.example>`, or
    // `cid:image001.png@01D8E9B3` for `<image001.png@01D8E9B3.host>` - a
    // pattern senders do emit) still resolves the part.
    local_part(&decoded_request) == local_part(&decoded_part)
}

/// Strips surrounding whitespace and the angle brackets of the RFC 2392
/// msg-id form (`<id@host>`).
fn trimmed_id(s: &str) -> &str {
    s.trim().trim_matches(|c| c == '<' || c == '>')
}

/// The local part of a Content-ID (everything before the first `@`), or the
/// whole reference when it has none.
fn local_part(s: &str) -> &str {
    s.split('@').next().unwrap_or(s)
}

/// RFC 3986 percent-decoding: `%XX` hex escapes become their byte. Enough
/// for `cid:` references, where only `@`, `/`, and a few punctuation
/// characters are ever encoded; anything malformed is passed through.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let high = (bytes[i + 1] as char).to_digit(16);
            let low = (bytes[i + 2] as char).to_digit(16);
            if let (Some(high), Some(low)) = (high, low) {
                out.push((high << 4 | low) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SpfResult {
    Pass,
    Fail,
    SoftFail,
    Neutral,
    None,
    TempError,
    PermError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DkimResult {
    Pass,
    Fail,
    Policy,
    Neutral,
    TempError,
    PermError,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DmarcResult {
    Pass,
    Fail,
    TempError,
    PermError,
    None,
}

/// Parsed from the `Authentication-Results` header (RFC 8601).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AuthenticationResults {
    pub spf: Option<SpfResult>,
    pub dkim: Option<DkimResult>,
    pub dmarc: Option<DmarcResult>,
}

/// Fetched lazily on message open. With a `BODYSTRUCTURE`-derived part
/// structure in the message's summary, only the text parts (and headers) are
/// downloaded (`BODY.PEEK[<part>]`); without one, the whole message is
/// fetched as a fallback. Attachment bytes are never fetched for display -
/// `parts` carries their metadata (`BodyPart::size`, filename, ...) so a
/// later on-demand download can target the right part number.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EmailBody {
    pub uid: Uid,
    pub text_body: Option<String>,
    pub html_body: Option<String>,
    pub parts: Vec<BodyPart>,
    pub headers: Vec<(String, String)>,
    pub auth_results: Option<AuthenticationResults>,
}

/// The unsubscribe actions a message offers, parsed from its
/// `List-Unsubscribe` (RFC 2369) and `List-Unsubscribe-Post` (RFC 8058)
/// headers. At most one `mailto:` address and at most one `http(s)` URL are
/// kept - the first of each kind in header order, matching the RFC's
/// "alternate methods" intent. `one_click` is true when the message also
/// carries `List-Unsubscribe-Post: List-Unsubscribe=One-Click`, which
/// signals the `http` URL accepts a one-click POST of
/// `List-Unsubscribe=One-Click` (no email round trip).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListUnsubscribe {
    /// The `mailto:` target with scheme stripped and percent-encoding
    /// undone (the part before any `?subject=...` parameters).
    pub mailto: Option<String>,
    /// The `http(s)://` URL, verbatim.
    pub http: Option<String>,
    pub one_click: bool,
}

/// Case-insensitive lookup of one header field in the raw `(name, value)`
/// pairs `EmailBody::headers` carries. Header names are case-insensitive per
/// RFC 5322, and the pairs come straight off the wire, so lookups must not
/// assume casing.
pub fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
}

/// Parses a message's unsubscribe headers into the actions its banner can
/// offer. Returns `None` when there's no `List-Unsubscribe` header at all,
/// or when it names no `mailto:`/`http(s)` action (e.g. only a
/// non-one-click-able scheme). A `mailto:` action is also kept when the
/// http(s) URL is present, so a failing POST can degrade to it.
pub fn parse_list_unsubscribe(headers: &[(String, String)]) -> Option<ListUnsubscribe> {
    let raw = header_value(headers, "list-unsubscribe")?;
    let mut mailto = None;
    let mut http = None;
    // RFC 2369: a comma-separated list of actions, each a `mailto:` or
    // `http(s):` URL in angle brackets (angle brackets optional in the wild).
    for item in raw.split(',') {
        let item = item.trim();
        let target = item.strip_prefix('<').and_then(|r| r.strip_suffix('>')).unwrap_or(item).trim();
        if let Some(addr) = target.strip_prefix("mailto:") {
            let addr = addr.split('?').next().unwrap_or("").trim();
            let addr = percent_decode(addr);
            if !addr.is_empty() && mailto.is_none() {
                mailto = Some(addr);
            }
        } else if (target.starts_with("https://") || target.starts_with("http://")) && http.is_none() {
            http = Some(target.to_string());
        }
    }
    if mailto.is_none() && http.is_none() {
        return None;
    }
    let one_click = header_value(headers, "list-unsubscribe-post").is_some_and(|v| v.to_ascii_lowercase().contains("one-click"));
    Some(ListUnsubscribe { mailto, http, one_click })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_label_prefers_name_over_bare_address() {
        let addr = EmailAddress {
            name: Some("Ada Lovelace".into()),
            address: "ada@example.com".into(),
        };
        assert_eq!(addr.display_label(), "Ada Lovelace");

        let addr = EmailAddress {
            name: None,
            address: "ada@example.com".into(),
        };
        assert_eq!(addr.display_label(), "ada@example.com");

        let addr = EmailAddress {
            name: Some("  ".into()),
            address: "ada@example.com".into(),
        };
        assert_eq!(addr.display_label(), "ada@example.com");
    }

    #[test]
    fn system_flag_bit_round_trips_through_imap_atoms() {
        for flag in [
            SystemFlagBit::Seen,
            SystemFlagBit::Answered,
            SystemFlagBit::Flagged,
            SystemFlagBit::Deleted,
            SystemFlagBit::Draft,
        ] {
            assert_eq!(SystemFlagBit::from_imap_flag(flag.as_imap_flag()), Some(flag));
        }
        assert_eq!(SystemFlagBit::from_imap_flag("\\Recent"), None);
    }

    #[test]
    fn tag_keyword_round_trips_through_the_namespace() {
        assert_eq!(tag_keyword("work"), "$Lookout-tag-work");
        assert_eq!(tag_key_from_keyword("$Lookout-tag-work"), Some("work"));
        assert_eq!(tag_key_from_keyword("$Lookout-tag-work"), tag_key_from_keyword(&tag_keyword("work")));
        // Not ours, or not a keyword at all.
        assert_eq!(tag_key_from_keyword("$Other-tag-work"), None);
        assert_eq!(tag_key_from_keyword("\\Seen"), None);
        assert_eq!(tag_key_from_keyword("work"), None);
    }

    #[test]
    fn sanitize_tag_key_produces_legal_atoms() {
        assert_eq!(sanitize_tag_key("Work"), "work");
        assert_eq!(sanitize_tag_key("Project: Xmas"), "project-xmas");
        assert_eq!(sanitize_tag_key("  IMPORTANT  "), "important");
        assert_eq!(sanitize_tag_key("a{b}*\"\\%"), "a-b");
        assert_eq!(sanitize_tag_key("!!!"), "");
        // The RFC 3501 reserved atom characters never survive sanitization.
        for c in "(){%*\"\\".chars() {
            assert!(!sanitize_tag_key(&format!("x{c}y")).contains(c));
        }
    }

    #[test]
    fn cid_matches_exact_references() {
        // The common case: the HTML references the Content-ID verbatim.
        assert!(cid_matches("logo123", "logo123"));
        assert!(cid_matches("logo123@host.example", "logo123@host.example"));
        // Angle brackets from the RFC 2392 msg-id form are trimmed away.
        assert!(cid_matches("logo123", "<logo123>"));
        assert!(cid_matches("<logo123@host.example>", "logo123@host.example"));
        // Whitespace padding is ignored.
        assert!(cid_matches(" logo123 ", "logo123"));
    }

    #[test]
    fn cid_matches_percent_encoded_references() {
        // `@` in the HTML reference can arrive percent-encoded, either side
        // of the comparison.
        assert!(cid_matches("logo%40123", "logo@123"));
        assert!(cid_matches("logo@123", "logo%40123"));
        assert!(cid_matches("logo%40123", "<logo@123>"));
    }

    #[test]
    fn cid_matches_local_part_of_a_hosted_id() {
        // HTML referencing just the local part resolves the full msg-id.
        assert!(cid_matches("logo123", "logo123@host.example"));
        // The same leniency when the reference keeps a prefix of the host.
        assert!(cid_matches("image001.png@01D8E9B3", "image001.png@01D8E9B3.host"));
        // Percent-encoded `@` decodes before the local parts are compared.
        assert!(cid_matches("logo%40123", "logo@123@host.example"));
    }

    #[test]
    fn cid_matches_rejects_unrelated_ids() {
        assert!(!cid_matches("logo123", "logo124"));
        assert!(!cid_matches("logo1", "logo12@host"));
        // A different local part never matches, whatever the host.
        assert!(!cid_matches("logo12@a", "logo123@b"));
        assert!(!cid_matches("image001@x", "image002@y"));
        // Empty on either side never matches.
        assert!(!cid_matches("", "logo123"));
        assert!(!cid_matches("logo123", ""));
        assert!(!cid_matches("<>", "<>"));
    }

    #[test]
    fn header_value_is_case_insensitive() {
        let headers = vec![("Subject".to_string(), "Hi".to_string()), ("LIST-UNSUBSCRIBE".to_string(), "<https://e.com/u>".to_string())];
        assert_eq!(header_value(&headers, "subject"), Some("Hi"));
        assert_eq!(header_value(&headers, "List-Unsubscribe"), Some("<https://e.com/u>"));
        assert_eq!(header_value(&headers, "list-unsubscribe-post"), None);
    }

    #[test]
    fn parse_list_unsubscribe_extracts_mailto_and_http_actions() {
        // Both actions, first of each kind kept.
        let headers = vec![("List-Unsubscribe".to_string(), "<mailto:unsub@list.example>, <https://list.example/unsub?id=7>".to_string())];
        let parsed = parse_list_unsubscribe(&headers).expect("both actions should parse");
        assert_eq!(parsed.mailto.as_deref(), Some("unsub@list.example"));
        assert_eq!(parsed.http.as_deref(), Some("https://list.example/unsub?id=7"));
        assert!(!parsed.one_click);

        // One-click POST capability comes from List-Unsubscribe-Post.
        let mut headers = headers;
        headers.push(("List-Unsubscribe-Post".to_string(), "List-Unsubscribe=One-Click".to_string()));
        assert!(parse_list_unsubscribe(&headers).expect("parses").one_click);

        // Percent-encoded mailto address is decoded (RFC 8058 sends these).
        let encoded = vec![("List-Unsubscribe".to_string(), "<mailto:user%40list.example?subject=unsubscribe>".to_string())];
        let parsed = parse_list_unsubscribe(&encoded).expect("mailto should parse");
        assert_eq!(parsed.mailto.as_deref(), Some("user@list.example"));
        assert_eq!(parsed.http, None);
    }

    #[test]
    fn parse_list_unsubscribe_is_lenient_about_formatting() {
        // Angle brackets optional, whitespace tolerated.
        let bare = vec![("list-unsubscribe".to_string(), "mailto:a@b.example, https://b.example/u".to_string())];
        let parsed = parse_list_unsubscribe(&bare).expect("bare form should parse");
        assert_eq!(parsed.mailto.as_deref(), Some("a@b.example"));
        assert_eq!(parsed.http.as_deref(), Some("https://b.example/u"));

        // Malformed escapes and unknown schemes pass through harmlessly.
        let weird = vec![(
            "list-unsubscribe".to_string(),
            "<mailto:100%%25ok@x.example>, <ftp://x.example>, <mailto:bad%zz@x.example>".to_string(),
        )];
        let parsed = parse_list_unsubscribe(&weird).expect("first valid mailto should parse");
        assert_eq!(parsed.mailto.as_deref(), Some("100%%ok@x.example"));
        assert_eq!(parsed.http, None);
    }

    #[test]
    fn parse_list_unsubscribe_returns_none_without_usable_actions() {
        // No header at all.
        assert_eq!(parse_list_unsubscribe(&[]), None);
        // A header naming only schemes we can't act on.
        let ftp = vec![("list-unsubscribe".to_string(), "<ftp://x.example/unsub>".to_string())];
        assert_eq!(parse_list_unsubscribe(&ftp), None);
        // An empty value.
        let empty = vec![("list-unsubscribe".to_string(), "".to_string())];
        assert_eq!(parse_list_unsubscribe(&empty), None);
    }
}
