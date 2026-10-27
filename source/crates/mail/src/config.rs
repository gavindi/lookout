/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
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
    /// The user's own addresses for `@mentions` detection (M-25): the account
    /// address plus any configured identity (send-as) aliases. The session
    /// compiles these into a [`lookout_core::MentionMatcher`] and scans body
    /// snippets against them.
    pub mention_addresses: Vec<String>,
    /// The user's names for `@mentions`: the display name, its first word and
    /// address local-parts, so `@Name`, `@First` and `@local-part` all match.
    pub mention_names: Vec<String>,
    /// Whether the local Focused Inbox classifier runs for this account
    /// (M-03 / item 2.12). Only ever set for non-Microsoft accounts; Microsoft
    /// 365 gets its classification from Graph.
    pub local_focus_enabled: bool,
    /// The user's known contact addresses (lowercased), fed to the local
    /// classifier so a message from a contact stays Focused even when it
    /// carries a `List-Id`.
    pub local_focus_contacts: Vec<String>,
    pub imap: EndpointConfig,
    pub smtp: EndpointConfig,
}
