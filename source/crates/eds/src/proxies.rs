//! Raw D-Bus proxy definitions for the EDS interfaces Lookout uses, kept
//! separate from `discovery.rs` and `sources.rs` so the wire-level shape is
//! easy to audit against EDS's own introspection XML
//! (`src/private/org.gnome.evolution.dataserver.*.xml` upstream).
//!
//! One set covers every supported release: the bus names below are unchanged
//! from EDS 3.34 to today, and none of the methods here changed signature in
//! that time (later releases only added members). If EDS bumps a bus name,
//! add the new set alongside this one and teach `discovery.rs` to pick.
//!
//! Recorded in EDS_TODO.md, "E0.1 spike notes":
//! - `Open*` on a factory returns the `(object_path, bus_name)` of the
//!   backend. Always use that pair - on default builds the bus name is the
//!   factory's own, on backend-per-process builds it's a
//!   `org.gnome.evolution.dataserver.Subprocess.Backend.*` name - and call
//!   `Open()` on it before anything else.
//! - Calendar strings are one bare `VEVENT`/`VTODO` each; a `VCALENDAR`
//!   wrapper is rejected. Batches are all-or-nothing.
//! - `mod_type` is GFlags nicks joined with `:` (`this`, `this-and-prior`,
//!   `this-and-future`, `all`, `only-this`); `opflags` is
//!   `ECalOperationFlags`/`EBookOperationFlags`, 0 for none.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::collections::HashMap;

use zbus::proxy;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

/// The source registry. Its presence on the bus is what `Eds::connect` checks.
pub const SOURCES_SERVICE: &str = "org.gnome.evolution.dataserver.Sources5";
/// The calendar, task-list and memo-list factory.
pub const CALENDAR_SERVICE: &str = "org.gnome.evolution.dataserver.Calendar8";
/// The address-book factory.
pub const ADDRESS_BOOK_SERVICE: &str = "org.gnome.evolution.dataserver.AddressBook10";

pub const SOURCE_MANAGER_PATH: &str = "/org/gnome/evolution/dataserver/SourceManager";
/// The interface every source object carries, with its `UID` and `Data`.
pub const SOURCE_INTERFACE: &str = "org.gnome.evolution.dataserver.Source";

/// The shape returned by `ObjectManager.GetManagedObjects`: object path ->
/// interface name -> property name -> value.
pub type ManagedObjects = HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>>;

/// The registry's object manager: one object per source
/// (`…/SourceManager/Source_N`; the numbers aren't stable, key by `UID`).
#[proxy(
    interface = "org.freedesktop.DBus.ObjectManager",
    default_service = "org.gnome.evolution.dataserver.Sources5",
    default_path = "/org/gnome/evolution/dataserver/SourceManager"
)]
pub trait SourceObjectManager {
    fn get_managed_objects(&self) -> zbus::Result<ManagedObjects>;

    #[zbus(signal)]
    fn interfaces_added(&self, object_path: OwnedObjectPath, interfaces: HashMap<String, HashMap<String, OwnedValue>>) -> zbus::Result<()>;

    #[zbus(signal)]
    fn interfaces_removed(&self, object_path: OwnedObjectPath, interfaces: Vec<String>) -> zbus::Result<()>;
}

#[proxy(
    interface = "org.gnome.evolution.dataserver.SourceManager",
    default_service = "org.gnome.evolution.dataserver.Sources5",
    default_path = "/org/gnome/evolution/dataserver/SourceManager"
)]
pub trait SourceManager {
    /// Maps each new source's UID (chosen by the caller) to its key-file
    /// text. The sources appear through `InterfacesAdded` shortly after.
    fn create_sources(&self, array: HashMap<&str, &str>) -> zbus::Result<()>;

    fn reload(&self) -> zbus::Result<()>;

    fn refresh_backend(&self, source_uid: &str) -> zbus::Result<()>;
}

/// `org.gnome.evolution.dataserver.Source` - bound to one source object per call.
#[proxy(interface = "org.gnome.evolution.dataserver.Source", default_service = "org.gnome.evolution.dataserver.Sources5")]
pub trait Source {
    #[zbus(property, name = "UID")]
    fn uid(&self) -> zbus::Result<String>;

    /// The source's whole key file, as stored in its `.source` file.
    #[zbus(property)]
    fn data(&self) -> zbus::Result<String>;
}

/// Present on every source; replaces the source's key file.
#[proxy(interface = "org.gnome.evolution.dataserver.Source.Writable", default_service = "org.gnome.evolution.dataserver.Sources5")]
pub trait SourceWritable {
    fn write(&self, data: &str) -> zbus::Result<()>;
}

/// Present only on sources the user may delete (including every source made
/// by `CreateSources`). Removing a local book or calendar leaves its data
/// directory on disk.
#[proxy(interface = "org.gnome.evolution.dataserver.Source.Removable", default_service = "org.gnome.evolution.dataserver.Sources5")]
pub trait SourceRemovable {
    fn remove(&self) -> zbus::Result<()>;
}

#[proxy(
    interface = "org.gnome.evolution.dataserver.CalendarFactory",
    default_service = "org.gnome.evolution.dataserver.Calendar8",
    default_path = "/org/gnome/evolution/dataserver/CalendarFactory"
)]
pub trait CalendarFactory {
    /// Returns `(object_path, bus_name)` of the backend.
    fn open_calendar(&self, source_uid: &str) -> zbus::Result<(String, String)>;

    fn open_task_list(&self, source_uid: &str) -> zbus::Result<(String, String)>;

    fn open_memo_list(&self, source_uid: &str) -> zbus::Result<(String, String)>;
}

#[proxy(
    interface = "org.gnome.evolution.dataserver.AddressBookFactory",
    default_service = "org.gnome.evolution.dataserver.AddressBook10",
    default_path = "/org/gnome/evolution/dataserver/AddressBookFactory"
)]
pub trait AddressBookFactory {
    /// Returns `(object_path, bus_name)` of the backend.
    fn open_address_book(&self, source_uid: &str) -> zbus::Result<(String, String)>;
}

/// An opened address-book backend, at the path and bus name the factory
/// returned. Queries are EBook S-expressions; field names use underscores
/// (`full_name`, `email`), and local books reject `#t`.
#[proxy(interface = "org.gnome.evolution.dataserver.AddressBook")]
pub trait AddressBook {
    fn open(&self) -> zbus::Result<Vec<String>>;

    fn close(&self) -> zbus::Result<()>;

    /// vCard 3.0 text, one card per string.
    fn get_contact_list(&self, query: &str) -> zbus::Result<Vec<String>>;

    fn get_contact_list_uids(&self, query: &str) -> zbus::Result<Vec<String>>;

    #[zbus(property)]
    fn writable(&self) -> zbus::Result<bool>;
}

/// An opened calendar, task-list or memo-list backend, at the path and bus
/// name the factory returned.
#[proxy(interface = "org.gnome.evolution.dataserver.Calendar")]
pub trait Calendar {
    fn open(&self) -> zbus::Result<Vec<String>>;

    fn close(&self) -> zbus::Result<()>;

    fn create_objects(&self, ics_objects: &[&str], opflags: u32) -> zbus::Result<Vec<String>>;

    fn modify_objects(&self, ics_objects: &[&str], mod_type: &str, opflags: u32) -> zbus::Result<()>;

    /// Each entry is `(uid, recurrence_id)`; the recurrence id is empty for a
    /// whole object.
    fn remove_objects(&self, uid_rid_array: &[(&str, &str)], mod_type: &str, opflags: u32) -> zbus::Result<()>;

    fn get_object(&self, uid: &str, rid: &str) -> zbus::Result<String>;

    /// A calendar S-expression: `#t`, `(uid? "…")`,
    /// `(occur-in-time-range? (make-time "…Z") (make-time "…Z"))`.
    fn get_object_list(&self, query: &str) -> zbus::Result<Vec<String>>;

    #[zbus(property)]
    fn writable(&self) -> zbus::Result<bool>;
}
