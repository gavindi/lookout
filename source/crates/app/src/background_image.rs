//! Persistence for the user's window-background image choice.
//!
//! The main window normally shows bundled artwork (`background2.jpg`); this
//! module stores an optional path to an image the user picked under
//! Config → Appearance → "Window background". The path lives in the GSettings
//! `window-background-path` key (see `settings.rs`): a missing or stale entry
//! just means the window falls back to the bundled artwork, never an error.
//! `window.rs` owns display; this module remembers the choice and decodes the
//! image ([`decode_for_display`]) no larger than the screen needs.
//! Runs that predate GSettings left the path in a plain file; the first load
//! after the migration imports it once and drops the file.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::path::{Path, PathBuf};

use gtk::glib;
use gtk::prelude::*;

use crate::app_config::config_dir;
use crate::settings::WINDOW_BACKGROUND_PATH;

/// The decode target when GDK reports no monitor at all.
const FALLBACK_DISPLAY_TARGET: (i32, i32) = (2560, 1600);

/// The size, in device pixels, a window background has to cover: the largest
/// connected monitor (logical size times its scale), or
/// [`FALLBACK_DISPLAY_TARGET`] when none is reported. A window can't be
/// larger than its monitor, so a background decoded to cover this is never
/// upscaled on screen.
pub fn display_target() -> (i32, i32) {
    let Some(display) = gtk::gdk::Display::default() else {
        return FALLBACK_DISPLAY_TARGET;
    };
    let monitors = display.monitors();
    let mut target: Option<(i32, i32)> = None;
    for index in 0..monitors.n_items() {
        let Some(monitor) = monitors.item(index).and_downcast::<gtk::gdk::Monitor>() else {
            continue;
        };
        let geometry = monitor.geometry();
        let scale = monitor.scale().max(1.0);
        let size = ((f64::from(geometry.width()) * scale).ceil() as i32, (f64::from(geometry.height()) * scale).ceil() as i32);
        target = Some(match target {
            Some((w, h)) => (w.max(size.0), h.max(size.1)),
            None => size,
        });
    }
    target.filter(|(w, h)| *w > 0 && *h > 0).unwrap_or(FALLBACK_DISPLAY_TARGET)
}

/// The size an image of `width` x `height` decodes to so that it still covers
/// `target` the way `gtk::ContentFit::Cover` draws it: scaled down to the
/// smallest size that fills both sides, never scaled up.
pub fn cover_size(width: i32, height: i32, target: (i32, i32)) -> (i32, i32) {
    if width <= 0 || height <= 0 {
        return (width, height);
    }
    let scale = (f64::from(target.0) / f64::from(width)).max(f64::from(target.1) / f64::from(height));
    if scale >= 1.0 {
        return (width, height);
    }
    (((f64::from(width) * scale).round() as i32).max(1), ((f64::from(height) * scale).round() as i32).max(1))
}

/// Decodes a background image no larger than it takes to cover `target`
/// (see [`cover_size`]), instead of at its full size: the bundled 4000 x 3250
/// artwork alone took 37 MB decoded (MEMORY_OPTIMISATION1.md, "Per-account
/// breakdown"). A format gdk-pixbuf has no loader for falls back to a
/// full-size decode, as the background was decoded before; the error is that
/// decode's when neither can read it.
pub fn decode_for_display(bytes: &glib::Bytes, target: (i32, i32)) -> Result<gtk::gdk::Texture, glib::Error> {
    let loader = gtk::gdk_pixbuf::PixbufLoader::new();
    loader.connect_size_prepared(move |loader, width, height| {
        let (w, h) = cover_size(width, height, target);
        if (w, h) != (width, height) {
            loader.set_size(w, h);
        }
    });
    let decoded = loader.write(bytes).and_then(|()| loader.close()).ok().and_then(|()| loader.pixbuf());
    match decoded {
        Some(pixbuf) => Ok(crate::window::pixbuf_to_texture(&pixbuf)),
        None => {
            let _ = loader.close();
            gtk::gdk::Texture::from_bytes(bytes)
        }
    }
}

/// Reads and decodes the image at `path` with [`decode_for_display`].
pub fn decode_file_for_display(path: &Path, target: (i32, i32)) -> Result<gtk::gdk::Texture, glib::Error> {
    let (bytes, _etag) = gtk::gio::File::for_path(path).load_bytes(gtk::gio::Cancellable::NONE)?;
    decode_for_display(&bytes, target)
}

/// The pre-GSettings `background-image-path` file, consulted once by the
/// migration in `load` and then deleted.
fn legacy_path() -> PathBuf {
    config_dir().join("lookout").join("background-image-path")
}

/// The stored custom background path, if any, and only when the file it points
/// at still exists. A missing file (the image was deleted or the disk was
/// swapped) is treated the same as never having chosen one.
pub fn load(store: &crate::settings::SettingsStore) -> Option<PathBuf> {
    let stored = PathBuf::from(store.get_string(WINDOW_BACKGROUND_PATH));
    stored.is_file().then_some(stored)
}

/// Remembers `path` as the custom background. Best-effort: a read-only home or
/// disk error only logs a warning, matching `calendar_colors::save`.
pub fn save(store: &crate::settings::SettingsStore, p: &Path) {
    store.set_string(WINDOW_BACKGROUND_PATH, &p.to_string_lossy());
}

/// Drops the stored custom background, reverting the window to the bundled
/// artwork on the next launch. Missing file is fine.
pub fn clear(store: &crate::settings::SettingsStore) {
    store.set_string(WINDOW_BACKGROUND_PATH, "");
}

/// Runs the one-time import of the pre-GSettings `background-image-path`
/// file. Called once from `build_window` before the first `load`, so a
/// machine upgrading from a plain-file build keeps its custom background.
/// The import only happens while the key still holds its default - a session
/// that already picked an image keeps it - and the legacy file is dropped
/// whether or not it could be imported.
pub fn migrate_legacy(store: &crate::settings::SettingsStore) {
    migrate_legacy_file_at(&legacy_path(), store);
}

fn migrate_legacy_file_at(path: &Path, store: &crate::settings::SettingsStore) {
    if !path.exists() {
        return;
    }
    let imported = (|| -> Option<()> {
        if !store.get_string(WINDOW_BACKGROUND_PATH).is_empty() {
            return None;
        }
        let text = std::fs::read_to_string(path).ok()?;
        let stored = PathBuf::from(text.trim());
        if stored.is_file() {
            store.set_string(WINDOW_BACKGROUND_PATH, &stored.to_string_lossy());
            Some(())
        } else {
            None
        }
    })();
    if imported.is_none() {
        tracing::debug!(path = %path.display(), "ignored legacy background-image-path (already migrated or stale)");
    }
    let _ = std::fs::remove_file(path);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cover_size_scales_down_to_cover_and_never_up() {
        // The bundled artwork on a 1080p screen: the width is the tighter side.
        assert_eq!(cover_size(4000, 3250, (1920, 1080)), (1920, 1560));
        // A tall target makes the height the tighter side.
        assert_eq!(cover_size(4000, 3250, (1080, 1920)), (2363, 1920));
        // Already smaller than the target: kept as is.
        assert_eq!(cover_size(640, 480, (1920, 1080)), (640, 480));
        assert_eq!(cover_size(0, 0, (1920, 1080)), (0, 0));
    }

    /// The bundled background is decoded to cover the target rather than at
    /// its full 4000 x 3250, a small image is not upscaled, and bytes that
    /// aren't an image are an error.
    #[test]
    fn background_is_decoded_to_cover_the_display_target() {
        if !crate::gtk_test::gtk_ready() {
            return;
        }
        let bundled = glib::Bytes::from_static(include_bytes!("../../../data/resources/backgrounds/background2.jpg"));
        let texture = decode_for_display(&bundled, (1920, 1080)).expect("the bundled background decodes");
        assert_eq!((texture.width(), texture.height()), (1920, 1560));

        let pixbuf = gtk::gdk_pixbuf::Pixbuf::new(gtk::gdk_pixbuf::Colorspace::Rgb, true, 8, 640, 480).expect("pixbuf");
        pixbuf.fill(0x3584e4ff);
        let png = pixbuf.save_to_bufferv("png", &[]).expect("png");
        let texture = decode_for_display(&glib::Bytes::from_owned(png), (1920, 1080)).expect("png decodes");
        assert_eq!((texture.width(), texture.height()), (640, 480), "not upscaled");

        assert!(decode_for_display(&glib::Bytes::from_static(b"not an image"), (1920, 1080)).is_err());
    }

    #[test]
    fn save_load_clear_round_trip() {
        let store = crate::settings::resolve();
        // Distinct root from the other test in this module: both use the
        // process id, and each test cleans its own tree up.
        let chosen = std::env::temp_dir().join(format!("lookout-bg-test-{}-round-trip", std::process::id()));
        let chosen = chosen.join("me.jpg");
        std::fs::create_dir_all(chosen.parent().unwrap()).unwrap();
        std::fs::write(&chosen, b"jpeg").unwrap();

        assert_eq!(load(&store), None);
        save(&store, &chosen);
        assert_eq!(load(&store), Some(chosen.clone()));
        clear(&store);
        assert_eq!(load(&store), None);

        // A stored path whose file has since disappeared (deleted, disk
        // swapped) reads back as no custom background at all.
        let ghost = chosen.parent().unwrap().join("deleted.png");
        save(&store, &ghost);
        assert_eq!(load(&store), None);

        let _ = std::fs::remove_dir_all(chosen.parent().unwrap());
    }

    #[test]
    fn legacy_file_is_imported_once() {
        // Distinct root from the other test in this module (same process id).
        let dir = std::env::temp_dir().join(format!("lookout-bg-test-{}-legacy", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let legacy = dir.join("background-image-path");
        let image = dir.join("picked.png");
        std::fs::write(&image, b"png").unwrap();
        let store = crate::settings::resolve();

        std::fs::write(&legacy, image.to_str().unwrap()).unwrap();
        migrate_legacy_file_at(&legacy, &store);
        assert_eq!(load(&store), Some(image.clone()));
        assert!(!legacy.exists());

        // A stale file is dropped without clobbering a newer choice.
        std::fs::write(&legacy, image.to_str().unwrap()).unwrap();
        let other = dir.join("other.png");
        std::fs::write(&other, b"png").unwrap();
        save(&store, &other);
        migrate_legacy_file_at(&legacy, &store);
        assert_eq!(load(&store), Some(other));
        assert!(!legacy.exists());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
