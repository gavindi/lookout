//! Opens the GNOME Online Accounts panel in the system settings app.
//!
//! Un-sandboxed, `gnome-control-center online-accounts` works, but a Flatpak
//! or snap can't exec host binaries - the settings app must be reached over
//! the session bus instead:
//!
//! * GNOME 48+ hosts expose the panel as the `launch-panel` GAction on the
//!   `org.gnome.Settings` bus name (invoked via `org.freedesktop.Application`
//!   `ActivateAction`, with the panel id + arg list as its parameter).
//! * GNOME 47 and older exposed `ActivatePanel` on `org.gnome.ControlCenter`.
//!
//! Both are tried in turn, with the plain spawn kept as a last resort (dev
//! runs outside any sandbox, where it still works).

use std::collections::HashMap;

use zbus::zvariant::Value;

const ONLINE_ACCOUNTS_PANEL: &str = "online-accounts";

/// Best-effort: every failure mode falls through to the next option, so the
/// user always ends up with either the settings panel or a launch attempt.
pub async fn open_online_accounts() {
    if let Ok(conn) = zbus::Connection::session().await {
        // GNOME 48+: `org.gnome.Settings` registers a `launch-panel` GAction
        // taking a `(sav)` parameter: (panel id, extra arguments).
        let launch = {
            let params = Value::from((ONLINE_ACCOUNTS_PANEL, Vec::<Value>::new()));
            let platform_data = HashMap::<String, Value>::new();
            conn.call_method(
                Some("org.gnome.Settings"),
                "/org/freedesktop/Application",
                Some("org.freedesktop.Application"),
                "ActivateAction",
                &("launch-panel", params, platform_data),
            )
            .await
        };
        if launch.is_ok() {
            return;
        }
        // GNOME <=47: `org.gnome.ControlCenter.ActivatePanel(panel id)`.
        let legacy = conn
            .call_method(
                Some("org.gnome.ControlCenter"),
                "/org/gnome/ControlCenter",
                Some("org.gnome.ControlCenter"),
                "ActivatePanel",
                &(ONLINE_ACCOUNTS_PANEL,),
            )
            .await;
        if legacy.is_ok() {
            return;
        }
    }
    // Unsandboxed fallback: the control-center CLI still accepts panel ids.
    let _ = std::process::Command::new("gnome-control-center").arg(ONLINE_ACCOUNTS_PANEL).spawn();
}
