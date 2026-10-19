/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use zbus::fdo::DBusProxy;
use zbus::names::{BusName, OwnedBusName};
use zbus::proxy::CacheProperties;
use zbus::Connection;

use crate::proxies::{ADDRESS_BOOK_SERVICE, CALENDAR_SERVICE, SOURCES_SERVICE};
use crate::Result;

/// Bus names EDS services start with. Anything under this prefix that isn't
/// one of the names in `proxies` belongs to an EDS release with interface
/// versions Lookout doesn't know.
const EDS_NAME_PREFIX: &str = "org.gnome.evolution.dataserver.";

/// A session-bus connection known to have a supported EDS on the other end.
/// `Clone` is cheap - `zbus::Connection` is an `Arc`-backed handle.
#[derive(Clone)]
pub struct Eds {
    connection: Connection,
    address_books: bool,
    calendars: bool,
}

impl Eds {
    /// Connects to the session bus and checks for a supported EDS. See
    /// [`connect_with`](Self::connect_with).
    pub async fn connect() -> Result<Option<Eds>> {
        Self::connect_with(Connection::session().await?).await
    }

    /// Checks `connection`'s bus for a supported EDS. Returns `Ok(None)` when
    /// the source registry isn't running or when EDS's services use interface
    /// versions Lookout doesn't know.
    ///
    /// EDS is never started from here: all three services are D-Bus
    /// activatable, so this only asks the bus who owns what. On a GNOME
    /// session the registry is always running (Shell's calendar server starts
    /// it). The two factories start on demand, so for them "activatable"
    /// counts as present.
    pub async fn connect_with(connection: Connection) -> Result<Option<Eds>> {
        let dbus = DBusProxy::builder(&connection).cache_properties(CacheProperties::No).build().await?;

        if !dbus.name_has_owner(bus_name(SOURCES_SERVICE)).await? {
            // Only for the log: tell "no EDS" apart from "an EDS we don't know".
            match dbus.list_names().await {
                Ok(names) => {
                    let unknown = unknown_eds_names(&names);
                    if unknown.is_empty() {
                        tracing::debug!("EDS: source registry {SOURCES_SERVICE} isn't running");
                    } else {
                        tracing::info!(
                            "EDS: unsupported interface versions on the bus ({}); desktop address books and calendars are unavailable",
                            unknown.join(", ")
                        );
                    }
                }
                Err(e) => tracing::debug!("EDS: source registry isn't running (couldn't list bus names: {e})"),
            }
            return Ok(None);
        }

        let activatable = match dbus.list_activatable_names().await {
            Ok(names) => Some(names),
            Err(e) => {
                tracing::debug!("EDS: couldn't list activatable names ({e}); assuming the factories match the registry");
                None
            }
        };
        let address_books = factory_available(dbus.name_has_owner(bus_name(ADDRESS_BOOK_SERVICE)).await?, activatable.as_deref(), ADDRESS_BOOK_SERVICE);
        let calendars = factory_available(dbus.name_has_owner(bus_name(CALENDAR_SERVICE)).await?, activatable.as_deref(), CALENDAR_SERVICE);
        if !address_books && !calendars {
            // EDS 3.28-3.32 ran `Sources5` with `Calendar7`/`AddressBook9`.
            tracing::info!("EDS: source registry is running but neither {CALENDAR_SERVICE} nor {ADDRESS_BOOK_SERVICE} is available; treating EDS as unavailable");
            return Ok(None);
        }
        tracing::debug!("EDS: available (address books: {address_books}, calendars: {calendars})");
        Ok(Some(Eds {
            connection,
            address_books,
            calendars,
        }))
    }

    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    /// Whether the address-book factory (`AddressBook10`) is running or
    /// activatable.
    pub fn supports_address_books(&self) -> bool {
        self.address_books
    }

    /// Whether the calendar factory (`Calendar8`) is running or activatable.
    pub fn supports_calendars(&self) -> bool {
        self.calendars
    }
}

fn bus_name(name: &'static str) -> BusName<'static> {
    BusName::from_static_str(name).expect("EDS service names are valid bus names")
}

/// A factory is available when it's running or the bus can start it. When the
/// activatable list couldn't be read (some confined environments refuse
/// `ListActivatableNames`), the running registry is taken as evidence enough:
/// every release since 3.34 ships matching factories, and a mismatch would
/// still only surface as a failed `Open*`, which callers treat as unavailable.
fn factory_available(running: bool, activatable: Option<&[OwnedBusName]>, name: &str) -> bool {
    running || activatable.is_none_or(|names| names.iter().any(|n| n.as_str() == name))
}

/// EDS service names on the bus that aren't the versions in `proxies` (for
/// example `org.gnome.evolution.dataserver.Sources4`), sorted.
fn unknown_eds_names(names: &[OwnedBusName]) -> Vec<String> {
    let known = [SOURCES_SERVICE, CALENDAR_SERVICE, ADDRESS_BOOK_SERVICE];
    let mut unknown: Vec<String> = names
        .iter()
        .map(|n| n.as_str())
        .filter(|n| {
            n.strip_prefix(EDS_NAME_PREFIX).is_some_and(|service| {
                ["Sources", "Calendar", "AddressBook"]
                    .iter()
                    .any(|base| service.strip_prefix(base).is_some_and(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit())))
            })
        })
        .filter(|n| !known.contains(n))
        .map(str::to_string)
        .collect();
    unknown.sort();
    unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<OwnedBusName> {
        list.iter().map(|n| OwnedBusName::try_from(*n).unwrap()).collect()
    }

    #[test]
    fn factory_counts_when_running_or_activatable() {
        let activatable = names(&[ADDRESS_BOOK_SERVICE, "org.example.Other"]);
        assert!(factory_available(true, Some(&activatable), CALENDAR_SERVICE));
        assert!(factory_available(false, Some(&activatable), ADDRESS_BOOK_SERVICE));
        assert!(!factory_available(false, Some(&activatable), CALENDAR_SERVICE));
        assert!(!factory_available(false, Some(&names(&["org.gnome.evolution.dataserver.Calendar7"])), CALENDAR_SERVICE));
    }

    #[test]
    fn unreadable_activatable_list_trusts_the_registry() {
        assert!(factory_available(false, None, CALENDAR_SERVICE));
    }

    #[test]
    fn unknown_versions_are_reported_and_known_ones_and_others_are_not() {
        let on_bus = names(&[
            ":1.42",
            "org.gnome.evolution.dataserver.Sources4",
            "org.gnome.evolution.dataserver.Calendar9",
            SOURCES_SERVICE,
            ADDRESS_BOOK_SERVICE,
            "org.gnome.evolution.dataserver.UserPrompter0",
            "org.gnome.evolution.dataserver.Subprocess.Backend.Calendarx12x3",
            "org.gnome.Evolution-alarm-notify",
        ]);
        assert_eq!(
            unknown_eds_names(&on_bus),
            ["org.gnome.evolution.dataserver.Calendar9", "org.gnome.evolution.dataserver.Sources4"]
        );
        assert!(unknown_eds_names(&names(&[":1.1", "org.freedesktop.DBus"])).is_empty());
    }
}
