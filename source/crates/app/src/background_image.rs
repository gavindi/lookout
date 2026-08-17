//! Persistence for the user's window-background image choice.
//!
//! The main window normally shows bundled artwork (`background2.jpg`); this
//! module stores an optional path to an image the user picked under
//! Config → Appearance → "Window background". The path lives in the GSettings
//! `window-background-path` key (see `settings.rs`): a missing or stale entry
//! just means the window falls back to the bundled artwork, never an error.
//! `window.rs` owns decoding/display; this module only remembers the choice.
//! Runs that predate GSettings left the path in a plain file; the first load
//! after the migration imports it once and drops the file.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::path::{Path, PathBuf};

use crate::settings::WINDOW_BACKGROUND_PATH;

fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| {
        let home = std::env::var_os("HOME").unwrap_or_else(|| std::env::var_os("USERPROFILE").unwrap_or_default());
        PathBuf::from(home).join(".config")
    })
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
