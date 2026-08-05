//! IMAP/SMTP account-session actor and local cache.
//!
//! One [`session::run_account_session`] future runs per connected mail
//! account, driven by [`session::AccountCommand`]s and emitting
//! [`session::AccountEvent`]s over `async_channel` - see that module's docs
//! for the actor/IDLE design. Deliberately has no dependency on
//! `lookout-goa`/`zbus`: credentials are supplied through the
//! [`session::CredentialProvider`] trait so this crate can be exercised
//! against a real IMAP server (e.g. in `tests/imap_integration.rs`) without
//! any GNOME/D-Bus session present.

mod auth;
pub mod body;
mod cache;
mod config;
mod connection;
mod envelope;
mod error;
pub mod send;
pub mod session;

// `Cache` itself is public so the app crate can open a read-side handle for
// the composer's recipient autocomplete (`search_addresses`), which must not
// go through the account session - a keystroke can't wait behind whatever
// IMAP round trip that actor is mid-way through.
pub use cache::{cache_info, clear_all_caches, Cache};
pub use config::{AccountConfig, Credential, EndpointConfig};
pub use error::{Error, Result};
pub use send::{new_message_id, ComposedMessage};
