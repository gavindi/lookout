//! Exercises `lookout_eds::Eds` against a hand-rolled fake EDS on a private
//! `dbus-daemon` that each test starts for itself, giving real D-Bus-wire
//! coverage of version detection and source listing without a GNOME session.
//!
//! Unlike `lookout-goa`'s `fake_goa`, this needs no `dbus-run-session`: the
//! private bus has its own configuration, whose only service directory is a
//! per-test temp dir, so the real EDS (which owns these names on a desktop
//! session) is never seen, shadowed or activated. Needs `dbus-daemon` on
//! `PATH`.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

use lookout_eds::{Eds, EdsSourceKind};
use zbus::interface;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

const SOURCES5: &str = "org.gnome.evolution.dataserver.Sources5";
const CALENDAR8: &str = "org.gnome.evolution.dataserver.Calendar8";
const ADDRESS_BOOK10: &str = "org.gnome.evolution.dataserver.AddressBook10";
const SOURCE_MANAGER_PATH: &str = "/org/gnome/evolution/dataserver/SourceManager";

/// A `dbus-daemon` with its own config and service directory, stopped (and
/// its temp dir removed) on drop.
struct PrivateBus {
    daemon: Child,
    dir: PathBuf,
    address: String,
}

impl PrivateBus {
    /// `activatable` lists `(bus name, Exec line)` pairs to install as
    /// activatable services.
    fn start(activatable: &[(&str, String)]) -> PrivateBus {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!("lookout-eds-test-{}-{}", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed)));
        let services = dir.join("services");
        std::fs::create_dir_all(&services).unwrap();
        for (name, exec) in activatable {
            std::fs::write(services.join(format!("{name}.service")), format!("[D-BUS Service]\nName={name}\nExec={exec}\n")).unwrap();
        }
        let config = dir.join("bus.conf");
        std::fs::write(
            &config,
            format!(
                r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:dir={dir}</listen>
  <servicedir>{services}</servicedir>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#,
                dir = dir.display(),
                services = services.display()
            ),
        )
        .unwrap();
        let mut daemon = Command::new("dbus-daemon")
            .arg(format!("--config-file={}", config.display()))
            .args(["--nofork", "--print-address"])
            .stdout(Stdio::piped())
            .spawn()
            .expect("couldn't start dbus-daemon - these tests need it on PATH (Debian/Ubuntu package `dbus-daemon`)");
        let mut address = String::new();
        BufReader::new(daemon.stdout.take().unwrap()).read_line(&mut address).unwrap();
        let address = address.trim().to_string();
        assert!(!address.is_empty(), "dbus-daemon printed no address");
        PrivateBus { daemon, dir, address }
    }

    async fn connect(&self) -> zbus::Connection {
        zbus::connection::Builder::address(self.address.as_str()).unwrap().build().await.unwrap()
    }
}

impl Drop for PrivateBus {
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct FakeObjectManager {
    objects: HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>>,
}

#[interface(name = "org.freedesktop.DBus.ObjectManager")]
impl FakeObjectManager {
    fn get_managed_objects(&self) -> HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>> {
        self.objects.clone()
    }
}

fn prop(value: impl Into<Value<'static>>) -> OwnedValue {
    OwnedValue::try_from(value.into()).unwrap()
}

/// A registry whose sources are `(uid, key file)` pairs, at the object
/// paths EDS uses (`Source_N`), each with the interfaces a real source has.
fn registry(sources: &[(&str, &str)]) -> FakeObjectManager {
    let objects = sources
        .iter()
        .enumerate()
        .map(|(i, (uid, data))| {
            let mut source = HashMap::new();
            source.insert("UID".to_string(), prop(uid.to_string()));
            source.insert("Data".to_string(), prop(data.to_string()));
            source.insert("ConnectionStatus".to_string(), prop(String::new()));
            let mut interfaces = HashMap::new();
            interfaces.insert("org.gnome.evolution.dataserver.Source".to_string(), source);
            interfaces.insert("org.gnome.evolution.dataserver.Source.Writable".to_string(), HashMap::new());
            (OwnedObjectPath::try_from(format!("{SOURCE_MANAGER_PATH}/Source_{i}")).unwrap(), interfaces)
        })
        .collect();
    FakeObjectManager { objects }
}

/// Serves `registry` at the SourceManager path and claims `names` on a new
/// connection, which must be kept alive for the names to stay owned.
async fn fake_eds(bus: &PrivateBus, names: &[&str], registry: FakeObjectManager) -> zbus::Connection {
    let connection = bus.connect().await;
    connection.object_server().at(SOURCE_MANAGER_PATH, registry).await.unwrap();
    for name in names {
        connection.request_name(*name).await.unwrap();
    }
    connection
}

#[tokio::test]
async fn detects_the_supported_version() {
    // The calendar factory is running and the address-book factory is only
    // activatable (it starts on first use) - both count.
    let bus = PrivateBus::start(&[(ADDRESS_BOOK10, "/bin/false".to_string())]);
    let _service = fake_eds(&bus, &[SOURCES5, CALENDAR8], registry(&[])).await;

    let eds = Eds::connect_with(bus.connect().await).await.unwrap().expect("a supported EDS should be detected");
    assert!(eds.supports_calendars());
    assert!(eds.supports_address_books());
}

#[tokio::test]
async fn detects_a_partial_install() {
    let bus = PrivateBus::start(&[]);
    let _service = fake_eds(&bus, &[SOURCES5, ADDRESS_BOOK10], registry(&[])).await;

    let eds = Eds::connect_with(bus.connect().await).await.unwrap().expect("the registry plus one factory is usable");
    assert!(eds.supports_address_books());
    assert!(!eds.supports_calendars());
}

#[tokio::test]
async fn an_unknown_registry_version_is_unavailable() {
    let bus = PrivateBus::start(&[]);
    let _service = fake_eds(
        &bus,
        &[
            "org.gnome.evolution.dataserver.Sources6",
            "org.gnome.evolution.dataserver.Calendar9",
            "org.gnome.evolution.dataserver.AddressBook11",
        ],
        registry(&[]),
    )
    .await;

    assert!(Eds::connect_with(bus.connect().await).await.unwrap().is_none());
}

#[tokio::test]
async fn an_old_release_with_unknown_factories_is_unavailable() {
    // EDS 3.28-3.32: Sources5 alongside Calendar7 and AddressBook9.
    let bus = PrivateBus::start(&[("org.gnome.evolution.dataserver.AddressBook9", "/bin/false".to_string())]);
    let _service = fake_eds(&bus, &[SOURCES5, "org.gnome.evolution.dataserver.Calendar7"], registry(&[])).await;

    assert!(Eds::connect_with(bus.connect().await).await.unwrap().is_none());
}

#[tokio::test]
async fn an_activatable_but_stopped_registry_is_not_started() {
    // If detection activated the registry, the bus would run this and the
    // marker file would appear.
    let bus = PrivateBus::start(&[]);
    let marker = bus.dir.join("registry-started");
    std::fs::write(
        bus.dir.join("services").join(format!("{SOURCES5}.service")),
        format!("[D-BUS Service]\nName={SOURCES5}\nExec=/usr/bin/env touch {}\n", marker.display()),
    )
    .unwrap();
    let connection = bus.connect().await;
    // Make the daemon rescan its service directory.
    connection
        .call_method(Some("org.freedesktop.DBus"), "/org/freedesktop/DBus", Some("org.freedesktop.DBus"), "ReloadConfig", &())
        .await
        .unwrap();

    assert!(Eds::connect_with(connection).await.unwrap().is_none());
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(!marker.exists(), "detection must not start EDS");
}

#[tokio::test]
async fn reads_sources_including_goa_backed_ones() {
    let bus = PrivateBus::start(&[]);
    let _service = fake_eds(
        &bus,
        &[SOURCES5, CALENDAR8, ADDRESS_BOOK10],
        registry(&[
            ("local-stub", "[Data Source]\nDisplayName=On This Computer\nEnabled=true\nParent=\n"),
            (
                "system-address-book",
                "[Data Source]\nDisplayName=Personal\nEnabled=true\nParent=local-stub\n\n[Address Book]\nBackendName=local\n",
            ),
            (
                "cb7ed32b",
                "[Data Source]\nDisplayName=me@example.com\nEnabled=true\nParent=\n\n[Collection]\nBackendName=google\n\n[GNOME Online Accounts]\nAccountId=account_1676261544_5\n",
            ),
            (
                "ed17a042",
                "[Data Source]\nDisplayName=Contacts\nEnabled=true\nParent=cb7ed32b\n\n[Address Book]\nBackendName=carddav\n",
            ),
            (
                "a0f57032",
                "[Data Source]\nDisplayName=Holidays\nEnabled=true\nParent=cb7ed32b\n\n[Calendar]\nBackendName=caldav\nSelected=true\n",
            ),
            (
                "1c43d0a2",
                "[Data Source]\nDisplayName=work@example.org\nEnabled=true\nParent=3a3773b6\n\n[Mail Identity]\nAddress=work@example.org\n",
            ),
            (
                "3a3773b6",
                "[Data Source]\nDisplayName=work@example.org\nEnabled=true\nParent=\n\n[Mail Account]\nBackendName=imapx\n",
            ),
        ]),
    )
    .await;

    let eds = Eds::connect_with(bus.connect().await).await.unwrap().unwrap();
    let sources = eds.sources().await.unwrap();
    let summary: Vec<(&str, EdsSourceKind, Option<&str>, Option<&str>)> = sources
        .iter()
        .map(|s| (s.uid.as_str(), s.kind, s.backend.as_deref(), s.goa_account_id.as_deref()))
        .collect();
    assert_eq!(
        summary,
        [
            ("ed17a042", EdsSourceKind::AddressBook, Some("carddav"), Some("account_1676261544_5")),
            ("system-address-book", EdsSourceKind::AddressBook, Some("local"), None),
            ("a0f57032", EdsSourceKind::Calendar, Some("caldav"), Some("account_1676261544_5")),
            ("3a3773b6", EdsSourceKind::MailAccount, Some("imapx"), None),
        ]
    );
    let goa_book = &sources[0];
    assert_eq!(goa_book.display_name, "Contacts");
    assert_eq!(goa_book.parent_uid.as_deref(), Some("cb7ed32b"));
    assert!(goa_book.enabled);
}
