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

pub use calendar::{
    Attendee, AttendeeRole, AttendeeStatus, CalendarEvent, CalendarInfo, CalendarTask, EventOccurrence, EventSensitivity, EventTransparency, TaskPriority, TaskStatus,
    WebcalSubscription,
};
pub use email::{
    cid_matches, header_value, parse_imip_method, parse_list_unsubscribe, sanitize_tag_key, tag_key_from_keyword, tag_keyword, AuthenticationResults, BodyPart, ContactsProvider,
    DkimResult, DmarcResult, EmailAddress, EmailBody, EmailSummary, ImipInvitation, ImipMethod, ListUnsubscribe, SpfResult, SystemFlagBit, TAG_KEYWORD_PREFIX,
};
pub use identity::Identity;
pub use ids::{AccountId, CalendarId, EventUid, MailboxId, TaskUid, Uid, UidValidity};
pub use mailbox::{Mailbox, MailboxRole};
pub use thread::{ThreadGroup, ThreadKey};
pub use vcard::{AddressField, EmailField, Name, OtherProperty, Parameter, TelephoneField, VCard, VCardError};
