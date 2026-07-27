//! GNOME Online Accounts discovery client.
//!
//! Talks to GOA over the session D-Bus (`org.gnome.OnlineAccounts`) via
//! `zbus` rather than linking `libgoa-1.0` — there is no maintained
//! GIR-generated Rust binding for it, and D-Bus is GNOME's own documented
//! integration path for non-C consumers. We never cache credentials
//! ourselves: passwords and OAuth2 access tokens are fetched fresh from GOA
//! per connection attempt and held only in memory for the session.

mod discovery;
mod error;
mod proxies;

pub use discovery::{AuthMethod, EndpointConfig, GoaClient, GoaMailAccount};
pub use error::{Error, Result};
