/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
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
    /// Whether the message carries an iCalendar (`text/calendar`) part - the
    /// iMIP path's cue that this is a meeting invitation (or related message)
    /// rather than ordinary mail. Derived from `structure` like
    /// `has_attachment`, so a `None` structure leaves both false.
    #[serde(default)]
    pub has_calendar: bool,
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

    pub fn is_pinned(&self) -> bool {
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

    /// Whether this part carries an iCalendar document (`text/calendar`, RFC
    /// 5545 §3.2). The iMIP path fetches these parts alongside the text parts
    /// and surfaces the invitation through the reading pane's banner rather
    /// than rendering the raw ICS as body text.
    pub fn is_calendar(&self) -> bool {
        self.content_type.eq_ignore_ascii_case("text/calendar")
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
    /// The decoded contents of the message's `text/calendar` part (if any) -
    /// the iMIP invitation payload the reading pane's banner acts on. Never
    /// fetched for display; `None` for the vast majority of messages.
    #[serde(default)]
    pub calendar_ics: Option<String>,
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

/// Parses a message's `Disposition-Notification-To` header (RFC 8098 §2.1.1)
/// into the addresses the sender wants a read receipt sent to. Returns an
/// empty vec when the header is missing or names no usable address. The
/// header is an RFC 5322 address list, so each comma-separated item may be a
/// bare address or `Display Name <address>`; parsing is deliberately lenient
/// (real-world senders put spaces, stray brackets and the like around the
/// addresses) - an item yields its address when an angle-bracketed or bare
/// token containing an `@` can be extracted, junk items are dropped, and
/// duplicates are kept (senders list each recipient they want notified).
pub fn parse_disposition_notification_to(headers: &[(String, String)]) -> Vec<String> {
    let Some(raw) = header_value(headers, "disposition-notification-to") else {
        return Vec::new();
    };
    let mut addresses = Vec::new();
    for item in raw.split(',') {
        if let Some(address) = dnt_address(item) {
            addresses.push(address);
        }
    }
    addresses
}

/// Extracts one routable address from a comma-separated item of a
/// `Disposition-Notification-To` value, or `None` when the item names no
/// usable address. Accepts `Display Name <address>`, bare `<address>`, a
/// bare address, and the stray-bracket variants real-world senders produce
/// (`<<address>>`, `name<address>`); junk items are dropped.
fn dnt_address(item: &str) -> Option<String> {
    let item = item.trim();
    // `Name <address>` (or `<address>`): the address is the last
    // angle-bracketed group - but only when the group is well-formed,
    // otherwise the item may still be a bare address below.
    if let Some(start) = item.rfind('<') {
        if let Some(addr) = item[start + 1..].strip_suffix('>') {
            let addr = addr.trim();
            if looks_like_address(addr) {
                return Some(addr.to_string());
            }
        }
    }
    // Bare address with one or more surrounding bracket pairs.
    let mut candidate = item;
    loop {
        let stripped = candidate.strip_prefix('<').and_then(|r| r.strip_suffix('>')).unwrap_or(candidate).trim();
        if stripped == candidate {
            break;
        }
        candidate = stripped;
    }
    looks_like_address(candidate).then(|| candidate.to_string())
}

/// Whether a value plausibly names an email address: exactly one `@` with
/// non-empty sides, and no whitespace, brackets, or delimiter characters -
/// the checks that keep `Name <addr>` fragments and dangling brackets out of
/// the parsed list. Quoted local parts are not validated further; the
/// receipt target just needs to be routable enough for SMTP to accept it (a
/// malformed address then fails at send time and toasts, rather than
/// silently skipping the receipt).
fn looks_like_address(value: &str) -> bool {
    let mut parts = value.split('@');
    let local = parts.next().unwrap_or("");
    let domain = parts.next().unwrap_or("");
    if local.is_empty() || domain.is_empty() || parts.next().is_some() {
        return false;
    }
    let clean = |part: &str| part.chars().all(|c| !c.is_whitespace() && !"<>(),;:\"\\".contains(c));
    clean(local) && clean(domain)
}

/// Whether the message is machine-generated (RFC 3834 `Auto-Submitted`):
/// `auto-generated` and `auto-replied` mark automated mail that must never
/// receive an automated response, or the response would loop. An absent
/// header (the common case for human mail) is *not* auto-submitted; `no` is
/// the explicit RFC 3834 value for human-originated mail and is not either.
pub fn is_auto_submitted(headers: &[(String, String)]) -> bool {
    header_value(headers, "auto-submitted")
        .map(|v| {
            let v = v.trim().to_ascii_lowercase();
            v.starts_with("auto-generated") || v.starts_with("auto-replied")
        })
        .unwrap_or(false)
}

/// Whether the message is itself a return receipt (or other report) - the
/// `multipart/report` test of RFC 8098 §2.1.4. Receipts for reports are
/// never generated; only a message whose *top-level* content type is a
/// report is caught (subparts may legitimately carry `message/rfc822` and
/// other embedded types).
pub fn is_report_message(headers: &[(String, String)]) -> bool {
    header_value(headers, "content-type")
        .map(|v| v.to_ascii_lowercase().contains("multipart/report"))
        .unwrap_or(false)
}

/// The iMIP `METHOD` property (RFC 5546 §3.2): what an email carrying a
/// `text/calendar` part asks the recipient to do. `REQUEST` is an invitation
/// (or re-invitation), `REPLY` carries an attendee's RSVP back to the
/// organizer, and `CANCEL` withdraws an event. The banner and its reply
/// actions are keyed on this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ImipMethod {
    Request,
    Reply,
    Cancel,
}

/// Reads the `METHOD` property off an iCalendar document as
/// [`ImipMethod`]. Per RFC 5546 §4.3, an email invitation with no `METHOD`
/// at all is a REQUEST, so anything missing or unrecognized resolves there
/// rather than failing. The scan only matches at the calendar level - a
/// `METHOD` property can never legally appear inside a `VEVENT`, but a
/// defensively-written scanner shouldn't pick one up from a nested component
/// either. iCalendar lines are short enough that `METHOD` never participates
/// in RFC 5545 line folding, so a plain line scan is sufficient.
pub fn parse_imip_method(ics: &str) -> ImipMethod {
    let mut depth = 0usize;
    for line in ics.lines() {
        let upper = line.trim().to_ascii_uppercase();
        if let Some(rest) = upper.strip_prefix("BEGIN:") {
            depth += usize::from(rest != "VCALENDAR");
            continue;
        }
        if let Some(_rest) = upper.strip_prefix("END:") {
            depth = depth.saturating_sub(1);
            continue;
        }
        if depth == 0 {
            if let Some(value) = upper.strip_prefix("METHOD:").map(str::trim) {
                return match value {
                    "REPLY" => ImipMethod::Reply,
                    "CANCEL" => ImipMethod::Cancel,
                    _ => ImipMethod::Request,
                };
            }
        }
    }
    ImipMethod::Request
}

/// An iMIP invitation (or related message) carried by a message's
/// `text/calendar` part, as surfaced to the reading pane's banner. The raw
/// `ics` is kept so the reply/removal flows can re-serialize the event with
/// the recipient's updated `PARTSTAT` (see `lookout-dav`'s
/// `build_imip_vcalendar`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImipInvitation {
    pub method: ImipMethod,
    /// The decoded iCalendar document from the message's `text/calendar` part.
    pub ics: String,
    /// The invited event's summary, for the banner's label.
    pub summary: Option<String>,
    /// The organizer's address (from `ORGANIZER`) - the reply's recipient.
    pub organizer: Option<EmailAddress>,
    /// The message's own `Message-ID`, threaded onto the reply's
    /// `In-Reply-To`/`References`. Filled in by the reading pane, which has
    /// the message's headers where the parser only has the iCalendar part.
    pub in_reply_to: Option<String>,
    /// The invited event's start, in UTC - the first instance's `DTSTART`
    /// (for a recurring invitation this is the master's start, not an
    /// occurrence's).
    pub start: chrono::DateTime<chrono::Utc>,
    /// The invited event's end, in UTC - `DTEND`, exclusive like `DTSTART`.
    pub end: chrono::DateTime<chrono::Utc>,
    /// Whether the event is an all-day entry (`DTSTART;VALUE=DATE`), which
    /// the details card renders as a date instead of a time range.
    pub all_day: bool,
    /// The event's `LOCATION`, if any.
    pub location: Option<String>,
    /// The event's `DESCRIPTION`, if any.
    pub description: Option<String>,
    /// The raw `RRULE` value (RFC 5545 `RECUR`) when the invitation is for a
    /// recurring event - the details card shows a "recurring" hint for it.
    pub rrule: Option<String>,
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

    #[test]
    fn parse_disposition_notification_to_extracts_addresses() {
        // Bare address.
        let headers = vec![("Disposition-Notification-To".to_string(), "sender@example.com".to_string())];
        assert_eq!(parse_disposition_notification_to(&headers), vec!["sender@example.com".to_string()]);

        // Display-name form.
        let headers = vec![("Disposition-Notification-To".to_string(), "Sally Sender <sender@example.com>".to_string())];
        assert_eq!(parse_disposition_notification_to(&headers), vec!["sender@example.com".to_string()]);

        // Multiple addresses, whitespace tolerated.
        let headers = vec![("disposition-notification-to".to_string(), " a@b.example, <c@d.example>, Name <e@f.example> ".to_string())];
        assert_eq!(
            parse_disposition_notification_to(&headers),
            vec!["a@b.example".to_string(), "c@d.example".to_string(), "e@f.example".to_string()]
        );
    }

    #[test]
    fn parse_disposition_notification_to_drops_junk() {
        // Missing header -> empty.
        assert!(parse_disposition_notification_to(&[]).is_empty());
        // Empty value.
        let empty = vec![("Disposition-Notification-To".to_string(), "".to_string())];
        assert!(parse_disposition_notification_to(&empty).is_empty());
        // A list of unusable items yields nothing; the one valid item survives.
        let junk = vec![("Disposition-Notification-To".to_string(), "not an address, Name <>, valid@example.com".to_string())];
        assert_eq!(parse_disposition_notification_to(&junk), vec!["valid@example.com".to_string()]);
        // Dangling brackets and double angles around a bare address.
        let messy = vec![("Disposition-Notification-To".to_string(), "<<messy@example.com>>".to_string())];
        assert_eq!(parse_disposition_notification_to(&messy), vec!["messy@example.com".to_string()]);
    }

    #[test]
    fn parse_disposition_notification_to_keeps_duplicates() {
        let headers = vec![("Disposition-Notification-To".to_string(), "a@b.example, a@b.example".to_string())];
        assert_eq!(parse_disposition_notification_to(&headers), vec!["a@b.example".to_string(), "a@b.example".to_string()]);
    }

    #[test]
    fn is_auto_submitted_detects_rfc_3834_machine_mail() {
        for value in ["auto-generated", "auto-replied", "Auto-Generated", "auto-replied; foo"] {
            let headers = vec![("Auto-Submitted".to_string(), value.to_string())];
            assert!(is_auto_submitted(&headers), "{value:?}");
        }
        // Absent, explicit `no`, and unknown values are all human mail.
        assert!(!is_auto_submitted(&[]));
        for value in ["no", "", "yes", "auto-notified"] {
            let headers = vec![("Auto-Submitted".to_string(), value.to_string())];
            assert!(!is_auto_submitted(&headers), "{value:?}");
        }
    }

    #[test]
    fn is_report_message_detects_multipart_report() {
        let headers = vec![(
            "Content-Type".to_string(),
            "multipart/report; report-type=disposition-notification; boundary=xyz".to_string(),
        )];
        assert!(is_report_message(&headers));
        let plain = vec![("Content-Type".to_string(), "multipart/alternative; boundary=xyz".to_string())];
        assert!(!is_report_message(&plain));
        assert!(!is_report_message(&[]));
    }

    #[test]
    fn parse_imip_method_reads_the_calendar_level_method() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:x@y\r\nDTSTAMP:20260101T000000Z\r\nDTSTART:20260715T140000Z\r\nSUMMARY:Hi\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        assert_eq!(parse_imip_method(ics), ImipMethod::Request);

        let reply = ics.replace("METHOD:REQUEST", "METHOD:REPLY");
        assert_eq!(parse_imip_method(&reply), ImipMethod::Reply);

        let cancel = ics.replace("METHOD:REQUEST", "METHOD:CANCEL");
        assert_eq!(parse_imip_method(&cancel), ImipMethod::Cancel);
    }

    #[test]
    fn parse_imip_method_defaults_to_request_without_a_method() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\nBEGIN:VEVENT\r\nUID:x@y\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        assert_eq!(parse_imip_method(ics), ImipMethod::Request);
    }

    #[test]
    fn parse_imip_method_is_case_insensitive() {
        let ics = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nmethod:reply\r\nEND:VCALENDAR\r\n";
        assert_eq!(parse_imip_method(ics), ImipMethod::Reply);
    }

    #[test]
    fn parse_imip_method_ignores_methods_inside_nested_components() {
        // A METHOD property nested inside a VEVENT is illegal RFC 5545, but a
        // defensive scan must not pick it up over the calendar-level one.
        let ics = "BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nMETHOD:CANCEL\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
        assert_eq!(parse_imip_method(ics), ImipMethod::Request);
    }

    #[test]
    fn parse_imip_method_tolerates_windows_line_endings() {
        let ics = "BEGIN:VCALENDAR\r\nMETHOD:REPLY\r\n";
        assert_eq!(parse_imip_method(ics), ImipMethod::Reply);
    }

    #[test]
    fn body_part_calendar_detection_is_case_insensitive() {
        let mut part = BodyPart {
            part_number: "2".to_string(),
            content_type: "text/calendar".to_string(),
            charset: None,
            transfer_encoding: None,
            filename: None,
            cid: None,
            size: 0,
            is_attachment: false,
        };
        assert!(part.is_calendar());
        part.content_type = "Text/Calendar".to_string();
        assert!(part.is_calendar());
        part.content_type = "text/plain".to_string();
        assert!(!part.is_calendar());
    }
}
