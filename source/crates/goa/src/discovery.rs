use std::collections::HashMap;

use lookout_core::AccountId;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};
use zbus::Connection;

use crate::proxies::{AccountProxy, ManagedObjects, OAuth2BasedProxy, ObjectManagerProxy, PasswordBasedProxy};
use crate::{Error, Result};

const IFACE_ACCOUNT: &str = "org.gnome.OnlineAccounts.Account";
const IFACE_MAIL: &str = "org.gnome.OnlineAccounts.Mail";
const IFACE_CALENDAR: &str = "org.gnome.OnlineAccounts.Calendar";
const IFACE_CONTACTS: &str = "org.gnome.OnlineAccounts.Contacts";
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
    /// GOA `ProviderType` for the account (`google`, `nextcloud`,
    /// `ms_graph`, ...), when the account advertises one.
    pub provider_type: Option<String>,
}

impl GoaMailAccount {
    /// Microsoft 365 accounts (GOA provider types `ms_graph`, the legacy
    /// `microsoft365`, and the older personal `microsoft`) are treated
    /// specially by the app: GOA's token for them carries only Microsoft
    /// Graph scopes and can't authenticate to Exchange Online IMAP/SMTP, so
    /// the app supplies its own OAuth2 credentials instead of GOA's.
    pub fn is_microsoft_365(&self) -> bool {
        matches!(self.provider_type.as_deref(), Some("ms_graph") | Some("microsoft365") | Some("microsoft"))
    }
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
    /// `Account.Identity` - the account's login username (the user id on a
    /// Nextcloud server, the email address on Google/Microsoft). This, not
    /// the display name, is what the DAV server expects over HTTP Basic Auth.
    pub identity: String,
    /// `Calendar.Uri` - the CalDAV base URL GOA has configured for this
    /// account (may be a principal URL, a calendar-home-set URL, or a bare
    /// server root, depending on provider - discovery must not assume which).
    /// Sanitized by [`normalize_dav_base_url`]: userinfo stripped, and a
    /// Nextcloud `remote.php/dav`-style path given its trailing slash.
    pub uri: String,
    pub accept_ssl_errors: bool,
    pub auth: CalendarAuthMethod,
    /// GOA `ProviderType` for the account (`google`, `nextcloud`/`owncloud`,
    /// ...), when the account advertises one.
    pub provider_type: Option<String>,
}

/// How to obtain live credentials for a [`GoaContactsAccount`]. Kept
/// separate from Mail and Calendar auth types for clarity at call sites.
#[derive(Debug, Clone)]
pub enum ContactsAuthMethod {
    OAuth2,
    /// `password_id` is the `PasswordBased.GetPassword` slot id to use
    /// (commonly `"contacts-password"`).
    Password {
        password_id: String,
    },
}

#[derive(Debug, Clone)]
pub struct GoaContactsAccount {
    pub account_id: AccountId,
    pub object_path: OwnedObjectPath,
    pub display_name: String,
    /// `Account.Identity` - the account's login username (see
    /// [`GoaCalendarAccount::identity`]).
    pub identity: String,
    /// `Contacts.Uri` - the CardDAV base URL GOA has configured for this
    /// account. Sanitized by [`normalize_dav_base_url`].
    pub uri: String,
    pub accept_ssl_errors: bool,
    pub auth: ContactsAuthMethod,
    /// GOA `ProviderType` for the account, when the account advertises one.
    pub provider_type: Option<String>,
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
        let microsoft_365 = is_microsoft365_provider(account_props);
        let imap_supported = get_bool(mail_props, "ImapSupported").unwrap_or(true);
        let smtp_supported = get_bool(mail_props, "SmtpSupported").unwrap_or(true);
        // GOA flags Microsoft 365 accounts as IMAP/SMTP-incapable (its
        // `ms_graph`/`microsoft365` providers model mail as EWS/Graph), but
        // the Exchange Online endpoints accept OAuth2 IMAP/SMTP - so those
        // accounts bypass this gate and get hardcoded endpoints below.
        if !microsoft_365 && (!imap_supported || !smtp_supported) {
            return Ok(None);
        }

        let display_name = get_string(mail_props, "Name")
            .or_else(|| get_string(account_props, "PresentationIdentity"))
            .unwrap_or_default();
        let email = get_string(mail_props, "EmailAddress").unwrap_or_default();

        let imap = if microsoft_365 {
            EndpointConfig {
                host: "outlook.office365.com".to_string(),
                port: Some(993),
                use_tls: true,
                accept_ssl_errors: false,
                username: email.clone(),
            }
        } else {
            let (imap_host, imap_port) = split_host_port(&get_string(mail_props, "ImapHost").unwrap_or_default());
            EndpointConfig {
                host: imap_host,
                port: imap_port,
                use_tls: get_bool(mail_props, "ImapUseSsl").unwrap_or(false) || get_bool(mail_props, "ImapUseTls").unwrap_or(false),
                accept_ssl_errors: get_bool(mail_props, "ImapAcceptSslErrors").unwrap_or(false),
                username: get_string(mail_props, "ImapUserName").unwrap_or_default(),
            }
        };
        let smtp = if microsoft_365 {
            EndpointConfig {
                host: "smtp.office365.com".to_string(),
                port: Some(587),
                use_tls: true,
                accept_ssl_errors: false,
                username: email.clone(),
            }
        } else {
            let (smtp_host, smtp_port) = split_host_port(&get_string(mail_props, "SmtpHost").unwrap_or_default());
            EndpointConfig {
                host: smtp_host,
                port: smtp_port,
                use_tls: get_bool(mail_props, "SmtpUseSsl").unwrap_or(false) || get_bool(mail_props, "SmtpUseTls").unwrap_or(false),
                accept_ssl_errors: get_bool(mail_props, "SmtpAcceptSslErrors").unwrap_or(false),
                username: get_string(mail_props, "SmtpUserName").unwrap_or_default(),
            }
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
            provider_type: get_string(account_props, "ProviderType"),
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

    /// Lists every GOA account with a usable `Contacts` interface.
    pub async fn list_contacts_accounts(&self) -> Result<Vec<GoaContactsAccount>> {
        let manager = ObjectManagerProxy::new(&self.connection).await?;
        let objects: ManagedObjects = manager.get_managed_objects().await?;

        let mut accounts = Vec::new();
        for (path, ifaces) in &objects {
            if !ifaces.contains_key(IFACE_ACCOUNT) {
                continue;
            }
            // `parse_contacts_account` silently returns `None` for any of
            // several reasons (no Contacts interface at all, an interface
            // with an empty Uri, no OAuth2/Password interface) - log what
            // GOA actually reported for every account so a "why didn't my
            // account show up" question doesn't require re-deriving this by
            // hand from a D-Bus introspection dump.
            let contacts_props = ifaces.get(IFACE_CONTACTS);
            tracing::debug!(
                "GOA account {path}: Contacts interface = {}, Uri = {:?}, OAuth2 = {}, Password = {}",
                contacts_props.is_some(),
                contacts_props.and_then(|props| get_string(props, "Uri")),
                ifaces.contains_key(IFACE_OAUTH2),
                ifaces.contains_key(IFACE_PASSWORD),
            );
            if let Some(account) = self.parse_contacts_account(path, ifaces)? {
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
        let identity = get_string(account_props, "Identity").unwrap_or_default();
        let uri = normalize_dav_base_url(get_string(cal_props, "Uri").unwrap_or_default(), get_string(account_props, "ProviderType").as_deref());
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
            identity,
            uri,
            accept_ssl_errors,
            auth,
            provider_type: get_string(account_props, "ProviderType"),
        }))
    }

    fn parse_contacts_account(&self, path: &OwnedObjectPath, ifaces: &HashMap<String, HashMap<String, OwnedValue>>) -> Result<Option<GoaContactsAccount>> {
        let Some(account_props) = ifaces.get(IFACE_ACCOUNT) else {
            return Ok(None);
        };
        let Some(contacts_props) = ifaces.get(IFACE_CONTACTS) else {
            return Ok(None);
        };

        let display_name = get_string(account_props, "PresentationIdentity").unwrap_or_default();
        let identity = get_string(account_props, "Identity").unwrap_or_default();
        let uri = normalize_dav_base_url(get_string(contacts_props, "Uri").unwrap_or_default(), get_string(account_props, "ProviderType").as_deref());
        if uri.is_empty() {
            return Ok(None);
        }
        let accept_ssl_errors = get_bool(contacts_props, "AcceptSslErrors").unwrap_or(false);

        let auth = if ifaces.contains_key(IFACE_OAUTH2) {
            ContactsAuthMethod::OAuth2
        } else if ifaces.contains_key(IFACE_PASSWORD) {
            ContactsAuthMethod::Password {
                password_id: "contacts-password".to_string(),
            }
        } else {
            return Ok(None);
        };

        Ok(Some(GoaContactsAccount {
            account_id: AccountId(path.to_string()),
            object_path: path.clone(),
            display_name,
            identity,
            uri,
            accept_ssl_errors,
            auth,
            provider_type: get_string(account_props, "ProviderType"),
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

    pub async fn ensure_credentials_contacts(&self, account: &GoaContactsAccount) -> Result<()> {
        self.ensure_credentials_for(&account.object_path).await
    }

    pub async fn get_access_token_contacts(&self, account: &GoaContactsAccount) -> Result<(String, i32)> {
        self.get_access_token_for(&account.object_path).await
    }

    pub async fn get_contacts_password(&self, account: &GoaContactsAccount) -> Result<String> {
        let ContactsAuthMethod::Password { password_id } = &account.auth else {
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

/// Microsoft 365 accounts (GOA provider types `ms_graph`, the legacy
/// `microsoft365`, and the older personal `microsoft`) advertise a Mail
/// interface whose host fields are empty and whose `ImapSupported`/
/// `SmtpSupported` flags are false, because GOA models their mail as
/// EWS/Graph. The Exchange Online endpoints do accept OAuth2 IMAP/SMTP, so
/// [`parse_mail_account`] supplies known-good settings for them instead of
/// reading them from the account.
fn is_microsoft365_provider(account_props: &HashMap<String, OwnedValue>) -> bool {
    matches!(
        get_string(account_props, "ProviderType").as_deref(),
        Some("ms_graph") | Some("microsoft365") | Some("microsoft")
    )
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

/// Cleans up a GOA-stored CalDAV/CardDAV base URL for use by the client.
///
/// Two fixes, both aimed at the way GOA's owncloud/Nextcloud provider records
/// its URIs (observed live as `https://user@server/remote.php/dav`):
///
/// * **Userinfo is stripped.** The username belongs in the `Authorization`
///   header (Basic auth carries it as a separate, always-present field), not
///   in the URL - a userinfo-bearing URL is at best redundant and leaks the
///   login into every error message and log line, and strict front-ends may
///   reject the request outright.
/// * **A missing trailing slash is restored on Nextcloud-style endpoints.**
///   Sabre/dav (which Nextcloud and ownCloud both use) is documented to
///   answer the slashless `/remote.php/dav` path with HTTP 400 Bad Request on
///   many setups; the canonical form is `/remote.php/dav/`.
///
/// Deliberately *not* applied to other providers: Google's CardDAV
/// well-known endpoint is `/.well-known/carddav` (RFC 6764 forbids the
/// trailing slash there) and its CalDAV endpoint `/caldav/v2/<user>/user`
/// serves fine without one, so the slash fix must not be blanket.
///
/// Anything that isn't an `http`/`https` URL is returned unchanged - the
/// empty string stays empty (callers treat that as "no account"), and a
/// genuinely malformed URL is left for the DAV client to report. This is
/// string surgery rather than a real URL parse to keep this D-Bus-only crate
/// free of URL-parsing dependencies; the inputs are GOA-produced http(s)
/// URIs, whose shape is well understood (no IPv6-with-userinfo corner cases
/// in practice, but an `@` in the *path* is never mistaken for userinfo
/// because the authority is split at the first `/`).
fn normalize_dav_base_url(uri: String, provider_type: Option<&str>) -> String {
    let Some(after_scheme) = uri.find("://") else {
        return uri;
    };
    let scheme = &uri[..after_scheme];
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return uri;
    }
    // The authority ends at the first `/`, `?`, or `#` after the scheme.
    let rest = &uri[after_scheme + 3..];
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = (&rest[..authority_end], &rest[authority_end..]);
    // Userinfo is the authority's content up to (and including) its last `@`.
    let authority = match authority.rfind('@') {
        Some(at) => &authority[at + 1..],
        None => authority,
    };
    let is_nextcloud = matches!(provider_type, Some("owncloud") | Some("nextcloud"));
    if !is_nextcloud {
        return format!("{scheme}://{authority}{tail}");
    }
    // Split the tail into path + query/fragment so the slash lands on the path.
    let path_end = tail.find(['?', '#']).unwrap_or(tail.len());
    let (path, query) = (&tail[..path_end], &tail[path_end..]);
    if path.ends_with('/') {
        format!("{scheme}://{authority}{tail}")
    } else {
        format!("{scheme}://{authority}{path}/{query}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::Value;

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

    #[test]
    fn recognizes_microsoft_365_provider_types() {
        fn props_with_provider_type(t: &str) -> HashMap<String, OwnedValue> {
            let mut props = HashMap::new();
            props.insert("ProviderType".to_string(), OwnedValue::try_from(Value::from(t)).unwrap());
            props
        }
        for t in ["ms_graph", "microsoft365", "microsoft"] {
            assert!(is_microsoft365_provider(&props_with_provider_type(t)), "expected {t:?} to be treated as Microsoft 365");
        }
        for t in ["google", "nextcloud"] {
            assert!(!is_microsoft365_provider(&props_with_provider_type(t)), "expected {t:?} not to be treated as Microsoft 365");
        }
        assert!(!is_microsoft365_provider(&HashMap::new()), "accounts without a ProviderType must not be special-cased");
    }

    #[test]
    fn strips_userinfo_and_restores_slash_for_nextcloud() {
        let uri = normalize_dav_base_url("https://ggraham@cloud.wahwahhut.com/remote.php/dav".to_string(), Some("owncloud"));
        assert_eq!(uri, "https://cloud.wahwahhut.com/remote.php/dav/");
    }

    #[test]
    fn nextcloud_uri_already_clean_is_untouched() {
        let uri = normalize_dav_base_url("https://cloud.wahwahhut.com/remote.php/dav/".to_string(), Some("owncloud"));
        assert_eq!(uri, "https://cloud.wahwahhut.com/remote.php/dav/");
    }

    #[test]
    fn nextcloud_bare_server_root_gains_root_slash() {
        let uri = normalize_dav_base_url("https://cloud.wahwahhut.com".to_string(), Some("owncloud"));
        assert_eq!(uri, "https://cloud.wahwahhut.com/");
    }

    #[test]
    fn google_well_known_carddav_keeps_its_slashless_path() {
        // RFC 6764 well-known paths must not gain a trailing slash.
        let uri = normalize_dav_base_url("https://www.googleapis.com/.well-known/carddav".to_string(), Some("google"));
        assert_eq!(uri, "https://www.googleapis.com/.well-known/carddav");
    }

    #[test]
    fn google_caldav_user_endpoint_keeps_its_slashless_path() {
        let uri = normalize_dav_base_url("https://apidata.googleusercontent.com/caldav/v2/gavindi@gmail.com/user".to_string(), Some("google"));
        assert_eq!(uri, "https://apidata.googleusercontent.com/caldav/v2/gavindi@gmail.com/user");
    }

    #[test]
    fn unknown_provider_still_strips_userinfo_but_keeps_path() {
        let uri = normalize_dav_base_url("https://alice@caldav.example.com/dav/alice".to_string(), None);
        assert_eq!(uri, "https://caldav.example.com/dav/alice");
    }

    #[test]
    fn empty_or_unparseable_uri_passes_through() {
        assert_eq!(normalize_dav_base_url(String::new(), Some("owncloud")), "");
        assert_eq!(normalize_dav_base_url("not a url".to_string(), Some("owncloud")), "not a url");
        assert_eq!(normalize_dav_base_url("webcal://feeds.example.com/cal.ics".to_string(), Some("owncloud")), "webcal://feeds.example.com/cal.ics");
    }
}
