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

use std::collections::HashMap;

use lookout_goa::{AuthMethod, GoaClient};
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

#[tokio::test]
async fn discovers_and_fetches_credentials_over_real_dbus_wire() {
    let mut objects = HashMap::new();

    let mut oauth_ifaces = mail_account_props("oauth@example.com", true);
    oauth_ifaces.insert("org.gnome.OnlineAccounts.OAuth2Based".to_string(), HashMap::new());
    objects.insert(OwnedObjectPath::try_from("/org/gnome/OnlineAccounts/Accounts/oauth_account").unwrap(), oauth_ifaces);

    let mut pw_ifaces = mail_account_props("password@example.com", true);
    pw_ifaces.insert("org.gnome.OnlineAccounts.PasswordBased".to_string(), HashMap::new());
    objects.insert(OwnedObjectPath::try_from("/org/gnome/OnlineAccounts/Accounts/pw_account").unwrap(), pw_ifaces);

    // An account whose Mail interface is present but unusable (mirrors the
    // real Microsoft 365 account observed against live GOA during
    // development: Mail interface exists, ImapSupported is false) - must be
    // filtered out by list_mail_accounts().
    let mut unsupported_ifaces = mail_account_props("unsupported@example.com", false);
    unsupported_ifaces.insert("org.gnome.OnlineAccounts.OAuth2Based".to_string(), HashMap::new());
    objects.insert(
        OwnedObjectPath::try_from("/org/gnome/OnlineAccounts/Accounts/unsupported_account").unwrap(),
        unsupported_ifaces,
    );

    // An account with no Mail interface at all (mirrors the Nextcloud
    // account observed live: only Contacts/Calendar/Files/PasswordBased).
    let mut no_mail_ifaces = HashMap::new();
    no_mail_ifaces.insert("org.gnome.OnlineAccounts.Account".to_string(), {
        let mut m = HashMap::new();
        m.insert("MailDisabled".to_string(), prop(false));
        m
    });
    no_mail_ifaces.insert("org.gnome.OnlineAccounts.PasswordBased".to_string(), HashMap::new());
    objects.insert(OwnedObjectPath::try_from("/org/gnome/OnlineAccounts/Accounts/no_mail_account").unwrap(), no_mail_ifaces);

    let connection = zbus::connection::Builder::session()
        .expect("couldn't start building a session-bus connection - is DBUS_SESSION_BUS_ADDRESS set? run this test under `dbus-run-session --`")
        .build()
        .await
        .expect("couldn't connect to the session bus - run this test under `dbus-run-session --`");

    connection
        .object_server()
        .at("/org/gnome/OnlineAccounts", FakeObjectManager { objects })
        .await
        .unwrap();
    connection.object_server().at("/org/gnome/OnlineAccounts/Accounts/oauth_account", FakeAccount).await.unwrap();
    connection
        .object_server()
        .at("/org/gnome/OnlineAccounts/Accounts/oauth_account", FakeOAuth2Based { token: "fake-access-token".to_string() })
        .await
        .unwrap();
    connection.object_server().at("/org/gnome/OnlineAccounts/Accounts/pw_account", FakeAccount).await.unwrap();
    connection
        .object_server()
        .at("/org/gnome/OnlineAccounts/Accounts/pw_account", FakePasswordBased { password: "fake-pw".to_string() })
        .await
        .unwrap();

    connection
        .request_name("org.gnome.OnlineAccounts")
        .await
        .expect(
            "couldn't claim the org.gnome.OnlineAccounts bus name - a real GOA daemon (or another instance of \
             this test) is likely already running on this bus. Run under `dbus-run-session --` for an isolated bus.",
        );

    let client = GoaClient::connect().await.unwrap();
    let mut accounts = client.list_mail_accounts().await.unwrap();
    accounts.sort_by(|a, b| a.email.cmp(&b.email));

    assert_eq!(accounts.len(), 2, "expected exactly the two usable-mail accounts, got: {accounts:?}");
    assert_eq!(accounts[0].email, "oauth@example.com");
    assert!(matches!(accounts[0].auth, AuthMethod::OAuth2));
    assert_eq!(accounts[1].email, "password@example.com");
    assert!(matches!(accounts[1].auth, AuthMethod::Password { .. }));

    client.ensure_credentials(&accounts[0]).await.unwrap();
    let (token, expires_in) = client.get_access_token(&accounts[0]).await.unwrap();
    assert_eq!(token, "fake-access-token");
    assert_eq!(expires_in, 3600);

    let password = client.get_imap_password(&accounts[1]).await.unwrap();
    assert_eq!(password, "fake-pw-imap-password");
}
