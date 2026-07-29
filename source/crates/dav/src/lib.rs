//! Minimal CalDAV client + iCalendar/RRULE parsing + a `CalendarSession`
//! actor, mirroring `lookout-mail`'s IMAP session actor. Deliberately has no
//! dependency on `lookout-goa`/`zbus`: credentials are supplied through the
//! [`session::CalendarCredentialProvider`] trait, keeping this crate free of
//! D-Bus concerns.

mod client;
mod config;
mod error;
mod ical;
mod recurrence;
pub mod session;
mod xml;

pub use config::{CalendarAccountConfig, Credential};
pub use error::{Error, Result};
