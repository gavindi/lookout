//! Protocol- and UI-agnostic domain types shared by every Lookout crate.
//!
//! Deliberately has no I/O dependencies (no tokio, no zbus, no gtk) so it can
//! be exercised with plain `cargo test` and reused by any future front end.

pub mod calendar;
pub mod email;
pub mod identity;
pub mod ids;
pub mod mailbox;
pub mod thread;
pub mod vcard;

pub use calendar::{CalendarEvent, CalendarInfo, EventOccurrence};
pub use email::{
    sanitize_tag_key, tag_key_from_keyword, tag_keyword, AuthenticationResults, BodyPart, ContactsProvider, DkimResult, DmarcResult, EmailAddress, EmailBody, EmailSummary,
    SpfResult, SystemFlagBit, TAG_KEYWORD_PREFIX,
};
pub use identity::Identity;
pub use ids::{AccountId, CalendarId, EventUid, MailboxId, Uid, UidValidity};
pub use mailbox::{Mailbox, MailboxRole};
pub use thread::{ThreadGroup, ThreadKey};
pub use vcard::{AddressField, EmailField, Name, OtherProperty, Parameter, TelephoneField, VCard, VCardError};
