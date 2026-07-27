//! Raw D-Bus proxy definitions for the interfaces GOA exposes on the session
//! bus (`org.gnome.OnlineAccounts`). Kept separate from `discovery.rs` so the
//! wire-level shape is easy to audit against GNOME's published D-Bus
//! reference independently of our own account-model mapping.

use std::collections::HashMap;

use zbus::proxy;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

/// The shape returned by `ObjectManager.GetManagedObjects`: object path ->
/// interface name -> property name -> value.
pub type ManagedObjects = HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>>;

#[proxy(
    interface = "org.freedesktop.DBus.ObjectManager",
    default_service = "org.gnome.OnlineAccounts",
    default_path = "/org/gnome/OnlineAccounts"
)]
pub trait ObjectManager {
    fn get_managed_objects(&self) -> zbus::Result<ManagedObjects>;

    #[zbus(signal)]
    fn interfaces_added(&self, object_path: OwnedObjectPath, interfaces: HashMap<String, HashMap<String, OwnedValue>>) -> zbus::Result<()>;

    #[zbus(signal)]
    fn interfaces_removed(&self, object_path: OwnedObjectPath, interfaces: Vec<String>) -> zbus::Result<()>;
}

/// `org.gnome.OnlineAccounts.Account` — bound to a specific account object
/// path per call, since (unlike `ObjectManager`) there's no single default path.
#[proxy(interface = "org.gnome.OnlineAccounts.Account", default_service = "org.gnome.OnlineAccounts")]
pub trait Account {
    /// Ensures cached credentials are valid, refreshing them if needed
    /// (OAuth2 token refresh, etc). Returns an error if the account needs
    /// user attention (e.g. reauthentication in GNOME Settings).
    fn ensure_credentials(&self) -> zbus::Result<i32>;
}

/// `org.gnome.OnlineAccounts.PasswordBased`.
#[proxy(interface = "org.gnome.OnlineAccounts.PasswordBased", default_service = "org.gnome.OnlineAccounts")]
pub trait PasswordBased {
    /// `id` is a provider-defined credential slot, e.g. `"imap-password"` /
    /// `"smtp-password"` for mail accounts (falls back to `"password"` for
    /// providers that don't split IMAP/SMTP credentials).
    fn get_password(&self, id: &str) -> zbus::Result<String>;
}

/// `org.gnome.OnlineAccounts.OAuth2Based`.
#[proxy(interface = "org.gnome.OnlineAccounts.OAuth2Based", default_service = "org.gnome.OnlineAccounts")]
pub trait OAuth2Based {
    /// Returns `(access_token, expires_in_seconds)`. Always call fresh —
    /// GOA's daemon transparently refreshes the underlying token.
    fn get_access_token(&self) -> zbus::Result<(String, i32)>;
}
