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
}

impl EmailSummary {
    pub fn is_unread(&self) -> bool {
        !self.flags.contains(&SystemFlagBit::Seen)
    }

    pub fn is_starred(&self) -> bool {
        self.flags.contains(&SystemFlagBit::Flagged)
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct BodyPart {
    /// IMAP body-part path, e.g. `"1.2"`, used for partial `BODY[]` fetches.
    pub part_number: String,
    pub content_type: String,
    pub filename: Option<String>,
    /// The `Content-ID` for inline `cid:` image resolution, if any.
    pub cid: Option<String>,
    pub size: u32,
    pub is_attachment: bool,
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
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct AuthenticationResults {
    pub spf: Option<SpfResult>,
    pub dkim: Option<DkimResult>,
    pub dmarc: Option<DmarcResult>,
}

/// Fetched lazily on message open, via `BODYSTRUCTURE`-driven partial fetch
/// rather than a whole-`RFC822` download.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
}
