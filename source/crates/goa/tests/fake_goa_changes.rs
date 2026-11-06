//! Exercises `GoaClient::account_changes` against a fake GOA service that
//! emits `ObjectManager` signals over a real D-Bus session bus. A separate
//! test binary from `fake_goa.rs`, because both claim the well-known name
//! `org.gnome.OnlineAccounts` and test binaries run one after another.
//!
//! Must run under an isolated session bus, never the desktop's (it would
//! shadow the real goa-daemon):
//!   dbus-run-session -- cargo test -p lookout-goa --test fake_goa_changes

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::collections::HashMap;
use std::time::Duration;

use lookout_goa::GoaClient;
use zbus::interface;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

type Interfaces = HashMap<String, HashMap<String, OwnedValue>>;

struct FakeObjectManager;

#[interface(name = "org.freedesktop.DBus.ObjectManager")]
impl FakeObjectManager {
    fn get_managed_objects(&self) -> HashMap<OwnedObjectPath, Interfaces> {
        HashMap::new()
    }
}

const MANAGER_PATH: &str = "/org/gnome/OnlineAccounts";
const MANAGER_IFACE: &str = "org.freedesktop.DBus.ObjectManager";

async fn emit_added(connection: &zbus::Connection, account: &str) {
    let path = OwnedObjectPath::try_from(format!("/org/gnome/OnlineAccounts/Accounts/{account}")).unwrap();
    let interfaces: Interfaces = HashMap::from([("org.gnome.OnlineAccounts.Mail".to_string(), HashMap::new())]);
    connection
        .emit_signal(None::<&str>, MANAGER_PATH, MANAGER_IFACE, "InterfacesAdded", &(path, interfaces))
        .await
        .unwrap();
}

async fn emit_removed(connection: &zbus::Connection, account: &str) {
    let path = OwnedObjectPath::try_from(format!("/org/gnome/OnlineAccounts/Accounts/{account}")).unwrap();
    let interfaces = vec!["org.gnome.OnlineAccounts.Mail".to_string()];
    connection
        .emit_signal(None::<&str>, MANAGER_PATH, MANAGER_IFACE, "InterfacesRemoved", &(path, interfaces))
        .await
        .unwrap();
}

#[tokio::test]
async fn account_changes_fire_on_added_and_removed_and_coalesce_a_burst() {
    let service = zbus::connection::Builder::session()
        .expect("no session bus - run this test under `dbus-run-session --`")
        .build()
        .await
        .expect("no session bus - run this test under `dbus-run-session --`");
    service.object_server().at(MANAGER_PATH, FakeObjectManager).await.unwrap();
    service
        .request_name("org.gnome.OnlineAccounts")
        .await
        .expect("couldn't claim org.gnome.OnlineAccounts - is a real GOA daemon on this bus? run under `dbus-run-session --`");

    let client = GoaClient::connect().await.unwrap();
    let mut changes = client.account_changes().await.unwrap();
    let settle = Duration::from_millis(200);

    // Adding one account exports several interfaces in a burst: one wakeup.
    for account in ["new_account", "new_account", "new_account"] {
        emit_added(&service, account).await;
    }
    let woke = tokio::time::timeout(Duration::from_secs(5), changes.next_settled(settle)).await;
    assert_eq!(woke.ok(), Some(true), "a burst of InterfacesAdded wakes the watcher");
    let again = tokio::time::timeout(Duration::from_millis(500), changes.next_settled(settle)).await;
    assert!(again.is_err(), "the burst was coalesced into one wakeup");

    // Removing an account (or switching a service off) is a change too.
    emit_removed(&service, "new_account").await;
    let woke = tokio::time::timeout(Duration::from_secs(5), changes.next_settled(settle)).await;
    assert_eq!(woke.ok(), Some(true), "InterfacesRemoved wakes the watcher");
}
