/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use serde::{Deserialize, Serialize};

/// Static connection parameters for one protocol endpoint (IMAP or SMTP).
/// Deliberately protocol-agnostic w.r.t. credentials - see [`Credential`].
#[derive(Debug, Clone)]
pub struct EndpointConfig {
    pub host: String,
    pub port: u16,
    pub security: Security,
    pub username: String,
}

impl EndpointConfig {
    /// An endpoint from a GNOME Online Accounts mail account's fields: its
    /// host, the port written into the host (if any), and its `*UseSsl` /
    /// `*UseTls` flags, resolved by [`Security::from_goa_flags`].
    pub fn from_goa(protocol: Protocol, host: &str, port: Option<u16>, use_ssl: bool, use_starttls: bool, username: &str) -> EndpointConfig {
        let (security, port) = Security::from_goa_flags(protocol, use_ssl, use_starttls, port);
        EndpointConfig {
            host: host.to_string(),
            port,
            security,
            username: username.to_string(),
        }
    }
}

/// Which mail protocol an endpoint speaks, for its default ports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Imap,
    Smtp,
}

/// How a connection to an endpoint is secured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Security {
    /// TLS from the first byte (IMAPS on 993, SMTPS on 465).
    ImplicitTls,
    /// A plain connection upgraded with STARTTLS before anything else is
    /// sent. The upgrade is required: a server that refuses it fails the
    /// connection rather than falling back to plaintext.
    StartTls,
    /// No encryption at all - the password crosses the network in the clear.
    /// Only ever used when the account is explicitly configured that way.
    Plain,
}

impl Protocol {
    /// The standard port for `security`: 993/143 for IMAP and 465/587 for
    /// SMTP. A plain connection uses the same port as STARTTLS.
    pub fn default_port(self, security: Security) -> u16 {
        match (self, security) {
            (Protocol::Imap, Security::ImplicitTls) => 993,
            (Protocol::Imap, Security::StartTls | Security::Plain) => 143,
            (Protocol::Smtp, Security::ImplicitTls) => 465,
            (Protocol::Smtp, Security::StartTls | Security::Plain) => 587,
        }
    }
}

impl Security {
    /// The mode Lookout used before it read a security setting, for an
    /// account saved without one: IMAP always over implicit TLS, SMTP by port
    /// (465 implicit, anything else STARTTLS).
    pub fn legacy_for_port(protocol: Protocol, port: u16) -> Security {
        match protocol {
            Protocol::Imap => Security::ImplicitTls,
            Protocol::Smtp if port == 465 => Security::ImplicitTls,
            Protocol::Smtp => Security::StartTls,
        }
    }

    /// The mode and port for a GNOME Online Accounts endpoint, from its
    /// `*UseSsl` (implicit TLS) and `*UseTls` (STARTTLS) flags and the port
    /// written into its host, if any.
    ///
    /// GOA's own account dialog sets at most one flag, and a missing port
    /// takes that mode's standard one. Some providers set both (the Google
    /// provider does for SMTP); then a standard STARTTLS port picks STARTTLS
    /// and anything else keeps [`Security::legacy_for_port`], so those
    /// accounts connect exactly as they did before the flags were read.
    pub fn from_goa_flags(protocol: Protocol, use_ssl: bool, use_starttls: bool, port: Option<u16>) -> (Security, u16) {
        let security = match (use_ssl, use_starttls) {
            (true, false) => Security::ImplicitTls,
            (false, true) => Security::StartTls,
            (false, false) => Security::Plain,
            (true, true) => {
                let starttls_port = protocol.default_port(Security::StartTls);
                match port {
                    Some(port) if port == starttls_port => Security::StartTls,
                    Some(port) => Security::legacy_for_port(protocol, port),
                    None => Security::legacy_for_port(protocol, Self::legacy_default_port(protocol)),
                }
            }
        };
        let port = port.unwrap_or_else(|| match (use_ssl, use_starttls) {
            (true, true) => Self::legacy_default_port(protocol),
            _ => protocol.default_port(security),
        });
        (security, port)
    }

    /// The port Lookout assumed for a GOA endpoint with none in its host
    /// before it read the security flags.
    fn legacy_default_port(protocol: Protocol) -> u16 {
        match protocol {
            Protocol::Imap => 993,
            Protocol::Smtp => 587,
        }
    }

    /// Whether the connection is encrypted at all.
    pub fn is_encrypted(self) -> bool {
        self != Security::Plain
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn goa_flags_pick_the_mode_and_its_standard_port() {
        use Protocol::{Imap, Smtp};
        use Security::{ImplicitTls, Plain, StartTls};
        let cases = [
            // (protocol, UseSsl, UseTls, port in host) -> (security, port)
            ((Imap, true, false, None), (ImplicitTls, 993)),
            ((Imap, false, true, None), (StartTls, 143)),
            ((Imap, false, false, None), (Plain, 143)),
            ((Smtp, true, false, None), (ImplicitTls, 465)),
            ((Smtp, false, true, None), (StartTls, 587)),
            ((Smtp, false, false, None), (Plain, 587)),
            // An explicit port is kept, whatever the mode.
            ((Imap, true, false, Some(1993)), (ImplicitTls, 1993)),
            ((Imap, false, true, Some(1143)), (StartTls, 1143)),
            ((Smtp, false, true, Some(25)), (StartTls, 25)),
            // Both flags (GOA's Google provider sets both for SMTP): what
            // Lookout did before it read them.
            ((Smtp, true, true, None), (StartTls, 587)),
            ((Smtp, true, true, Some(465)), (ImplicitTls, 465)),
            ((Smtp, true, true, Some(587)), (StartTls, 587)),
            ((Smtp, true, true, Some(2525)), (StartTls, 2525)),
            ((Imap, true, true, None), (ImplicitTls, 993)),
            ((Imap, true, true, Some(143)), (StartTls, 143)),
            ((Imap, true, true, Some(993)), (ImplicitTls, 993)),
        ];
        for ((protocol, ssl, tls, port), expected) in cases {
            assert_eq!(
                Security::from_goa_flags(protocol, ssl, tls, port),
                expected,
                "{protocol:?} ssl={ssl} tls={tls} port={port:?}"
            );
        }
    }

    #[test]
    fn legacy_mode_matches_the_old_engine() {
        assert_eq!(Security::legacy_for_port(Protocol::Imap, 993), Security::ImplicitTls);
        assert_eq!(Security::legacy_for_port(Protocol::Imap, 143), Security::ImplicitTls);
        assert_eq!(Security::legacy_for_port(Protocol::Smtp, 465), Security::ImplicitTls);
        assert_eq!(Security::legacy_for_port(Protocol::Smtp, 587), Security::StartTls);
        assert_eq!(Security::legacy_for_port(Protocol::Smtp, 25), Security::StartTls);
    }

    #[test]
    fn security_serializes_as_snake_case() {
        assert_eq!(serde_json::to_string(&Security::ImplicitTls).unwrap(), "\"implicit_tls\"");
        assert_eq!(serde_json::to_string(&Security::StartTls).unwrap(), "\"start_tls\"");
        assert_eq!(serde_json::to_string(&Security::Plain).unwrap(), "\"plain\"");
    }
}
