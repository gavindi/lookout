/// Static connection parameters for one protocol endpoint (IMAP or SMTP).
/// Deliberately protocol-agnostic w.r.t. credentials - see [`Credential`].
#[derive(Debug, Clone)]
pub struct EndpointConfig {
    pub host: String,
    pub port: u16,
    pub use_tls: bool,
    pub username: String,
}

/// A credential obtained from GOA immediately before use. `lookout-mail`
/// never persists these; the caller (the app crate, via `lookout-goa`) is
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

#[derive(Debug, Clone)]
pub struct AccountConfig {
    pub account_id: lookout_core::AccountId,
    pub display_name: String,
    pub email: String,
    pub imap: EndpointConfig,
    pub smtp: EndpointConfig,
}
