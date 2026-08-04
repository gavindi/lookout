//! Persistence for the user's window-background image choice.
//!
//! The main window normally shows bundled artwork (`background2.png`); this
//! module stores an optional path to an image the user picked under
//! Config → Appearance → "Window background". The path lives in a plain file
//! (`$XDG_CONFIG_HOME/lookout/background-image-path`), the same
//! best-effort-JSON-file convention as calendar colours: a missing, unreadable
//! or stale entry just means the window falls back to the bundled artwork,
//! never an error. `window.rs` owns decoding/display; this module only
//! remembers the choice.

use std::path::{Path, PathBuf};

fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| {
        let home = std::env::var_os("HOME").unwrap_or_else(|| std::env::var_os("USERPROFILE").unwrap_or_default());
        PathBuf::from(home).join(".config")
    })
}

/// `$XDG_CONFIG_HOME/lookout/background-image-path` (or the equivalent
/// `~/.config` path when `XDG_CONFIG_HOME` is unset).
pub fn path() -> PathBuf {
    config_dir().join("lookout").join("background-image-path")
}

/// The stored custom background path, if any, and only when the file it points
/// at still exists. A missing file (the image was deleted or the disk was
/// swapped) is treated the same as never having chosen one.
pub fn load() -> Option<PathBuf> {
    let text = std::fs::read_to_string(path()).ok()?;
    let stored = PathBuf::from(text.trim());
    stored.is_file().then_some(stored)
}

/// Remembers `path` as the custom background. Best-effort: a read-only home or
/// disk error only logs a warning, matching `calendar_colors::save`.
pub fn save(p: &Path) {
    let file = path();
    if let Some(dir) = file.parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            tracing::warn!(path = %file.display(), "could not create config dir: {e}");
            return;
        }
    }
    if let Err(e) = std::fs::write(&file, p.to_string_lossy().as_bytes()) {
        tracing::warn!(path = %file.display(), "could not save background image path: {e}");
    }
}

/// Drops the stored custom background, reverting the window to the bundled
/// artwork on the next launch. Missing file is fine.
pub fn clear() {
    let _ = std::fs::remove_file(path());
}

#[cfg(test)]
mod tests {
    use super::*;

    // A single test owns `XDG_CONFIG_HOME`: the env var is process-global and
    // parallel test threads would race over it otherwise.
    #[test]
    fn save_load_clear_round_trip() {
        let dir = std::env::temp_dir().join(format!("lookout-bg-test-{}", std::process::id()));
        let dir = dir.join("round-trip");
        std::env::set_var("XDG_CONFIG_HOME", &dir);
        let chosen = dir.join("me.jpg");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&chosen, b"jpeg").unwrap();

        assert_eq!(load(), None);
        save(&chosen);
        assert_eq!(load(), Some(chosen.clone()));
        clear();
        assert_eq!(load(), None);

        // A stored path whose file has since disappeared (deleted, disk
        // swapped) reads back as no custom background at all.
        let ghost = dir.join("deleted.png");
        save(&ghost);
        assert_eq!(load(), None);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
