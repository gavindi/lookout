//! Ubuntu-dock unread badge via the Unity LauncherEntry D-Bus protocol.
//!
//! Ubuntu's dock (the GNOME Shell "Ubuntu Dock" extension, a Dash-to-Dock
//! fork) subscribes to every `com.canonical.Unity.LauncherEntry.Update`
//! signal on the session bus - any sender, any object path (its
//! `launcherAPI.js` subscribes with sender/path unset) - and paints the
//! `count`/`progress`/`urgent` properties from that signal's vardict onto
//! the icon of the app named by its `app_uri` (a `application://` URI of
//! the .desktop file id). No bus-name ownership is required, and when the
//! app's unique name disappears the dock drops the entry, so a badge can
//! never linger after Lookout quits.
//!
//! Everything here is best-effort like `background.rs`: no bus, no Unity
//! protocol consumer (plain GNOME Shell, KDE, ...), or a transient D-Bus
//! failure just logs at `debug!`. The window code decides *what* to show
//! (the Config toggle, the summed Inbox unread count) and only calls in
//! here once it has - this module only owns the wire format.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::collections::HashMap;
use std::sync::OnceLock;

use gtk::glib;
use zbus::zvariant::{OwnedValue, Value};

/// The .desktop file id as installed by the deb/rpm/flatpak packages.
const DESKTOP_FILE_ID: &str = "io.github.gavindi.Lookout.desktop";
/// Under snap confinement the .desktop file is renamed to the snap's own
/// `<name>_<name>.desktop`.
const SNAP_DESKTOP_FILE_ID: &str = "lookout_lookout.desktop";

/// The object path the signal is emitted from. The dock ignores it (any
/// path matches), but snapd's `unity7` AppArmor rule only lets a snapped
/// app send on `/com/canonical/unity/launcherentry/<digits>`, so this
/// path is the snap-compatible one.
const ENTRY_PATH: &str = "/com/canonical/unity/launcherentry/1";

/// The candidate `application://` URIs the badge is emitted for. The dock
/// keys a signal's count on the desktop-file id in the URI, and matches it
/// against the icon's `app.id` - which depends on how the app was launched,
/// not on anything Lookout controls:
///
/// * `<prgname>.desktop` - GNOME Shell's fuzzy id for a window whose binary
///   has no matching installed .desktop file (a bare `target/release/lookout`
///   run, or any launch outside the shell's own app system). The binary is
///   `lookout`, so this is `lookout.desktop`.
/// * `io.github.gavindi.Lookout.desktop` - the installed deb/rpm/flatpak
///   .desktop file, or a launch through it (pinned-dock click).
/// * `lookout_lookout.desktop` - the snap build, which snapd renames.
///
/// Emitting for all candidates costs a handful of D-Bus messages per count
/// change; unmatched ids are inert (the dock keeps a per-id stack and only
/// renders the one matching a docked icon), and the duplicate emissions
/// guarantee the badge lands no matter which id the shell picked.
fn candidate_app_uris() -> Vec<String> {
    let prgname = glib::prgname().map(|p| p.to_string()).unwrap_or_else(|| "lookout".to_string());
    let mut uris = vec![format!("application://{prgname}.desktop")];
    uris.push(format!("application://{DESKTOP_FILE_ID}"));
    if std::env::var("SNAP").is_ok() {
        uris.push(format!("application://{SNAP_DESKTOP_FILE_ID}"));
    }
    uris
}

/// The `a{sv}` vardict the Update signal carries. Only the two properties
/// Lookout uses are sent - the protocol says to include just what changed
/// (the dock keeps the rest at their defaults). `count` is an int64 per
/// the spec; `count-visible` hides the badge at zero so it can't linger
/// showing "0" once every message is read.
fn update_vardict(count: u32) -> HashMap<String, OwnedValue> {
    let mut properties: HashMap<String, OwnedValue> = HashMap::new();
    for (key, value) in [("count", Value::from(count as i64)), ("count-visible", Value::from(count > 0))] {
        // A `'static` `Value` always converts to `OwnedValue` - only
        // borrowed (non-static) payloads can fail, and none are used here.
        properties.insert(key.to_string(), OwnedValue::try_from(value).expect("static Value converts to OwnedValue"));
    }
    properties
}

/// The outbound queue of badge counts. `set_unread_count` sends the latest
/// count here from the UI thread (in call order); the single consumer task
/// `init` spawned drains it in order and emits each on the session bus. The
/// consumer dedupes, so only a count that actually differs is emitted, and
/// because the drain is strictly FIFO the badge always lands on the *last*
/// count the window published - a burst of rapid updates can't leave a stale
/// one painted last.
static TX: OnceLock<async_channel::Sender<u32>> = OnceLock::new();

/// Hands `worker`'s tokio handle to the badge machinery and starts the one
/// consumer task that owns the session-bus connection and every emission.
/// Called once at startup, before any window code can publish a count;
/// without it `set_unread_count` is a silent no-op (the `rebuild_folder_tree`
/// tests exercise `refresh_unread_indicators` without ever touching a bus).
pub fn init(worker: &crate::worker::Worker) {
    let (tx, rx) = async_channel::unbounded();
    let _ = TX.set(tx);
    worker.spawn(async move {
        badge_loop(rx).await;
    });
}

/// Drains `rx` in order, emitting each count on the session bus exactly once
/// per change. Runs on the worker's tokio runtime - zbus needs a reactor, and
/// the GLib main context the UI runs on has none, so the emissions must live
/// here. Only the latest count matters (the dock paints the badge from the
/// last signal it saw), so the loop is the single point where ordering is
/// guaranteed: each count is fully emitted before the next is read, and a
/// failed emission leaves `last` unset so the next queued count retries it.
async fn badge_loop(rx: async_channel::Receiver<u32>) {
    let mut last: Option<u32> = None;
    let mut connection: Option<zbus::Connection> = None;
    while let Ok(count) = rx.recv().await {
        if connection.is_none() {
            match zbus::Connection::session().await {
                Ok(c) => connection = Some(c),
                Err(e) => {
                    tracing::debug!("dock badge: no session bus, deferring count {count}: {e}");
                    continue;
                }
            }
        }
        let vardict = update_vardict(count);
        if last == Some(count) {
            continue;
        }
        let mut sent = true;
        for app_uri in candidate_app_uris() {
            let conn = connection.as_ref().expect("connection established above");
            if let Err(e) = conn
                .emit_signal(None::<&str>, ENTRY_PATH, "com.canonical.Unity.LauncherEntry", "Update", &(app_uri.clone(), vardict.clone()))
                .await
            {
                tracing::debug!("dock badge update for {app_uri} failed: {e}");
                sent = false;
                break;
            }
        }
        if sent {
            last = Some(count);
        }
    }
}

/// Emits the dock badge for `count` (the summed Inbox unread count) - or
/// hides it for zero. Safe to call from the UI thread; the count is queued
/// and the consumer task (see `init`) emits it in order on the worker's
/// tokio runtime. A session bus with no Unity-protocol listener (plain GNOME
/// Shell, KDE, no bus at all) just logs at `debug!`.
pub fn set_unread_count(count: u32) {
    let Some(tx) = TX.get() else { return };
    let _ = tx.try_send(count);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidates_cover_the_fuzzy_binary_id_and_the_packaged_id() {
        let uris = candidate_app_uris();
        assert!(uris.iter().any(|u| u == "application://lookout.desktop"));
        assert!(uris.iter().any(|u| u == "application://io.github.gavindi.Lookout.desktop"));
    }

    #[test]
    fn vardict_shows_the_count_and_visibility() {
        let props = update_vardict(7);
        let count = props["count"].downcast_ref::<i64>().unwrap();
        assert_eq!(count, 7);
        let visible = props["count-visible"].downcast_ref::<bool>().unwrap();
        assert!(visible);
    }

    #[test]
    fn vardict_hides_at_zero() {
        let props = update_vardict(0);
        let count = props["count"].downcast_ref::<i64>().unwrap();
        assert_eq!(count, 0);
        let visible = props["count-visible"].downcast_ref::<bool>().unwrap();
        assert!(!visible);
    }
}
