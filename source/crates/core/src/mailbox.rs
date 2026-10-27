/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
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

/// The conventional display spelling for a mailbox name. IMAP canonicalizes
/// its special mailbox to "INBOX" (RFC 3501), but clients show it in title
/// case, so `"INBOX"` renders as `"Inbox"` while every other name passes
/// through untouched.
pub fn display_name(name: &str) -> String {
    if name.eq_ignore_ascii_case("INBOX") {
        "Inbox".to_string()
    } else {
        name.to_string()
    }
}

/// Outlook's spelling for a well-known folder role, used when the user hasn't
/// asked to see the server's own folder names (UX-03). `None` for a custom
/// folder, which always keeps its own name.
pub fn role_display_name(role: MailboxRole) -> Option<&'static str> {
    match role {
        MailboxRole::Inbox => Some("Inbox"),
        MailboxRole::Sent => Some("Sent Items"),
        MailboxRole::Drafts => Some("Drafts"),
        MailboxRole::Trash => Some("Deleted Items"),
        MailboxRole::Junk => Some("Junk Email"),
        MailboxRole::Archive => Some("Archive"),
        MailboxRole::Custom => None,
    }
}

/// The name to show for a mailbox: its role's well-known spelling unless
/// `use_server_names` is set, in which case the server's own name is shown
/// (with `INBOX` still title-cased). Custom folders always keep their name.
pub fn display_name_for(name: &str, role: MailboxRole, use_server_names: bool) -> String {
    if use_server_names {
        return display_name(name);
    }
    role_display_name(role).map(str::to_string).unwrap_or_else(|| display_name(name))
}

/// Guesses a [`MailboxRole`] from a folder's display name when the server
/// doesn't advertise `SPECIAL-USE` attributes. Case-insensitive, matches the
/// common names used by Gmail, Outlook/Exchange, Dovecot, and other major
/// IMAP servers - note providers disagree even on the base concept ("Trash"
/// vs Gmail's "Bin" vs Outlook's "Deleted Items"), so every spelling they use
/// needs an entry here.
///
/// This is only a fallback; the result should be user-overridable and the
/// override persisted (see the Folders settings tab in the roadmap).
pub fn guess_role_from_name(name: &str) -> MailboxRole {
    match name.to_ascii_lowercase().as_str() {
        "inbox" => MailboxRole::Inbox,
        "sent" | "sent items" | "sent mail" | "sent messages" => MailboxRole::Sent,
        "drafts" | "draft" => MailboxRole::Drafts,
        "trash" | "bin" | "recycle bin" | "deleted" | "deleted items" | "deleted messages" => MailboxRole::Trash,
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
    fn displays_inbox_in_title_case() {
        assert_eq!(display_name("INBOX"), "Inbox");
        assert_eq!(display_name("inbox"), "Inbox");
        assert_eq!(display_name("Inbox"), "Inbox");
        assert_eq!(display_name("Archive"), "Archive");
        assert_eq!(display_name("Projects"), "Projects");
    }

    #[test]
    fn roles_get_outlooks_names_unless_server_names_are_requested() {
        assert_eq!(display_name_for("Junk Mail", MailboxRole::Junk, false), "Junk Email");
        assert_eq!(display_name_for("Bin", MailboxRole::Trash, false), "Deleted Items");
        assert_eq!(display_name_for("Sent", MailboxRole::Sent, false), "Sent Items");
        assert_eq!(display_name_for("INBOX", MailboxRole::Inbox, false), "Inbox");
        assert_eq!(display_name_for("My Projects", MailboxRole::Custom, false), "My Projects");
    }

    #[test]
    fn server_folder_names_keep_the_providers_spelling() {
        assert_eq!(display_name_for("Junk Mail", MailboxRole::Junk, true), "Junk Mail");
        assert_eq!(display_name_for("Bin", MailboxRole::Trash, true), "Bin");
        assert_eq!(display_name_for("INBOX", MailboxRole::Inbox, true), "Inbox");
    }

    #[test]
    fn recognizes_providers_own_trash_spellings() {
        // Gmail names its trash "Bin" (en-GB) / "Trash" (en-US); Outlook
        // uses "Deleted Items", some servers just "Deleted".
        assert_eq!(guess_role_from_name("Bin"), MailboxRole::Trash);
        assert_eq!(guess_role_from_name("TRASH"), MailboxRole::Trash);
        assert_eq!(guess_role_from_name("Deleted"), MailboxRole::Trash);
        assert_eq!(guess_role_from_name("Deleted Messages"), MailboxRole::Trash);
        assert_eq!(guess_role_from_name("Recycle Bin"), MailboxRole::Trash);
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
