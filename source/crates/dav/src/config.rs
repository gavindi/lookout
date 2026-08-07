/// Static connection parameters for one CalDAV account. Deliberately its own
/// type rather than a reuse of `lookout_mail::config::AccountConfig` - this
/// crate is D-Bus- and IMAP/SMTP-free by the same convention.
#[derive(Debug, Clone)]
pub struct CalendarAccountConfig {
    pub account_id: lookout_core::AccountId,
    pub display_name: String,
    /// GOA's `Calendar.Uri` - the CalDAV base URL to start discovery from.
    pub base_url: String,
    pub accept_ssl_errors: bool,
    /// Username to pair with a `Credential::Password` over HTTP Basic Auth.
    /// Unlike Mail (which has a protocol-specific `ImapUserName`), GOA's
    /// `Calendar` interface has no username property of its own - this is
    /// the account's `Identity` (the login the DAV server actually expects,
    /// e.g. the Nextcloud user id), never the display name. Unused for
    /// `Credential::OAuth2AccessToken` (Bearer auth needs no separate
    /// username).
    pub username: String,
}

/// Static connection parameters for one CardDAV account.
#[derive(Debug, Clone)]
pub struct CardDavAccountConfig {
    pub account_id: lookout_core::AccountId,
    pub display_name: String,
    /// GOA's `Contacts.Uri` / CardDAV base URL to start discovery from.
    pub base_url: String,
    pub accept_ssl_errors: bool,
    /// Username to pair with a `Credential::Password` over HTTP Basic Auth.
    /// Unused for `Credential::OAuth2AccessToken`.
    pub username: String,
}

/// A credential obtained from GOA immediately before use. `lookout-dav` never
/// persists these; the caller (the app crate, via `lookout-goa`) is
/// responsible for fetching a fresh one on every (re)connect.
#[derive(Clone)]
pub enum Credential {
    Password(String),
    OAuth2AccessToken(String),
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Credential::Password(_) => write!(f, "Password(..)"),
            Credential::OAuth2AccessToken(_) => write!(f, "OAuth2AccessToken(..)"),
        }
    }
}
