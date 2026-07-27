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
mod body;
mod config;
mod connection;
mod envelope;
mod error;
pub mod session;

pub use config::{AccountConfig, Credential, EndpointConfig};
pub use error::{Error, Result};
