//! Minimal CalDAV client + iCalendar/RRULE parsing + a `CalendarSession`
//! actor, mirroring `lookout-mail`'s IMAP session actor. Deliberately has no
//! dependency on `lookout-goa`/`zbus`: credentials are supplied through the
//! [`session::CalendarCredentialProvider`] trait, keeping this crate free of
//! D-Bus concerns.

pub mod cache;
mod client;
mod config;
mod error;
mod ical;
mod recurrence;
pub mod session;
pub mod subscription;
mod xml;

pub use cache::{cache_info, clear_all_caches, remove_subscription_cache};
pub use client::{fetch_webcal_ics, normalize_webcal_url, AddressBookInfo, DavClient, MAX_FEED_BYTES};
pub use config::{CalendarAccountConfig, CardDavAccountConfig, Credential};
pub use error::{Error, Result};
pub use ical::{build_imip_vcalendar, build_vcalendar, parse_imip_invitation, parse_vevents, parse_vevents_with_meta};
