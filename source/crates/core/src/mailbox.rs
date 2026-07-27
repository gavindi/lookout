use crate::ids::{AccountId, MailboxId, UidValidity};

/// The special-use role of a mailbox, derived from `LIST (SPECIAL-USE)` attributes
/// with a name-heuristic fallback when the server doesn't advertise RFC 6154.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum MailboxRole {
    Inbox,
    Sent,
    Drafts,
    Trash,
    Junk,
    Archive,
    Custom,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Mailbox {
    pub id: MailboxId,
    pub account_id: AccountId,
    /// Display name (the final path segment, decoded from IMAP modified UTF-7).
    pub name: String,
    pub parent: Option<MailboxId>,
    /// The server's hierarchy delimiter for this mailbox (commonly `/` or `.`).
    pub delimiter: char,
    pub role: MailboxRole,
    pub uidvalidity: UidValidity,
    pub uidnext: u32,
    /// Present iff the server advertises CONDSTORE/QRESYNC; enables cheap
    /// incremental resync via `CHANGEDSINCE` instead of a full re-fetch.
    pub highest_modseq: Option<u64>,
    pub total: u32,
    pub unread: u32,
    /// Raw IMAP mailbox flags (e.g. `\Noselect`, `\HasChildren`).
    pub flags: Vec<String>,
    pub subscribed: bool,
}

/// Guesses a [`MailboxRole`] from a folder's display name when the server
/// doesn't advertise `SPECIAL-USE` attributes. Case-insensitive, matches the
/// common names used by Gmail, Dovecot, and other major IMAP servers.
///
/// This is only a fallback; the result should be user-overridable and the
/// override persisted (see the Folders settings tab in the roadmap).
pub fn guess_role_from_name(name: &str) -> MailboxRole {
    match name.to_ascii_lowercase().as_str() {
        "inbox" => MailboxRole::Inbox,
        "sent" | "sent items" | "sent mail" | "sent messages" => MailboxRole::Sent,
        "drafts" | "draft" => MailboxRole::Drafts,
        "trash" | "deleted items" | "deleted messages" => MailboxRole::Trash,
        "junk" | "spam" | "junk e-mail" | "bulk mail" => MailboxRole::Junk,
        "archive" | "all mail" => MailboxRole::Archive,
        _ => MailboxRole::Custom,
    }
}

/// Parses the `\SpecialUse` attribute names returned by `LIST (SPECIAL-USE)`
/// (RFC 6154) into a role, falling back to a name guess if none apply.
pub fn role_from_special_use(attrs: &[String], name: &str) -> MailboxRole {
    for attr in attrs {
        match attr.as_str() {
            "\\Inbox" => return MailboxRole::Inbox,
            "\\Sent" => return MailboxRole::Sent,
            "\\Drafts" => return MailboxRole::Drafts,
            "\\Trash" => return MailboxRole::Trash,
            "\\Junk" => return MailboxRole::Junk,
            "\\Archive" | "\\All" => return MailboxRole::Archive,
            _ => {}
        }
    }
    guess_role_from_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guesses_common_role_names_case_insensitively() {
        assert_eq!(guess_role_from_name("INBOX"), MailboxRole::Inbox);
        assert_eq!(guess_role_from_name("Sent Items"), MailboxRole::Sent);
        assert_eq!(guess_role_from_name("Junk E-mail"), MailboxRole::Junk);
        assert_eq!(guess_role_from_name("Deleted Items"), MailboxRole::Trash);
        assert_eq!(guess_role_from_name("Projects"), MailboxRole::Custom);
    }

    #[test]
    fn special_use_attr_wins_over_name_guess() {
        let attrs = vec!["\\HasChildren".to_string(), "\\Junk".to_string()];
        assert_eq!(role_from_special_use(&attrs, "Some Weird Name"), MailboxRole::Junk);
    }

    #[test]
    fn falls_back_to_name_guess_when_no_special_use_attrs() {
        let attrs: Vec<String> = vec![];
        assert_eq!(role_from_special_use(&attrs, "Trash"), MailboxRole::Trash);
    }
}
