//! Evolution's mail accounts, read so Lookout can offer to import them as
//! manual IMAP/SMTP accounts (EDS_TODO.md, E3.1).
//!
//! Evolution splits one mail account across three sources:
//!
//! - the **account** (`[Mail Account]`, backend `imapx`), whose
//!   `[Authentication]` and `[Security]` groups hold the IMAP server and whose
//!   `IdentityUid` names the identity;
//! - the **identity** (`[Mail Identity]`), with the sender's name and address,
//!   whose `[Mail Submission] TransportUid` names the transport;
//! - the **transport** (`[Mail Transport]`, backend `smtp`), with the SMTP
//!   server in the same two groups.
//!
//! The key files come from each source's `Data` property over D-Bus rather
//! than from `~/.config/evolution/sources`, so the Flatpak needs no
//! filesystem permission (E0.1). Every source's UID is kept, because
//! Evolution's keyring items are keyed by it (`e-source-uid`, E3.3).

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::collections::HashMap;

use crate::keyfile::KeyFile;
use crate::sources::{goa_account_id, managed_sources, parse_key_files};
use crate::{Eds, Result};

/// How a server connection is secured - Evolution's `[Security] Method`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailSecurity {
    /// `none`: a plain connection.
    None,
    /// `starttls-on-standard-port`: plain, upgraded with STARTTLS.
    StartTls,
    /// `ssl-on-alternate-port`: TLS from the first byte (IMAPS, SMTPS).
    Tls,
}

impl MailSecurity {
    /// A missing method is `none`, ESourceSecurity's default. An unknown one
    /// counts as TLS, matching ESourceSecurity's `secure` property (any method
    /// but `none`), so an import never quietly drops encryption.
    fn from_method(method: Option<&str>) -> MailSecurity {
        match method.map(str::trim) {
            None | Some("") | Some("none") => MailSecurity::None,
            Some("starttls-on-standard-port") => MailSecurity::StartTls,
            Some(_) => MailSecurity::Tls,
        }
    }
}

/// One server, IMAP or SMTP, as Evolution configured it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailServer {
    /// The source this server came from - the account for IMAP, the transport
    /// for SMTP. Evolution's keyring item for its password carries this as
    /// `e-source-uid`.
    pub source_uid: String,
    pub host: String,
    /// The configured port, or the protocol's usual one for `security` when
    /// the source leaves it out (or at 0).
    pub port: u16,
    /// Empty when the server needs no login (an SMTP relay, for example).
    pub user: String,
    pub security: MailSecurity,
}

/// An Evolution mail account that Lookout could import: an IMAP account with
/// password login that doesn't belong to GNOME Online Accounts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvolutionMailAccount {
    /// The account's name in Evolution's account list (often the address).
    pub account_name: String,
    /// The identity's sender name, when it has one.
    pub name: Option<String>,
    /// The identity's address, when the identity is found and has one.
    pub address: Option<String>,
    pub imap: MailServer,
    /// `None` when the identity names no transport, the transport is missing,
    /// or it isn't an SMTP server Lookout can log in to (sendmail, OAuth).
    pub smtp: Option<MailServer>,
}

impl Eds {
    /// Lists Evolution's importable mail accounts, sorted by account name.
    pub async fn mail_accounts(&self) -> Result<Vec<EvolutionMailAccount>> {
        let raw = managed_sources(self).await?;
        Ok(parse_mail_accounts(raw.iter().map(|s| (s.uid.as_str(), s.data.as_str()))))
    }
}

/// Builds the importable accounts from every source's `(UID, Data)`. Skipped,
/// and logged:
///
/// - sources whose key file doesn't parse;
/// - accounts that aren't IMAP (the built-in `local`, `vfolder` and `rss`
///   stores, POP, Exchange and so on);
/// - accounts inside a GNOME Online Accounts collection, which Lookout
///   already reaches through GOA;
/// - accounts that log in with OAuth, since a manual account logs in with a
///   password and Evolution's keyring item holds a token;
/// - accounts with no IMAP host or user.
pub fn parse_mail_accounts<'a>(raw: impl IntoIterator<Item = (&'a str, &'a str)>) -> Vec<EvolutionMailAccount> {
    let files = parse_key_files(raw);
    let mut accounts: Vec<EvolutionMailAccount> = files
        .iter()
        .filter(|(_, file)| file.has_group("Mail Account"))
        .filter_map(|(&uid, file)| mail_account(uid, file, &files))
        .collect();
    accounts.sort_by(|a, b| (a.account_name.to_lowercase(), &a.imap.source_uid).cmp(&(b.account_name.to_lowercase(), &b.imap.source_uid)));
    accounts
}

fn mail_account(uid: &str, file: &KeyFile, files: &HashMap<&str, KeyFile>) -> Option<EvolutionMailAccount> {
    let backend = file.get_non_empty("Mail Account", "BackendName");
    if backend != Some("imapx") {
        tracing::debug!("EDS: not importing mail account {uid}: backend {backend:?} isn't IMAP");
        return None;
    }
    if let Some(goa) = goa_account_id(uid, files) {
        tracing::debug!("EDS: not importing mail account {uid}: it belongs to GOA account {goa}");
        return None;
    }
    if uses_oauth(file) {
        tracing::debug!("EDS: not importing mail account {uid}: it logs in with OAuth");
        return None;
    }
    let Some(imap) = server(uid, file, 993, 143).filter(|s| !s.user.is_empty()) else {
        tracing::warn!("EDS: not importing mail account {uid}: it has no IMAP host or user");
        return None;
    };

    let identity_uid = file.get_non_empty("Mail Account", "IdentityUid");
    let identity = identity_uid.and_then(|id| files.get(id)).filter(|f| f.has_group("Mail Identity"));
    if identity.is_none() {
        tracing::warn!("EDS: mail account {uid} has no identity ({identity_uid:?}); importing it without an address");
    }
    let smtp = identity.and_then(|identity| transport(uid, identity, files));

    Some(EvolutionMailAccount {
        account_name: file.get("Data Source", "DisplayName").unwrap_or_default().to_string(),
        name: identity.and_then(|f| f.get_non_empty("Mail Identity", "Name")).map(str::to_string),
        address: identity.and_then(|f| f.get_non_empty("Mail Identity", "Address")).map(str::to_string),
        imap,
        smtp,
    })
}

/// The SMTP server the identity sends through, if Lookout can use it.
fn transport(account_uid: &str, identity: &KeyFile, files: &HashMap<&str, KeyFile>) -> Option<MailServer> {
    let Some(transport_uid) = identity.get_non_empty("Mail Submission", "TransportUid") else {
        tracing::warn!("EDS: mail account {account_uid} names no transport; importing it without SMTP");
        return None;
    };
    let Some(file) = files.get(transport_uid).filter(|f| f.has_group("Mail Transport")) else {
        tracing::warn!("EDS: mail account {account_uid}'s transport {transport_uid} is missing; importing it without SMTP");
        return None;
    };
    let backend = file.get_non_empty("Mail Transport", "BackendName");
    if backend != Some("smtp") {
        tracing::debug!("EDS: mail account {account_uid}'s transport uses {backend:?}, not SMTP; importing it without SMTP");
        return None;
    }
    if uses_oauth(file) {
        tracing::debug!("EDS: mail account {account_uid}'s transport logs in with OAuth; importing it without SMTP");
        return None;
    }
    let server = server(transport_uid, file, 465, 587);
    if server.is_none() {
        tracing::warn!("EDS: mail account {account_uid}'s transport has no host; importing it without SMTP");
    }
    server
}

/// The `[Authentication]` and `[Security]` groups of an account or transport.
/// `None` without a host. `tls_port` is the default for implicit TLS,
/// `plain_port` for STARTTLS and plain connections.
fn server(source_uid: &str, file: &KeyFile, tls_port: u16, plain_port: u16) -> Option<MailServer> {
    let host = file.get_non_empty("Authentication", "Host")?.trim();
    if host.is_empty() {
        return None;
    }
    let security = MailSecurity::from_method(file.get("Security", "Method"));
    let port = file
        .get("Authentication", "Port")
        .and_then(|p| p.trim().parse::<u16>().ok())
        .filter(|&p| p != 0)
        .unwrap_or(if security == MailSecurity::Tls { tls_port } else { plain_port });
    Some(MailServer {
        source_uid: source_uid.to_string(),
        host: host.to_string(),
        port,
        user: file.get("Authentication", "User").unwrap_or_default().trim().to_string(),
        security,
    })
}

/// Evolution's OAuth mechanisms: the generic `XOAUTH2`/`OAuth2` and the
/// per-provider `Google`, `Outlook` and `Yahoo`.
fn uses_oauth(file: &KeyFile) -> bool {
    file.get("Authentication", "Method")
        .map(str::trim)
        .is_some_and(|m| ["xoauth2", "oauth2", "google", "outlook", "yahoo"].iter().any(|o| m.eq_ignore_ascii_case(o)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `*.source` file in `test-fixtures/eds`, keyed by file stem as
    /// `~/.config/evolution/sources` keys them by UID.
    fn fixtures() -> Vec<(String, String)> {
        let dir = std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../test-fixtures/eds"));
        let mut files: Vec<(String, String)> = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|e| e == "source"))
            .map(|path| (path.file_stem().unwrap().to_string_lossy().into_owned(), std::fs::read_to_string(&path).unwrap()))
            .collect();
        files.sort();
        files
    }

    fn accounts() -> Vec<EvolutionMailAccount> {
        let raw = fixtures();
        parse_mail_accounts(raw.iter().map(|(uid, data)| (uid.as_str(), data.as_str())))
    }

    fn find(accounts: &[EvolutionMailAccount], uid: &str) -> EvolutionMailAccount {
        accounts
            .iter()
            .find(|a| a.imap.source_uid == uid)
            .cloned()
            .unwrap_or_else(|| panic!("{uid} missing from {accounts:#?}"))
    }

    #[test]
    fn imports_only_the_password_imap_accounts_outside_goa() {
        let uids: Vec<String> = accounts().into_iter().map(|a| a.imap.source_uid).collect();
        // Sorted by account name. Left out: the built-in `local` and
        // `vfolder` stores, a POP account, the GOA-backed account, the OAuth
        // account and the malformed file.
        assert_eq!(uids, ["no-transport-account", "plain-account", "starttls-account", "tls-account"]);
    }

    #[test]
    fn a_plain_account_reads_imap_smtp_and_identity() {
        let account = find(&accounts(), "plain-account");
        assert_eq!(account.account_name, "Plain mail");
        assert_eq!(account.name.as_deref(), Some("Pat Plain"));
        assert_eq!(account.address.as_deref(), Some("pat@plain.example"));
        assert_eq!(
            account.imap,
            MailServer {
                source_uid: "plain-account".to_string(),
                host: "mail.plain.example".to_string(),
                port: 143,
                user: "pat".to_string(),
                security: MailSecurity::None,
            }
        );
        assert_eq!(
            account.smtp,
            Some(MailServer {
                source_uid: "plain-transport".to_string(),
                host: "mail.plain.example".to_string(),
                port: 25,
                user: "pat".to_string(),
                security: MailSecurity::None,
            })
        );
    }

    #[test]
    fn starttls_and_implicit_tls_are_told_apart() {
        let accounts = accounts();
        let starttls = find(&accounts, "starttls-account");
        assert_eq!((starttls.imap.security, starttls.imap.port), (MailSecurity::StartTls, 143));
        let smtp = starttls.smtp.unwrap();
        assert_eq!((smtp.security, smtp.port, smtp.user.as_str()), (MailSecurity::StartTls, 587, "sam.submit"));
        assert_eq!(smtp.host, "smtp.starttls.example");

        let tls = find(&accounts, "tls-account");
        assert_eq!((tls.imap.security, tls.imap.port), (MailSecurity::Tls, 993));
        let smtp = tls.smtp.unwrap();
        assert_eq!((smtp.security, smtp.port), (MailSecurity::Tls, 465));
        // The identity has no Name.
        assert_eq!(tls.name, None);
        assert_eq!(tls.address.as_deref(), Some("taylor@tls.example"));
    }

    #[test]
    fn a_missing_transport_leaves_smtp_blank() {
        let account = find(&accounts(), "no-transport-account");
        assert_eq!(account.smtp, None);
        assert_eq!(account.address.as_deref(), Some("noel@example.net"));
        assert_eq!(account.imap.host, "imap.example.net");
    }

    #[test]
    fn goa_oauth_and_malformed_accounts_are_skipped() {
        let accounts = accounts();
        for uid in ["goa-account", "oauth-account", "malformed-account", "pop-account", "local", "vfolder"] {
            assert!(accounts.iter().all(|a| a.imap.source_uid != uid), "{uid} should be skipped");
        }
    }

    #[test]
    fn missing_ports_default_by_security() {
        let source = |security: &str| format!("[Authentication]\nHost=h\nUser=u\n\n[Security]\nMethod={security}\n");
        let port = |security: &str| server("x", &KeyFile::parse(&source(security)).unwrap(), 993, 143).unwrap().port;
        assert_eq!(port("ssl-on-alternate-port"), 993);
        assert_eq!(port("starttls-on-standard-port"), 143);
        assert_eq!(port("none"), 143);
        let zero = KeyFile::parse("[Authentication]\nHost=h\nPort=0\n").unwrap();
        assert_eq!(server("x", &zero, 465, 587).unwrap().port, 587);
        assert!(server("x", &KeyFile::parse("[Authentication]\nHost=\n").unwrap(), 993, 143).is_none());
    }

    #[test]
    fn security_methods_map_like_esource_security() {
        assert_eq!(MailSecurity::from_method(None), MailSecurity::None);
        assert_eq!(MailSecurity::from_method(Some("none")), MailSecurity::None);
        assert_eq!(MailSecurity::from_method(Some("starttls-on-standard-port")), MailSecurity::StartTls);
        assert_eq!(MailSecurity::from_method(Some("ssl-on-alternate-port")), MailSecurity::Tls);
        assert_eq!(MailSecurity::from_method(Some("tls")), MailSecurity::Tls);
    }
}
