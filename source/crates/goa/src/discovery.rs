use std::collections::HashMap;

use lookout_core::AccountId;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};
use zbus::Connection;

use crate::proxies::{AccountProxy, ManagedObjects, OAuth2BasedProxy, ObjectManagerProxy, PasswordBasedProxy};
use crate::{Error, Result};

const IFACE_ACCOUNT: &str = "org.gnome.OnlineAccounts.Account";
const IFACE_MAIL: &str = "org.gnome.OnlineAccounts.Mail";
const IFACE_CALENDAR: &str = "org.gnome.OnlineAccounts.Calendar";
const IFACE_OAUTH2: &str = "org.gnome.OnlineAccounts.OAuth2Based";
const IFACE_PASSWORD: &str = "org.gnome.OnlineAccounts.PasswordBased";

/// How to obtain live credentials for a [`GoaMailAccount`]. Never holds an
/// actual secret — only enough to ask GOA for one on demand.
#[derive(Debug, Clone)]
pub enum AuthMethod {
    OAuth2,
    /// `imap_password_id`/`smtp_password_id` are the `PasswordBased.GetPassword`
    /// slot ids to use for each protocol (commonly `"imap-password"` /
    /// `"smtp-password"`, falling back to a generic `"password"` id for
    /// providers that don't split them).
    Password {
        imap_password_id: String,
        smtp_password_id: String,
    },
}

#[derive(Debug, Clone)]
pub struct EndpointConfig {
    pub host: String,
    /// `None` means "use the protocol default port" — GOA embeds a port
    /// override in the host string itself (`host:port`) only when it differs
    /// from the default.
    pub port: Option<u16>,
    pub use_tls: bool,
    pub accept_ssl_errors: bool,
    pub username: String,
}

#[derive(Debug, Clone)]
pub struct GoaMailAccount {
    pub account_id: AccountId,
    pub object_path: OwnedObjectPath,
    pub display_name: String,
    pub email: String,
    pub imap: EndpointConfig,
    pub smtp: EndpointConfig,
    pub auth: AuthMethod,
}

/// How to obtain live credentials for a [`GoaCalendarAccount`]. A separate
/// type from [`AuthMethod`] rather than a reuse - Mail's variant is
/// IMAP/SMTP-shaped (two password ids), which doesn't fit Calendar's single
/// CalDAV endpoint.
#[derive(Debug, Clone)]
pub enum CalendarAuthMethod {
    OAuth2,
    /// `password_id` is the `PasswordBased.GetPassword` slot id to use
    /// (commonly `"calendar-password"`, per GOA's one-slot-per-service-
    /// interface convention - distinct from `imap-password`/`smtp-password`
    /// even on an account that also has Mail enabled).
    Password {
        password_id: String,
    },
}

#[derive(Debug, Clone)]
pub struct GoaCalendarAccount {
    pub account_id: AccountId,
    pub object_path: OwnedObjectPath,
    pub display_name: String,
    /// `Calendar.Uri` - the CalDAV base URL GOA has configured for this
    /// account (may be a principal URL, a calendar-home-set URL, or a bare
    /// server root, depending on provider - discovery must not assume which).
    pub uri: String,
    pub accept_ssl_errors: bool,
    pub auth: CalendarAuthMethod,
}

/// Thin wrapper over a session-bus [`Connection`] providing GOA account
/// discovery and on-demand credential fetches. Holds no secrets itself.
/// `Clone` is cheap - `zbus::Connection` is itself an `Arc`-backed handle -
/// so one `GoaClient` can be shared across multiple accounts' credential
/// providers instead of opening a redundant D-Bus connection per account.
#[derive(Clone)]
pub struct GoaClient {
    connection: Connection,
}

impl GoaClient {
    pub async fn connect() -> Result<Self> {
        let connection = Connection::session().await?;
        Ok(GoaClient { connection })
    }

    /// Lists every GOA account that has Mail enabled (`Account.MailDisabled`
    /// is false or absent, and a `Mail` interface is present).
    pub async fn list_mail_accounts(&self) -> Result<Vec<GoaMailAccount>> {
        let manager = ObjectManagerProxy::new(&self.connection).await?;
        let objects: ManagedObjects = manager.get_managed_objects().await?;

        let mut accounts = Vec::new();
        for (path, ifaces) in &objects {
            if let Some(account) = self.parse_mail_account(path, ifaces)? {
                accounts.push(account);
            }
        }
        Ok(accounts)
    }

    fn parse_mail_account(&self, path: &OwnedObjectPath, ifaces: &HashMap<String, HashMap<String, OwnedValue>>) -> Result<Option<GoaMailAccount>> {
        let Some(account_props) = ifaces.get(IFACE_ACCOUNT) else {
            return Ok(None);
        };
        let Some(mail_props) = ifaces.get(IFACE_MAIL) else {
            return Ok(None);
        };

        let mail_disabled = get_bool(account_props, "MailDisabled").unwrap_or(false);
        if mail_disabled {
            return Ok(None);
        }
        let imap_supported = get_bool(mail_props, "ImapSupported").unwrap_or(true);
        let smtp_supported = get_bool(mail_props, "SmtpSupported").unwrap_or(true);
        if !imap_supported || !smtp_supported {
            return Ok(None);
        }

        let display_name = get_string(mail_props, "Name")
            .or_else(|| get_string(account_props, "PresentationIdentity"))
            .unwrap_or_default();
        let email = get_string(mail_props, "EmailAddress").unwrap_or_default();

        let (imap_host, imap_port) = split_host_port(&get_string(mail_props, "ImapHost").unwrap_or_default());
        let (smtp_host, smtp_port) = split_host_port(&get_string(mail_props, "SmtpHost").unwrap_or_default());

        let imap = EndpointConfig {
            host: imap_host,
            port: imap_port,
            use_tls: get_bool(mail_props, "ImapUseSsl").unwrap_or(false) || get_bool(mail_props, "ImapUseTls").unwrap_or(false),
            accept_ssl_errors: get_bool(mail_props, "ImapAcceptSslErrors").unwrap_or(false),
            username: get_string(mail_props, "ImapUserName").unwrap_or_default(),
        };
        let smtp = EndpointConfig {
            host: smtp_host,
            port: smtp_port,
            use_tls: get_bool(mail_props, "SmtpUseSsl").unwrap_or(false) || get_bool(mail_props, "SmtpUseTls").unwrap_or(false),
            accept_ssl_errors: get_bool(mail_props, "SmtpAcceptSslErrors").unwrap_or(false),
            username: get_string(mail_props, "SmtpUserName").unwrap_or_default(),
        };

        let auth = if ifaces.contains_key(IFACE_OAUTH2) {
            AuthMethod::OAuth2
        } else if ifaces.contains_key(IFACE_PASSWORD) {
            AuthMethod::Password {
                imap_password_id: "imap-password".to_string(),
                smtp_password_id: "smtp-password".to_string(),
            }
        } else {
            // No known credential interface - can't authenticate this account.
            return Ok(None);
        };

        Ok(Some(GoaMailAccount {
            account_id: AccountId(path.to_string()),
            object_path: path.clone(),
            display_name,
            email,
            imap,
            smtp,
            auth,
        }))
    }

    /// Must be called (and succeed) before fetching credentials for an
    /// account whose stored credentials might be stale or missing. On
    /// failure, the caller should surface a banner directing the user to
    /// `gnome-control-center online-accounts` rather than attempting our own
    /// reauth flow.
    pub async fn ensure_credentials(&self, account: &GoaMailAccount) -> Result<()> {
        self.ensure_credentials_for(&account.object_path).await
    }

    pub async fn get_access_token(&self, account: &GoaMailAccount) -> Result<(String, i32)> {
        self.get_access_token_for(&account.object_path).await
    }

    pub async fn get_imap_password(&self, account: &GoaMailAccount) -> Result<String> {
        let AuthMethod::Password { imap_password_id, .. } = &account.auth else {
            return Err(Error::WrongAuthMethod);
        };
        self.get_password_for(&account.object_path, imap_password_id).await
    }

    pub async fn get_smtp_password(&self, account: &GoaMailAccount) -> Result<String> {
        let AuthMethod::Password { smtp_password_id, .. } = &account.auth else {
            return Err(Error::WrongAuthMethod);
        };
        self.get_password_for(&account.object_path, smtp_password_id).await
    }

    /// Lists every GOA account with a usable `Calendar` interface - a fully
    /// separate set from [`list_mail_accounts`](Self::list_mail_accounts)'s
    /// results (a Calendar-only account has no `Mail` interface at all, and a
    /// Mail account may equally have no `Calendar` interface).
    pub async fn list_calendar_accounts(&self) -> Result<Vec<GoaCalendarAccount>> {
        let manager = ObjectManagerProxy::new(&self.connection).await?;
        let objects: ManagedObjects = manager.get_managed_objects().await?;

        let mut accounts = Vec::new();
        for (path, ifaces) in &objects {
            if let Some(account) = self.parse_calendar_account(path, ifaces)? {
                accounts.push(account);
            }
        }
        Ok(accounts)
    }

    fn parse_calendar_account(&self, path: &OwnedObjectPath, ifaces: &HashMap<String, HashMap<String, OwnedValue>>) -> Result<Option<GoaCalendarAccount>> {
        let Some(account_props) = ifaces.get(IFACE_ACCOUNT) else {
            return Ok(None);
        };
        let Some(cal_props) = ifaces.get(IFACE_CALENDAR) else {
            return Ok(None);
        };

        // NB: unlike Mail's `MailDisabled`, there's no confirmed
        // `CalendarDisabled` boolean on the real GOA `Account` interface -
        // presence of the `Calendar` interface itself is treated as
        // authoritative for now. Revisit if a live account turns up one.
        let display_name = get_string(account_props, "PresentationIdentity").unwrap_or_default();
        let uri = get_string(cal_props, "Uri").unwrap_or_default();
        if uri.is_empty() {
            return Ok(None);
        }
        let accept_ssl_errors = get_bool(cal_props, "AcceptSslErrors").unwrap_or(false);

        let auth = if ifaces.contains_key(IFACE_OAUTH2) {
            CalendarAuthMethod::OAuth2
        } else if ifaces.contains_key(IFACE_PASSWORD) {
            CalendarAuthMethod::Password {
                password_id: "calendar-password".to_string(),
            }
        } else {
            return Ok(None);
        };

        Ok(Some(GoaCalendarAccount {
            account_id: AccountId(path.to_string()),
            object_path: path.clone(),
            display_name,
            uri,
            accept_ssl_errors,
            auth,
        }))
    }

    pub async fn ensure_credentials_calendar(&self, account: &GoaCalendarAccount) -> Result<()> {
        self.ensure_credentials_for(&account.object_path).await
    }

    pub async fn get_access_token_calendar(&self, account: &GoaCalendarAccount) -> Result<(String, i32)> {
        self.get_access_token_for(&account.object_path).await
    }

    pub async fn get_calendar_password(&self, account: &GoaCalendarAccount) -> Result<String> {
        let CalendarAuthMethod::Password { password_id } = &account.auth else {
            return Err(Error::WrongAuthMethod);
        };
        self.get_password_for(&account.object_path, password_id).await
    }

    async fn ensure_credentials_for(&self, object_path: &OwnedObjectPath) -> Result<()> {
        let proxy = AccountProxy::builder(&self.connection).path(object_path.as_ref())?.build().await?;
        proxy.ensure_credentials().await?;
        Ok(())
    }

    async fn get_access_token_for(&self, object_path: &OwnedObjectPath) -> Result<(String, i32)> {
        let proxy = OAuth2BasedProxy::builder(&self.connection).path(object_path.as_ref())?.build().await?;
        Ok(proxy.get_access_token().await?)
    }

    async fn get_password_for(&self, object_path: &OwnedObjectPath, id: &str) -> Result<String> {
        let proxy = PasswordBasedProxy::builder(&self.connection).path(object_path.as_ref())?.build().await?;
        Ok(proxy.get_password(id).await?)
    }
}

fn get_string(props: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    props.get(key).and_then(|v| String::try_from(v.clone()).ok())
}

fn get_bool(props: &HashMap<String, OwnedValue>, key: &str) -> Option<bool> {
    props.get(key).and_then(|v| bool::try_from(v.clone()).ok())
}

/// GOA embeds a port override in the host string itself (`"host:port"`) only
/// when it differs from the protocol default; splits it back out. Only a
/// *single* colon is treated as a host:port separator, since a bare IPv6
/// literal (e.g. `::1`) contains multiple colons and no port.
fn split_host_port(host: &str) -> (String, Option<u16>) {
    if host.matches(':').count() == 1 {
        if let Some((h, p)) = host.split_once(':') {
            if let Ok(port) = p.parse::<u16>() {
                return (h.to_string(), Some(port));
            }
        }
    }
    (host.to_string(), None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_explicit_port_override() {
        assert_eq!(split_host_port("imap.example.com:993"), ("imap.example.com".to_string(), Some(993)));
    }

    #[test]
    fn leaves_bare_host_without_port() {
        assert_eq!(split_host_port("imap.example.com"), ("imap.example.com".to_string(), None));
    }

    #[test]
    fn ipv6_without_port_is_not_misparsed() {
        // A bare IPv6 literal contains colons but no port; the trailing
        // segment after the last colon won't parse as u16, so we should fall
        // back to treating the whole string as the host.
        let (host, port) = split_host_port("::1");
        assert_eq!(port, None);
        assert_eq!(host, "::1");
    }
}
