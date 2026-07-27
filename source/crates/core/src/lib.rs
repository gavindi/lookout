//! Protocol- and UI-agnostic domain types shared by every Lookout crate.
//!
//! Deliberately has no I/O dependencies (no tokio, no zbus, no gtk) so it can
//! be exercised with plain `cargo test` and reused by any future front end.

pub mod email;
pub mod identity;
pub mod ids;
pub mod mailbox;
pub mod thread;

pub use email::{AuthenticationResults, BodyPart, DkimResult, DmarcResult, EmailAddress, EmailBody, EmailSummary, SpfResult, SystemFlagBit};
pub use identity::Identity;
pub use ids::{AccountId, MailboxId, Uid, UidValidity};
pub use mailbox::{Mailbox, MailboxRole};
pub use thread::{ThreadGroup, ThreadKey};
