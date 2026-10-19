//! GNOME Evolution Data Server (EDS) client.
//!
//! Talks to EDS over the session D-Bus via `zbus` rather than linking
//! `libecal`/`libebook`, the same way `lookout-goa` reaches GNOME Online
//! Accounts. EDS's D-Bus interfaces are internal and carry a version in their
//! bus names; this crate knows the set shipped by every EDS release since 3.34
//! (`Sources5`, `Calendar8`, `AddressBook10` - see EDS_TODO.md, "E0.1 spike
//! notes") and reports EDS as unavailable when the session runs anything else.
//!
//! Every failure here is meant to be "EDS unavailable" to callers outside the
//! crate - never an error dialog. [`Eds::connect`] returns `Ok(None)` for the
//! expected cases (no EDS on the bus, an unknown interface version) and only
//! errs when D-Bus itself misbehaves, which callers treat the same way.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
mod discovery;
mod error;
mod keyfile;
pub mod proxies;
mod sources;

pub use discovery::Eds;
pub use error::{Error, Result};
pub use sources::{EdsSource, EdsSourceKind};
