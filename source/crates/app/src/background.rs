//! Background-running and login-autostart support - the machinery that
//! keeps Lookout alive (and notifying) when no window is on screen.
//!
//! Two mechanisms, layered:
//!
//! * The session's **Background portal**
//!   (`org.freedesktop.portal.Background`, v2 API): `RequestBackground`
//!   both asks the shell for permission to run without a window (it shows
//!   a dialog the first time, then remembers) and, with `autostart: true`
//!   plus a `commandline`, registers the app to launch at login. The shell
//!   owns that registration (GNOME: Settings → Apps), so it can't be
//!   revoked over D-Bus - see [`disable_login_autostart`].
//! * A managed **XDG autostart file**
//!   (`~/.config/autostart/io.github.gavindi.Lookout.desktop`) written by
//!   the app itself when no portal is available (or the portal call fails
//!   outright). Disabling removes the file.
//!
//! Every function is best-effort: the portal path logs instead of failing,
//! and the file path returns `io::Result`s for the caller to report. The
//! window code decides *when* to call in (the Config toggles, the window's
//! close request) - nothing here touches the UI.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::StreamExt;
use gtk::glib;
use zbus::proxy;
use zbus::zvariant::{OwnedValue, Value};

/// The autostart entry's file name under `~/.config/autostart`.
pub const AUTOSTART_FILE_NAME: &str = "io.github.gavindi.Lookout.desktop";

/// How long to wait for the portal's `Request::Response` before giving up
/// (the shell's decision dialog is interactive, so this is generous).
const PORTAL_RESPONSE_TIMEOUT: Duration = Duration::from_secs(60);

/// `org.freedesktop.portal.Background` (v2): the session portal's
/// background/autostart interface. v2's `RequestBackground` is
/// asynchronous - it returns a `Request` object whose `Response` signal
/// carries the outcome (whether the app may run in the background and/or
/// autostart at login).
#[proxy(
    interface = "org.freedesktop.portal.Background",
    default_service = "org.freedesktop.portal.Desktop",
    default_path = "/org/freedesktop/portal/desktop"
)]
trait BackgroundPortal {
    fn request_background(&self, parent_window: &str, options: HashMap<String, OwnedValue>) -> zbus::Result<zbus::zvariant::OwnedObjectPath>;
    fn set_status(&self, options: HashMap<String, OwnedValue>) -> zbus::Result<()>;
    #[zbus(property)]
    fn version(&self) -> zbus::Result<u32>;
}

/// The per-request `org.freedesktop.portal.Request` object: its `Response`
/// signal is the portal's answer to a `RequestBackground` call. The
/// generated args struct carries a status (`response`, 0 = success) plus
/// the `background`/`autostart` boolean results in a vardict.
#[proxy(interface = "org.freedesktop.portal.Request", default_service = "org.freedesktop.portal.Desktop")]
trait PortalRequest {
    #[zbus(signal)]
    fn response(&self, response: u32, results: HashMap<String, OwnedValue>) -> zbus::Result<()>;
}

/// A valid object-path element identifying this request to the portal.
/// The portal rejects some characters the object-path grammar allows
/// (dashes included) - underscores are known-good.
fn handle_token() -> String {
    format!("lookout_{}", uuid::Uuid::new_v4().simple())
}

/// One `RequestBackground` round-trip: subscribes to the response *before*
/// calling (so a fast portal can't slip the signal past the subscription),
/// then returns the results vardict. `Err` means the portal is absent,
/// unreachable, denied the request, or took too long - the caller decides
/// what that means for its fallback.
async fn request_background(options: HashMap<String, OwnedValue>) -> Result<HashMap<String, OwnedValue>, String> {
    let connection = zbus::Connection::session().await.map_err(|e| e.to_string())?;
    let portal = BackgroundPortalProxy::new(&connection).await.map_err(|e| e.to_string())?;
    let handle = portal.request_background("", options).await.map_err(|e| e.to_string())?;
    let request = PortalRequestProxy::builder(&connection)
        .destination("org.freedesktop.portal.Desktop")
        .map_err(|e| e.to_string())?
        .path(handle)
        .map_err(|e| e.to_string())?
        .build()
        .await
        .map_err(|e| e.to_string())?;
    let mut responses = request.receive_response().await.map_err(|e| e.to_string())?;
    let response = tokio::time::timeout(PORTAL_RESPONSE_TIMEOUT, responses.next())
        .await
        .map_err(|_| "timed out waiting for the portal's answer".to_string())?
        .ok_or_else(|| "the portal closed the response stream".to_string())?;
    let args = response.args().map_err(|e| e.to_string())?;
    if args.response != 0 {
        return Err(format!("the portal denied the request (response {})", args.response));
    }
    Ok(args.results)
}

/// Builds the `a{sv}` options vardict for `RequestBackground` from plain
/// `Value`s (which convert freely from `&str`/`String`/`bool`/arrays).
fn options_vardict(items: Vec<(&str, Value<'static>)>) -> HashMap<String, OwnedValue> {
    items
        .into_iter()
        // A `'static` `Value` always converts to `OwnedValue` - only
        // borrowed (non-static) payloads can fail, and we never build one.
        .map(|(key, value)| (key.to_string(), OwnedValue::try_from(value).expect("static Value converts to OwnedValue")))
        .collect()
}

/// The outcome of asking the portal to autostart the app at login.
pub enum AutostartDecision {
    /// The shell registered the autostart.
    Granted,
    /// The shell (or its policy) declined - respect it, no fallback.
    Denied,
    /// No portal, or the request failed outright - fall back to the
    /// self-managed XDG autostart file.
    Unavailable,
}

/// Asks the portal to start Lookout at login (hidden, via `--hidden`) and
/// run it in the background.
pub async fn request_portal_autostart() -> AutostartDecision {
    let options = options_vardict(vec![
        ("handle_token", Value::from(handle_token())),
        ("reason", Value::from("Start Lookout at login so your mail and calendar notifications keep working")),
        ("autostart", Value::from(true)),
        ("commandline", Value::from(vec!["lookout".to_string(), "--hidden".to_string()])),
    ]);
    match request_background(options).await {
        Ok(results) => match results.get("autostart") {
            Some(value) => match value.downcast_ref::<bool>() {
                Ok(true) => AutostartDecision::Granted,
                _ => {
                    tracing::debug!(?results, "the portal answered without autostart approval");
                    AutostartDecision::Denied
                }
            },
            None => {
                tracing::debug!(?results, "the portal answered without an autostart result");
                AutostartDecision::Denied
            }
        },
        Err(e) => {
            tracing::debug!("background portal unavailable: {e}");
            AutostartDecision::Unavailable
        }
    }
}

/// One-shot: ask the portal to allow running in the background (windowless).
/// Called at startup when the close-to-background setting is on; the shell
/// shows a dialog the first time, then remembers. Fire-and-forget - the
/// outcome only affects where the shell lists the app.
pub async fn request_background_approval() {
    let options = options_vardict(vec![
        ("handle_token", Value::from(handle_token())),
        ("reason", Value::from("Lookout stays running to sync your mail and calendar")),
    ]);
    match request_background(options).await {
        Ok(results) => tracing::debug!(?results, "background portal approved background running"),
        Err(e) => tracing::debug!("background portal unavailable: {e}"),
    }
}

/// v2 `SetStatus`: the one-line message the shell shows under the app in
/// its background-apps list. Call when hiding (and clear with `""` when
/// showing again).
pub async fn set_background_status(message: &str) {
    let Ok(connection) = zbus::Connection::session().await else { return };
    let Ok(portal) = BackgroundPortalProxy::new(&connection).await else { return };
    let options = options_vardict(vec![("message", Value::from(message.to_string()))]);
    if let Err(e) = portal.set_status(options).await {
        tracing::debug!("background portal status update failed: {e}");
    }
}

/// The autostart entry Lookout manages itself - `~/.config/autostart/…`.
/// `Exec` carries `--hidden` so a login launch starts without a window
/// (the entry assumes the installed `lookout` binary on `PATH`, i.e. a
/// packaged install).
pub fn autostart_file_path() -> PathBuf {
    let mut dir = glib::user_config_dir();
    dir.push("autostart");
    dir.push(AUTOSTART_FILE_NAME);
    dir
}

fn autostart_file_content() -> &'static str {
    "[Desktop Entry]\nType=Application\nName=Lookout\nComment=Start at login to keep mail and calendar notifications working\nExec=lookout --hidden\nIcon=io.github.gavindi.Lookout\nTerminal=false\nX-GNOME-Autostart-enabled=true\n"
}

fn write_autostart_file_in(path: &Path) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    // Write the temp file next to the target, then rename over it, so a
    // half-written entry can never be picked up by the autostart generator.
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    fs::write(&tmp, autostart_file_content())?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Writes the app's own autostart entry (creating `~/.config/autostart`
/// if needed).
pub fn write_autostart_file() -> std::io::Result<()> {
    write_autostart_file_in(&autostart_file_path())
}

fn remove_autostart_file_in(path: &Path) -> std::io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Removes the app's own autostart entry.
pub fn remove_autostart_file() -> std::io::Result<()> {
    remove_autostart_file_in(&autostart_file_path())
}

/// Whether the app's own autostart entry exists right now.
pub fn autostart_file_exists() -> bool {
    autostart_file_path().is_file()
}

/// The full enable path used by the Config toggle: the portal when the
/// session provides it (respecting a denial - no fallback then), the
/// managed XDG file otherwise. Returns whether login autostart is active.
pub async fn enable_login_autostart() -> bool {
    match request_portal_autostart().await {
        AutostartDecision::Granted => true,
        AutostartDecision::Denied => false,
        AutostartDecision::Unavailable => match write_autostart_file() {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(error = %e, "could not write the autostart entry");
                false
            }
        },
    }
}

/// The full disable path: removes the managed XDG entry. A registration the
/// portal made lives in the desktop's own app settings (the portal API has
/// no unregister call), which the Config row's subtitle points at.
pub fn disable_login_autostart() {
    if let Err(e) = remove_autostart_file() {
        tracing::warn!(error = %e, "could not remove the autostart entry");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lookout-autostart-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn writes_the_autostart_entry_with_the_hidden_flag() {
        let dir = temp_dir("write");
        let path = dir.join(AUTOSTART_FILE_NAME);
        write_autostart_file_in(&path).expect("write entry");
        let content = fs::read_to_string(&path).expect("read entry");
        assert!(content.contains("[Desktop Entry]"));
        assert!(content.contains("Exec=lookout --hidden"));
        assert!(content.contains("X-GNOME-Autostart-enabled=true"));
    }

    #[test]
    fn creates_the_autostart_directory() {
        let dir = temp_dir("mkdir");
        let path = dir.join("does-not-exist").join(AUTOSTART_FILE_NAME);
        write_autostart_file_in(&path).expect("write entry");
        assert!(path.is_file());
    }

    #[test]
    fn removes_the_entry_and_tolerates_absence() {
        let dir = temp_dir("remove");
        let path = dir.join(AUTOSTART_FILE_NAME);
        write_autostart_file_in(&path).expect("write entry");
        remove_autostart_file_in(&path).expect("remove entry");
        assert!(!path.exists());
        remove_autostart_file_in(&path).expect("removing an absent entry is fine");
    }
}
