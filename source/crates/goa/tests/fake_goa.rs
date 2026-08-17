//! Exercises `lookout_goa::GoaClient` against a hand-rolled fake GOA D-Bus
//! service instead of a real GOA daemon, giving real D-Bus-wire coverage of
//! the discovery/credential-fetch logic without needing a GNOME session.
//! Must run under an isolated session bus (`dbus-run-session`) since it
//! claims the well-known name `org.gnome.OnlineAccounts` - claiming that
//! name on a real desktop session bus would fail (or worse, shadow the real
//! GOA daemon), so this test deliberately fails fast with a clear message
//! rather than silently doing something surprising if run outside one.
//!
//! Run with:
//!   dbus-run-session -- cargo test -p lookout-goa --test fake_goa

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::collections::HashMap;

use lookout_goa::{AuthMethod, CalendarAuthMethod, ContactsAuthMethod, GoaClient};
use zbus::interface;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

struct FakeObjectManager {
    objects: HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>>,
}

#[interface(name = "org.freedesktop.DBus.ObjectManager")]
impl FakeObjectManager {
    fn get_managed_objects(&self) -> HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>> {
        self.objects.clone()
    }
}

struct FakeAccount;

#[interface(name = "org.gnome.OnlineAccounts.Account")]
impl FakeAccount {
    fn ensure_credentials(&self) -> i32 {
        0
    }
}

struct FakePasswordBased {
    password: String,
}

#[interface(name = "org.gnome.OnlineAccounts.PasswordBased")]
impl FakePasswordBased {
    fn get_password(&self, id: &str) -> String {
        format!("{}-{}", self.password, id)
    }
}

struct FakeOAuth2Based {
    token: String,
}

#[interface(name = "org.gnome.OnlineAccounts.OAuth2Based")]
impl FakeOAuth2Based {
    fn get_access_token(&self) -> (String, i32) {
        (self.token.clone(), 3600)
    }
}

fn prop(value: impl Into<Value<'static>>) -> OwnedValue {
    OwnedValue::try_from(value.into()).unwrap()
}

fn mail_account_props(email: &str, imap_supported: bool) -> HashMap<String, HashMap<String, OwnedValue>> {
    let mut ifaces = HashMap::new();

    let mut account = HashMap::new();
    account.insert("MailDisabled".to_string(), prop(false));
    ifaces.insert("org.gnome.OnlineAccounts.Account".to_string(), account);

    let mut mail = HashMap::new();
    mail.insert("EmailAddress".to_string(), prop(email.to_string()));
    mail.insert("Name".to_string(), prop(email.to_string()));
    mail.insert("ImapSupported".to_string(), prop(imap_supported));
    mail.insert("ImapHost".to_string(), prop("imap.example.com:993".to_string()));
    mail.insert("ImapUseSsl".to_string(), prop(true));
    mail.insert("ImapUseTls".to_string(), prop(false));
    mail.insert("ImapUserName".to_string(), prop(email.to_string()));
    mail.insert("SmtpSupported".to_string(), prop(true));
    mail.insert("SmtpHost".to_string(), prop("smtp.example.com:587".to_string()));
    mail.insert("SmtpUseSsl".to_string(), prop(true));
    mail.insert("SmtpUseTls".to_string(), prop(false));
    mail.insert("SmtpUserName".to_string(), prop(email.to_string()));
    ifaces.insert("org.gnome.OnlineAccounts.Mail".to_string(), mail);

    ifaces
}

/// Adds a `Calendar` interface's properties onto an already-built `ifaces`
/// map, in place - so a fixture can carry Mail, Calendar, both, or neither.
fn add_calendar_iface(ifaces: &mut HashMap<String, HashMap<String, OwnedValue>>, uri: &str, accept_ssl_errors: bool) {
    let mut calendar = HashMap::new();
    calendar.insert("Uri".to_string(), prop(uri.to_string()));
    calendar.insert("AcceptSslErrors".to_string(), prop(accept_ssl_errors));
    ifaces.insert("org.gnome.OnlineAccounts.Calendar".to_string(), calendar);
}

/// Adds a `Contacts` interface's properties onto an already-built `ifaces`
/// map, in place.
fn add_contacts_iface(ifaces: &mut HashMap<String, HashMap<String, OwnedValue>>, uri: &str, accept_ssl_errors: bool) {
    let mut contacts = HashMap::new();
    contacts.insert("Uri".to_string(), prop(uri.to_string()));
    contacts.insert("AcceptSslErrors".to_string(), prop(accept_ssl_errors));
    ifaces.insert("org.gnome.OnlineAccounts.Contacts".to_string(), contacts);
}

/// Microsoft 365-style account: GOA's `ms_graph`/`microsoft365`/`microsoft`
/// providers leave the Mail interface's host/username fields empty and flag
/// `ImapSupported`/`SmtpSupported` false (mirrors the real Microsoft 365
/// account observed against live GOA during development). Such accounts must
/// be listed by `list_mail_accounts()` with hardcoded Exchange Online
/// endpoints, unlike the genuinely-unsupported `unsupported_account` below.
fn microsoft365_account_props(email: &str) -> HashMap<String, HashMap<String, OwnedValue>> {
    let mut ifaces = HashMap::new();

    let mut account = HashMap::new();
    account.insert("MailDisabled".to_string(), prop(false));
    account.insert("ProviderType".to_string(), prop("ms_graph".to_string()));
    ifaces.insert("org.gnome.OnlineAccounts.Account".to_string(), account);

    let mut mail = HashMap::new();
    mail.insert("EmailAddress".to_string(), prop(email.to_string()));
    mail.insert("Name".to_string(), prop(email.to_string()));
    mail.insert("ImapSupported".to_string(), prop(false));
    mail.insert("SmtpSupported".to_string(), prop(false));
    mail.insert("ImapHost".to_string(), prop("".to_string()));
    mail.insert("SmtpHost".to_string(), prop("".to_string()));
    ifaces.insert("org.gnome.OnlineAccounts.Mail".to_string(), mail);

    ifaces.insert("org.gnome.OnlineAccounts.OAuth2Based".to_string(), HashMap::new());
    ifaces
}

#[tokio::test]
async fn discovers_and_fetches_credentials_over_real_dbus_wire() {
    let mut objects = HashMap::new();

    let mut oauth_ifaces = mail_account_props("oauth@example.com", true);
    oauth_ifaces.insert("org.gnome.OnlineAccounts.OAuth2Based".to_string(), HashMap::new());
    add_calendar_iface(&mut oauth_ifaces, "https://caldav.example.com/dav/oauth@example.com/", false);
    add_contacts_iface(&mut oauth_ifaces, "https://carddav.example.com/dav/oauth@example.com/", false);
    objects.insert(OwnedObjectPath::try_from("/org/gnome/OnlineAccounts/Accounts/oauth_account").unwrap(), oauth_ifaces);

    let mut pw_ifaces = mail_account_props("password@example.com", true);
    pw_ifaces.insert("org.gnome.OnlineAccounts.PasswordBased".to_string(), HashMap::new());
    add_calendar_iface(&mut pw_ifaces, "https://caldav.example.com/dav/password@example.com/", true);
    add_contacts_iface(&mut pw_ifaces, "https://carddav.example.com/dav/password@example.com/", true);
    objects.insert(OwnedObjectPath::try_from("/org/gnome/OnlineAccounts/Accounts/pw_account").unwrap(), pw_ifaces);

    // A genuine Microsoft 365 account: Mail interface present but hosts
    // empty and ImapSupported/SmtpSupported false (mirrors the live account),
    // plus a Calendar interface with an empty Uri as GOA reports for these
    // accounts. list_mail_accounts() must list it with hardcoded Exchange
    // Online endpoints; the empty-Uri calendar must not be listed.
    let mut ms_ifaces = microsoft365_account_props("work@contoso.com");
    add_calendar_iface(&mut ms_ifaces, "", false);
    objects.insert(OwnedObjectPath::try_from("/org/gnome/OnlineAccounts/Accounts/ms365_account").unwrap(), ms_ifaces);

    // A non-Microsoft account whose Mail interface is unusable (no
    // ProviderType, so not special-cased) - must be filtered out by
    // list_mail_accounts().
    let mut unsupported_ifaces = mail_account_props("unsupported@example.com", false);
    unsupported_ifaces.insert("org.gnome.OnlineAccounts.OAuth2Based".to_string(), HashMap::new());
    objects.insert(
        OwnedObjectPath::try_from("/org/gnome/OnlineAccounts/Accounts/unsupported_account").unwrap(),
        unsupported_ifaces,
    );

    // An account with no Mail interface at all (mirrors the Nextcloud
    // account observed live: only Contacts/Calendar/Files/PasswordBased).
    // The live GOA URI for it carries the login in the URL's userinfo
    // (`https://ggraham@cloud.wahwahhut.com/remote.php/dav`) with no
    // trailing slash on the DAV path - the exact shape `normalize_dav_base_url`
    // exists to fix - and its `Identity` (`ggraham`) differs from its display
    // name (`ggraham@cloud.wahwahhut.com`), so a parsed account must carry
    // both and the URI must come out cleaned.
    let mut no_mail_ifaces = HashMap::new();
    no_mail_ifaces.insert("org.gnome.OnlineAccounts.Account".to_string(), {
        let mut m = HashMap::new();
        m.insert("MailDisabled".to_string(), prop(false));
        m.insert("Identity".to_string(), prop("ggraham".to_string()));
        m.insert("PresentationIdentity".to_string(), prop("ggraham@cloud.wahwahhut.com".to_string()));
        m.insert("ProviderType".to_string(), prop("owncloud".to_string()));
        m
    });
    no_mail_ifaces.insert("org.gnome.OnlineAccounts.PasswordBased".to_string(), HashMap::new());
    add_calendar_iface(&mut no_mail_ifaces, "https://ggraham@cloud.wahwahhut.com/remote.php/dav", false);
    add_contacts_iface(&mut no_mail_ifaces, "https://ggraham@cloud.wahwahhut.com/remote.php/dav/addressbooks/users/me/", false);
    objects.insert(OwnedObjectPath::try_from("/org/gnome/OnlineAccounts/Accounts/no_mail_account").unwrap(), no_mail_ifaces);

    let connection = zbus::connection::Builder::session()
        .expect("couldn't start building a session-bus connection - is DBUS_SESSION_BUS_ADDRESS set? run this test under `dbus-run-session --`")
        .build()
        .await
        .expect("couldn't connect to the session bus - run this test under `dbus-run-session --`");

    connection.object_server().at("/org/gnome/OnlineAccounts", FakeObjectManager { objects }).await.unwrap();
    connection
        .object_server()
        .at("/org/gnome/OnlineAccounts/Accounts/oauth_account", FakeAccount)
        .await
        .unwrap();
    connection
        .object_server()
        .at(
            "/org/gnome/OnlineAccounts/Accounts/oauth_account",
            FakeOAuth2Based {
                token: "fake-access-token".to_string(),
            },
        )
        .await
        .unwrap();
    connection.object_server().at("/org/gnome/OnlineAccounts/Accounts/pw_account", FakeAccount).await.unwrap();
    connection
        .object_server()
        .at("/org/gnome/OnlineAccounts/Accounts/pw_account", FakePasswordBased { password: "fake-pw".to_string() })
        .await
        .unwrap();
    connection
        .object_server()
        .at("/org/gnome/OnlineAccounts/Accounts/ms365_account", FakeAccount)
        .await
        .unwrap();
    connection
        .object_server()
        .at(
            "/org/gnome/OnlineAccounts/Accounts/ms365_account",
            FakeOAuth2Based {
                token: "fake-ms-token".to_string(),
            },
        )
        .await
        .unwrap();
    connection
        .object_server()
        .at("/org/gnome/OnlineAccounts/Accounts/no_mail_account", FakeAccount)
        .await
        .unwrap();
    connection
        .object_server()
        .at(
            "/org/gnome/OnlineAccounts/Accounts/no_mail_account",
            FakePasswordBased {
                password: "fake-nc-pw".to_string(),
            },
        )
        .await
        .unwrap();

    connection.request_name("org.gnome.OnlineAccounts").await.expect(
        "couldn't claim the org.gnome.OnlineAccounts bus name - a real GOA daemon (or another instance of \
             this test) is likely already running on this bus. Run under `dbus-run-session --` for an isolated bus.",
    );

    let client = GoaClient::connect().await.unwrap();
    let mut accounts = client.list_mail_accounts().await.unwrap();
    accounts.sort_by(|a, b| a.email.cmp(&b.email));

    assert_eq!(accounts.len(), 3, "expected exactly the three usable-mail accounts, got: {accounts:?}");
    assert_eq!(accounts[0].email, "oauth@example.com");
    assert!(matches!(accounts[0].auth, AuthMethod::OAuth2));
    assert_eq!(accounts[1].email, "password@example.com");
    assert!(matches!(accounts[1].auth, AuthMethod::Password { .. }));

    // The Microsoft 365 account gets hardcoded Exchange Online endpoints
    // (GOA reports empty hosts) and OAuth2 auth - the same credential path
    // the Google account uses.
    assert_eq!(accounts[2].email, "work@contoso.com");
    assert!(matches!(accounts[2].auth, AuthMethod::OAuth2));
    assert_eq!(accounts[2].imap.host, "outlook.office365.com");
    assert_eq!(accounts[2].imap.port, Some(993));
    assert!(accounts[2].imap.use_tls);
    assert_eq!(accounts[2].imap.username, "work@contoso.com");
    assert_eq!(accounts[2].smtp.host, "smtp.office365.com");
    assert_eq!(accounts[2].smtp.port, Some(587));
    assert!(accounts[2].smtp.use_tls);
    assert_eq!(accounts[2].smtp.username, "work@contoso.com");

    client.ensure_credentials(&accounts[0]).await.unwrap();
    let (token, expires_in) = client.get_access_token(&accounts[0]).await.unwrap();
    assert_eq!(token, "fake-access-token");
    assert_eq!(expires_in, 3600);

    let password = client.get_imap_password(&accounts[1]).await.unwrap();
    assert_eq!(password, "fake-pw-imap-password");

    // Calendar accounts are a fully separate set: `unsupported_account` (no
    // Calendar interface at all) must be excluded, `ms365_account`'s
    // empty-Uri Calendar interface is excluded too (matching live GOA), while
    // `no_mail_account` (no Mail interface, but a real Calendar one - mirrors
    // the live Nextcloud account this fixture is modeled on) must now appear
    // here even though it's absent from `list_mail_accounts()` above.
    let mut cal_accounts = client.list_calendar_accounts().await.unwrap();
    cal_accounts.sort_by(|a, b| a.uri.cmp(&b.uri));

    assert_eq!(cal_accounts.len(), 3, "expected exactly the three usable-calendar accounts, got: {cal_accounts:?}");
    assert_eq!(cal_accounts[0].uri, "https://caldav.example.com/dav/oauth@example.com/");
    assert!(matches!(cal_accounts[0].auth, CalendarAuthMethod::OAuth2));
    assert_eq!(cal_accounts[1].uri, "https://caldav.example.com/dav/password@example.com/");
    assert!(matches!(cal_accounts[1].auth, CalendarAuthMethod::Password { .. }));
    // The live-shaped Nextcloud URI comes out normalized: userinfo stripped
    // and the DAV path given its trailing slash. The `@` inside the other
    // accounts' *paths* (e.g. `oauth@example.com`) must be untouched.
    assert_eq!(cal_accounts[2].uri, "https://cloud.wahwahhut.com/remote.php/dav/");
    assert!(matches!(cal_accounts[2].auth, CalendarAuthMethod::Password { .. }));
    // `Identity` (the login the CalDAV server expects over Basic auth) is a
    // separate value from the display name for this account.
    assert_eq!(cal_accounts[2].identity, "ggraham");
    assert_eq!(cal_accounts[2].display_name, "ggraham@cloud.wahwahhut.com");
    assert_eq!(cal_accounts[2].provider_type.as_deref(), Some("owncloud"));

    client.ensure_credentials_calendar(&cal_accounts[0]).await.unwrap();
    let (cal_token, cal_expires_in) = client.get_access_token_calendar(&cal_accounts[0]).await.unwrap();
    assert_eq!(cal_token, "fake-access-token");
    assert_eq!(cal_expires_in, 3600);

    let cal_password = client.get_calendar_password(&cal_accounts[1]).await.unwrap();
    assert_eq!(cal_password, "fake-pw-calendar-password");

    let nextcloud_password = client.get_calendar_password(&cal_accounts[2]).await.unwrap();
    assert_eq!(nextcloud_password, "fake-nc-pw-calendar-password");

    // Contacts accounts are likewise a fully separate set from Mail.
    let mut contacts_accounts = client.list_contacts_accounts().await.unwrap();
    contacts_accounts.sort_by(|a, b| a.uri.cmp(&b.uri));

    assert_eq!(
        contacts_accounts.len(),
        3,
        "expected exactly the three usable-contacts accounts, got: {contacts_accounts:?}"
    );
    assert_eq!(contacts_accounts[0].uri, "https://carddav.example.com/dav/oauth@example.com/");
    assert!(matches!(contacts_accounts[0].auth, ContactsAuthMethod::OAuth2));
    assert_eq!(contacts_accounts[1].uri, "https://carddav.example.com/dav/password@example.com/");
    assert!(matches!(contacts_accounts[1].auth, ContactsAuthMethod::Password { .. }));
    assert_eq!(contacts_accounts[2].uri, "https://cloud.wahwahhut.com/remote.php/dav/addressbooks/users/me/");
    assert!(matches!(contacts_accounts[2].auth, ContactsAuthMethod::Password { .. }));
    assert_eq!(contacts_accounts[2].identity, "ggraham");
    assert_eq!(contacts_accounts[2].provider_type.as_deref(), Some("owncloud"));

    client.ensure_credentials_contacts(&contacts_accounts[0]).await.unwrap();
    let (contacts_token, contacts_expires_in) = client.get_access_token_contacts(&contacts_accounts[0]).await.unwrap();
    assert_eq!(contacts_token, "fake-access-token");
    assert_eq!(contacts_expires_in, 3600);

    let contacts_password = client.get_contacts_password(&contacts_accounts[1]).await.unwrap();
    assert_eq!(contacts_password, "fake-pw-contacts-password");
}
