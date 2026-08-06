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
}
