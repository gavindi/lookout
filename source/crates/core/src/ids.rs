use std::fmt;

/// Derived from a GOA account object path, e.g. `/org/gnome/OnlineAccounts/Accounts/account_1234`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct AccountId(pub String);

impl fmt::Display for AccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// An IMAP UID. Only unique within a given `(MailboxId, UidValidity)` pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct Uid(pub u32);

/// IMAP UIDVALIDITY for a mailbox. If this changes, all previously cached UIDs
/// must be discarded and re-synced from scratch (RFC 3501 §2.3.1.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct UidValidity(pub u32);

/// Account-scoped identifier for a mailbox (IMAP folder), synthesized from the
/// account id and decoded folder path since IMAP has no opaque folder id.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct MailboxId(pub String);

impl fmt::Display for MailboxId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl MailboxId {
    pub fn new(account_id: &AccountId, folder_path: &str) -> Self {
        MailboxId(format!("{}:{}", account_id.0, folder_path))
    }
}

/// Account-scoped identifier for a CalDAV calendar collection, synthesized
/// from the account id and the collection's href since CalDAV has no opaque
/// calendar id of its own (mirrors `MailboxId`'s synthesis for IMAP folders).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct CalendarId(pub String);

impl fmt::Display for CalendarId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl CalendarId {
    pub fn new(account_id: &AccountId, calendar_href: &str) -> Self {
        CalendarId(format!("{}:{}", account_id.0, calendar_href))
    }
}

/// An iCalendar `UID` (RFC 5545 §3.8.4.7). Only unique within a given
/// `CalendarId` - the same UID string could coincidentally appear in a
/// different calendar/account.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct EventUid(pub String);

impl fmt::Display for EventUid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}
