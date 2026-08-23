//! Optional StatusNotifierItem tray icon (the AppIndicator protocol).
//!
//! Shows the app in the notification area - Ubuntu's AppIndicator extension,
//! KDE Plasma's system tray, and other SNI hosts - with the summed Inbox
//! unread count baked into the icon, a tooltip naming the count, and a
//! right-click menu (Open Lookout / Compose… / Quit). Left-click toggles
//! the main window's visibility (the "show/hide" this feature exists for).
//!
//! Everything is opt-in via the Config → General → "Tray icon" toggle
//! (`tray-icon-enabled` GSettings key, default off), and best-effort like
//! `background.rs`: no StatusNotifierWatcher on the session bus (plain
//! GNOME Shell without the extension) means the item simply never appears,
//! logged at `debug!` - ksni itself stays offline and retries until a
//! watcher shows up. The window code decides *when* the icon is wanted and
//! *what* the count is (`refresh_unread_indicators` in `window.rs`); this
//! module owns the SNI wire format, the icon rendering, and the menu.
//!
//! The icon is rendered on the UI thread (GTK's icon theme and cairo are
//! not thread-safe) as ARGB32 pixmaps at the sizes hosts commonly request
//! (16/22/24 px), then handed to the tray service thread through
//! `ksni::Handle::update`. The app's icon comes from the icon theme when
//! it resolves, with a cairo-drawn envelope fallback when it doesn't (a
//! headless run, or a snap without librsvg to decode the SVG); the unread
//! count rides in a red badge at the icon's bottom-right corner, `99+`
//! past 99 - the same compact convention the dock badge's "0.9.91" work
//! adopted.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use ksni::menu::StandardItem;
use ksni::{Icon, MenuItem, TrayMethods};

use gtk::cairo;

/// The well-known icon name this app registers under.
const TRAY_ID: &str = "lookout";

/// How long `start` waits for the tray service to come up on the session
/// bus before giving up. A missing StatusNotifierWatcher fails fast (ksni
/// stays offline and retries), so this only guards a stalled bus.
const START_TIMEOUT: Duration = Duration::from_secs(2);

/// One action the tray's menu or click wants performed on the UI thread.
/// The tray service runs on the worker's tokio runtime; the window code
/// owns the widgets, so everything crosses back over this channel and the
/// window dispatches it on the GLib main context (see `build_window`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayCommand {
    /// Left-click: show the window if hidden, hide it if shown.
    ToggleWindow,
    /// "Open Lookout" menu item: present the window.
    OpenWindow,
    /// "Compose…" menu item: open a new-message composer.
    Compose,
    /// "Quit" menu item.
    Quit,
}

/// The tray item, updated from the UI thread via `ksni::Handle::update`.
pub struct LookoutTray {
    /// The summed Inbox unread count, shown in the icon badge and tooltip.
    count: u32,
    /// Pre-rendered ARGB32 pixmaps (one per commonly-requested size).
    icon: Vec<Icon>,
    /// Back-channel to the window's command dispatcher.
    commands: async_channel::Sender<TrayCommand>,
}

impl ksni::Tray for LookoutTray {
    fn id(&self) -> String {
        TRAY_ID.into()
    }

    fn category(&self) -> ksni::Category {
        ksni::Category::Communications
    }

    fn title(&self) -> String {
        "Lookout".into()
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        self.icon.clone()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            icon_name: String::new(),
            icon_pixmap: self.icon.clone(),
            title: "Lookout".into(),
            description: match self.count {
                0 => String::new(),
                1 => "1 unread message".into(),
                n => format!("{n} unread messages"),
            },
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.commands.try_send(TrayCommand::ToggleWindow);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut menu = Vec::new();
        for (label, icon, command) in [
            ("Open Lookout", "go-home-symbolic", TrayCommand::OpenWindow),
            ("Compose…", "mail-message-new-symbolic", TrayCommand::Compose),
        ] {
            let commands = self.commands.clone();
            menu.push(
                StandardItem {
                    label: label.into(),
                    icon_name: icon.into(),
                    activate: Box::new(move |_| {
                        let _ = commands.try_send(command);
                    }),
                    ..Default::default()
                }
                .into(),
            );
        }
        menu.push(MenuItem::Separator);
        let commands = self.commands.clone();
        menu.push(
            StandardItem {
                label: "Quit".into(),
                icon_name: "application-exit-symbolic".into(),
                activate: Box::new(move |_| {
                    let _ = commands.try_send(TrayCommand::Quit);
                }),
                ..Default::default()
            }
            .into(),
        );
        menu
    }
}

/// The worker's tokio handle, handed in once at startup (see `init`): the
/// tray service needs a Tokio reactor, which lives on the worker thread
/// (`worker.rs`) rather than the GLib main context - the same constraint
/// `launcher_entry` documents.
static TOKIO: OnceLock<tokio::runtime::Handle> = OnceLock::new();

/// The live tray service handle, `None` while the tray is disabled. Kept
/// here (rather than in `UiState`) so `set_unread_count` can reach it from
/// anywhere without the window plumbing a handle around.
static TRAY: OnceLock<Mutex<Option<ksni::Handle<LookoutTray>>>> = OnceLock::new();

/// The last count sent to the tray, so consecutive identical updates skip
/// the re-render and the D-Bus round trip.
static LAST_COUNT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(u32::MAX);

/// Hands `worker`'s tokio handle to the tray machinery. Called once at
/// startup, before any window code can start the tray; without it every
/// `start`/`set_unread_count` is a silent no-op (as in the unit tests).
pub fn init(worker: &crate::worker::Worker) {
    let _ = TOKIO.set(worker.handle());
}

/// Starts the tray service and registers it with the session's
/// StatusNotifierWatcher. Returns whether the service came up (a missing
/// watcher just means the icon doesn't appear yet; ksni stays offline and
/// retries). Blocks briefly on the first registration, so callers should
/// be on the UI thread with no locks held.
pub fn start(commands: async_channel::Sender<TrayCommand>) -> bool {
    let Some(tokio) = TOKIO.get() else { return false };
    let tray = LookoutTray {
        count: 0,
        icon: render_tray_icons(0),
        commands,
    };
    let (tx, rx) = std::sync::mpsc::channel();
    tokio.spawn(async move {
        let _ = tx.send(tray.disable_dbus_name(true).spawn().await.ok());
    });
    let Ok(Some(handle)) = rx.recv_timeout(START_TIMEOUT) else {
        return false;
    };
    TRAY.get_or_init(|| Mutex::new(None)).lock().unwrap().replace(handle);
    LAST_COUNT.store(u32::MAX, std::sync::atomic::Ordering::Relaxed);
    true
}

/// Shuts the tray service down (unregistering the item, so the host drops
/// the icon). No-op when the tray was never started.
pub fn stop() {
    let Some(guard) = TRAY.get() else { return };
    let Some(handle) = guard.lock().unwrap().take() else { return };
    if let Some(tokio) = TOKIO.get() {
        tokio.spawn(async move {
            handle.shutdown().await;
        });
    }
}

/// Repaints the tray icon (and tooltip) for `count` - the same summed
/// Inbox unread count the dock badge shows. Called from the UI thread;
/// the icon is rendered here (GTK/cairo aren't thread-safe) and shipped
/// to the tray service via `Handle::update` on the worker's runtime.
/// Dedupes identical consecutive counts.
pub fn set_unread_count(count: u32) {
    if LAST_COUNT.swap(count, std::sync::atomic::Ordering::Relaxed) == count {
        return;
    }
    let (Some(tokio), Some(handle)) = (TOKIO.get(), TRAY.get().and_then(|g| g.lock().unwrap().clone())) else {
        return;
    };
    let icon = render_tray_icons(count);
    tokio.spawn(async move {
        let _ = handle
            .update(|tray| {
                tray.count = count;
                tray.icon = icon;
            })
            .await;
    });
}

/// The icon sizes hosts commonly ask an SNI item for. Rendered upfront at
/// each size rather than scaled by the host, so the badge text stays crisp.
const ICON_SIZES: [i32; 3] = [16, 22, 24];

/// Renders the tray icon pixmaps for `count`, one per [`ICON_SIZES`].
/// Pure cairo - no display needed - so it works headless (the unit tests
/// exercise it); the app's icon comes from the icon theme when it
/// resolves, with a drawn envelope fallback otherwise.
pub fn render_tray_icons(count: u32) -> Vec<Icon> {
    ICON_SIZES.iter().filter_map(|&size| render_tray_icon(size, count)).collect()
}

fn render_tray_icon(size: i32, count: u32) -> Option<Icon> {
    let mut surface = cairo::ImageSurface::create(cairo::Format::ARgb32, size, size).ok()?;
    let cr = cairo::Context::new(&surface).ok()?;
    if !draw_app_icon(&cr, size) {
        draw_envelope(&cr, size);
    }
    if count > 0 {
        draw_count_badge(&cr, size, count);
    }
    // Cairo's ARGB32 memory layout is premultiplied BGRA; the SNI spec
    // wants straight ARGB32 in network byte order. Unpremultiplying keeps
    // the semi-transparent icon edges clean on hosts that blend the bytes
    // naively, and avoids the "ARGB vs RGBA" guessing game entirely by
    // spelling out the byte order. The context must be dropped first: it
    // holds a reference to the surface, and `data()` requires an exclusive
    // (reference-count-1) borrow.
    drop(cr);
    let mut bgra = surface.data().ok()?.to_vec();
    let mut argb = Vec::with_capacity(bgra.len());
    for pixel in bgra.chunks_exact_mut(4) {
        let (b, g, r, a) = (pixel[0], pixel[1], pixel[2], pixel[3]);
        let (r, g, b) = if a == 0 {
            (0, 0, 0)
        } else {
            let f = 255.0 / a as f32;
            ((r as f32 * f) as u8, (g as f32 * f) as u8, (b as f32 * f) as u8)
        };
        argb.extend_from_slice(&[a, r, g, b]);
    }
    Some(Icon {
        width: size,
        height: size,
        data: argb,
    })
}

/// Paints the app icon onto `cr` at `size`; returns whether it resolved.
/// The icon file is found through the standard data paths and rendered by
/// gdk-pixbuf (the SVG loader, librsvg, scales it to `size`); a headless
/// run, a snap without librsvg, or a non-installed checkout falls back to
/// [`draw_envelope`].
fn draw_app_icon(cr: &cairo::Context, size: i32) -> bool {
    let Some(path) = app_icon_path() else { return false };
    let Ok(pixbuf) = gtk::gdk_pixbuf::Pixbuf::from_file_at_scale(&path, size, size, true) else {
        return false;
    };
    let (w, h) = (pixbuf.width(), pixbuf.height());
    if w <= 0 || h <= 0 {
        return false;
    }
    let bytes = pixbuf.read_pixel_bytes();
    let rowstride = pixbuf.rowstride().max(0) as usize;
    // The pixbuf is straight RGBA; cairo's ARGB32 wants the same channels
    // premultiplied in BGRA byte order. `create_for_data` wraps those bytes
    // as a surface so `paint()` composites them exactly like any other
    // source - no manual blending of the semi-transparent edges.
    let mut bgra = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h as usize {
        let row = &bytes.as_ref()[y * rowstride..y * rowstride + w as usize * 4];
        for px in row.chunks_exact(4) {
            let [r, g, b, a] = [px[0], px[1], px[2], px[3]];
            if a == 0 {
                bgra.extend_from_slice(&[0, 0, 0, 0]);
            } else {
                let f = a as f64 / 255.0;
                bgra.extend_from_slice(&[(b as f64 * f) as u8, (g as f64 * f) as u8, (r as f64 * f) as u8, a]);
            }
        }
    }
    let Ok(base) = cairo::ImageSurface::create_for_data(bgra, cairo::Format::ARgb32, w, h, w * 4) else {
        return false;
    };
    let _ = cr.set_source_surface(&base, (size - w) as f64 / 2.0, (size - h) as f64 / 2.0);
    let _ = cr.paint();
    true
}

/// Finds the app's icon file: the user's local share first, then every
/// `XDG_DATA_DIRS` icons directory, then the repo-relative copy for a bare
/// `target/release/lookout` run straight out of a checkout.
fn app_icon_path() -> Option<std::path::PathBuf> {
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(std::path::PathBuf::from(home).join(".local/share/icons/hicolor/scalable/apps/io.github.gavindi.Lookout.svg"));
    }
    let data_dirs = std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    for dir in data_dirs.split(':') {
        if !dir.is_empty() {
            candidates.push(std::path::PathBuf::from(dir).join("icons/hicolor/scalable/apps/io.github.gavindi.Lookout.svg"));
        }
    }
    // A dev checkout run straight from `target/release` (no install).
    candidates.push(std::path::PathBuf::from("data/icons/hicolor/scalable/apps/io.github.gavindi.Lookout.svg"));
    candidates.push(std::path::PathBuf::from("../../../data/icons/hicolor/scalable/apps/io.github.gavindi.Lookout.svg"));
    candidates.into_iter().find(|p| p.is_file())
}

/// The fallback base icon: a rounded square in the app's accent blue with
/// a white envelope, drawn entirely with cairo primitives (usable with no
/// icon theme, no display, no librsvg).
fn draw_envelope(cr: &cairo::Context, size: i32) {
    let s = size as f64;
    rounded_rect(cr, 0.0, 0.0, s, s, s * 0.22);
    // The app's `@lookout-accent` blue, #4d9dff.
    cr.set_source_rgb(0x4d as f64 / 255.0, 0x9d as f64 / 255.0, 1.0);
    let _ = cr.fill();

    let inset = s * 0.22;
    let body_height = (s - 2.0 * inset) * 0.68;
    cr.set_source_rgb(1.0, 1.0, 1.0);
    cr.set_line_width(s * 0.09);
    cr.set_line_cap(cairo::LineCap::Round);
    cr.set_line_join(cairo::LineJoin::Round);
    cr.rectangle(inset, inset, s - 2.0 * inset, body_height);
    let _ = cr.stroke();
    cr.move_to(inset, inset);
    cr.line_to(s / 2.0, s / 2.0 - body_height * 0.12);
    cr.line_to(s - inset, inset);
    let _ = cr.stroke();
}

/// The unread-count badge: a red disc with a white ring and a bold count,
/// anchored at the icon's bottom-right corner. `99+` past 99, so the badge
/// never outgrows a small icon.
fn draw_count_badge(cr: &cairo::Context, size: i32, count: u32) {
    let s = size as f64;
    let radius = s * 0.34;
    let (cx, cy) = (s - radius * 1.08, s - radius * 1.08);
    cr.set_source_rgb(1.0, 1.0, 1.0);
    cr.arc(cx, cy, radius, 0.0, std::f64::consts::TAU);
    let _ = cr.fill();
    // GNOME red, #e01b24.
    cr.set_source_rgb(0.878, 0.106, 0.141);
    cr.arc(cx, cy, radius * 0.84, 0.0, std::f64::consts::TAU);
    let _ = cr.fill();

    let label = if count <= 99 { count.to_string() } else { "99+".into() };
    cr.set_source_rgb(1.0, 1.0, 1.0);
    cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
    // Two digits fit at this size; "99+" needs a bit less room.
    cr.set_font_size(if label.len() > 2 { radius * 0.72 } else { radius * 1.05 });
    let ext = cr.text_extents(&label).ok();
    let ext = ext.as_ref().map(|e| (e.width(), e.height(), e.x_bearing(), e.y_bearing())).unwrap_or((0.0, 0.0, 0.0, 0.0));
    cr.move_to(cx - ext.0 / 2.0 - ext.2, cy - ext.1 / 2.0 - ext.3);
    let _ = cr.show_text(&label);
}

fn rounded_rect(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64, radius: f64) {
    cr.move_to(x + radius, y);
    cr.arc(x + w - radius, y + radius, radius, -std::f64::consts::FRAC_PI_2, 0.0);
    cr.arc(x + w - radius, y + h - radius, radius, 0.0, std::f64::consts::FRAC_PI_2);
    cr.arc(x + radius, y + h - radius, radius, std::f64::consts::FRAC_PI_2, std::f64::consts::PI);
    cr.arc(x + radius, y + radius, radius, std::f64::consts::PI, std::f64::consts::PI * 1.5);
    cr.close_path();
}

#[cfg(test)]
mod tests {
    use super::*;
    use ksni::Tray;

    #[test]
    fn renders_icons_at_the_standard_sizes() {
        let icons = render_tray_icons(0);
        assert_eq!(icons.len(), ICON_SIZES.len());
        for (icon, size) in icons.iter().zip(ICON_SIZES) {
            assert_eq!(icon.width, size);
            assert_eq!(icon.height, size);
            assert_eq!(icon.data.len(), (size * size * 4) as usize);
        }
    }

    #[test]
    fn count_badge_turns_pixels_red_at_the_corner() {
        let quiet = render_tray_icons(0).into_iter().find(|i| i.width == 22).unwrap();
        let counting = render_tray_icons(3).into_iter().find(|i| i.width == 22).unwrap();
        assert_ne!(quiet.data, counting.data, "the badge must change the pixmap");
        let red_pixels = |icon: &Icon| {
            icon.data
                .chunks_exact(4)
                // A badge pixel: alpha opaque, red channel dominant.
                .filter(|px| px[0] > 200 && px[1] > 180 && px[2] < 90 && px[3] < 90)
                .count()
        };
        assert_eq!(red_pixels(&quiet), 0, "no badge when the count is zero");
        assert!(red_pixels(&counting) > 0, "the badge disc is red");
    }

    #[test]
    fn menu_carries_open_compose_and_quit() {
        let (commands, _rx) = async_channel::unbounded();
        let tray = LookoutTray {
            count: 0,
            icon: Vec::new(),
            commands,
        };
        let labels: Vec<String> = tray
            .menu()
            .iter()
            .filter_map(|item| match item {
                MenuItem::Standard(item) => Some(item.label.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(labels, ["Open Lookout", "Compose…", "Quit"]);
    }

    #[test]
    fn tooltip_names_the_unread_count() {
        let (commands, _rx) = async_channel::unbounded();
        let tray = LookoutTray {
            count: 14,
            icon: Vec::new(),
            commands,
        };
        assert_eq!(tray.tool_tip().description, "14 unread messages");
        let (commands, _rx) = async_channel::unbounded();
        let tray = LookoutTray {
            count: 1,
            icon: Vec::new(),
            commands,
        };
        assert_eq!(tray.tool_tip().description, "1 unread message");
    }
}
