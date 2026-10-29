//! Persistent per-calendar colours for the "My calendars" checklist.
//!
//! The user-facing ask is that every calendar get a colour and the colour is
//! remembered between sessions, so this module owns both halves of that: a
//! plain JSON file (`$XDG_CONFIG_HOME/lookout/calendar-colors.json`) mapping
//! `CalendarId` -> hex colour, and the assignment rules for calendars that
//! don't have one yet. A CalDAV server can advertise a colour itself (the
//! `calendar-color` extension property on `CalendarInfo.color`); when it does,
//! that wins. Otherwise the calendar gets the next colour from
//! [`CALENDAR_PALETTE`], picked deterministically from the sorted list of
//! discovered calendars so a given set of calendars always maps to the same
//! colours. Once assigned, an entry is never re-coloured automatically - the
//! stored value is treated as the user's choice.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use lookout_core::{CalendarId, CalendarInfo};

use crate::app_config::config_dir;

pub type CalendarColorMap = HashMap<CalendarId, String>;

/// Distinct, colourblind-friendly-ish accent colours (blue, red, green,
/// orange, purple, ...) assigned round-robin to calendars whose server
/// doesn't advertise one.
pub const CALENDAR_PALETTE: [&str; 10] = [
    "#3584e4", // blue
    "#f66151", // red
    "#33d17a", // green
    "#ff9f0a", // orange
    "#c061cb", // purple
    "#1a5fb4", // dark blue
    "#e5a50a", // gold
    "#9141ac", // magenta
    "#e01b24", // crimson
    "#56b9c4", // teal
];

/// The colour shown while a calendar is checked but hasn't been assigned one
/// yet - a neutral grey, kept in sync with the app's dim-text tone.
pub const DEFAULT_CHECK_COLOR: &str = "#9a9996";

/// `$XDG_CONFIG_HOME/lookout/calendar-colors.json` (or the equivalent
/// `~/.config` path when `XDG_CONFIG_HOME` is unset).
pub fn colors_path() -> PathBuf {
    config_dir().join("lookout").join("calendar-colors.json")
}

/// Backups created by [`load_at`] for a file it couldn't parse, drained by the
/// window so the user hears about the preserved copy.
static CORRUPT_BACKUPS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// Takes the backup paths recorded since the last call.
pub(crate) fn take_corrupt_backups() -> Vec<PathBuf> {
    CORRUPT_BACKUPS.lock().map(|mut backups| std::mem::take(&mut *backups)).unwrap_or_default()
}

/// Loads the saved `CalendarId` -> colour map. A missing file yields an empty
/// map; an unreadable one is moved aside first, so the next save never
/// overwrites the only copy.
pub fn load() -> CalendarColorMap {
    load_at(&colors_path())
}

fn load_at(path: &std::path::Path) -> CalendarColorMap {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str(&text) {
            Ok(map) => map,
            Err(e) => {
                tracing::warn!(path = %path.display(), "unreadable calendar-colors.json: {e}");
                match crate::fsutil::backup_corrupt(path) {
                    Ok(backup) => {
                        tracing::warn!(path = %path.display(), backup = %backup.display(), "kept a copy of the unreadable calendar-colors.json");
                        if let Ok(mut backups) = CORRUPT_BACKUPS.lock() {
                            backups.push(backup);
                        }
                    }
                    Err(e) => tracing::error!(path = %path.display(), "could not back up the unreadable calendar-colors.json: {e}"),
                }
                CalendarColorMap::new()
            }
        },
        Err(_) => CalendarColorMap::new(),
    }
}

/// Writes the map out atomically (creating the directory as needed).
/// Best-effort: a read-only home or disk error only logs a warning.
pub fn save(map: &CalendarColorMap) {
    save_at(&colors_path(), map);
}

fn save_at(path: &std::path::Path, map: &CalendarColorMap) {
    match serde_json::to_string_pretty(map) {
        Ok(text) => {
            if let Err(e) = crate::fsutil::write_atomic(path, text.as_bytes(), 0o600) {
                tracing::warn!(path = %path.display(), "could not save calendar colours: {e}");
            }
        }
        Err(e) => tracing::warn!("could not serialize calendar colours: {e}"),
    }
}

/// Resolves the colour to use for one calendar: the server's advertised hex
/// colour when it's well-formed, otherwise `fallback` (a palette colour).
pub fn resolve_color(server_color: Option<&str>, fallback: &str) -> String {
    match server_color.filter(|c| is_hex_color(c)) {
        Some(c) => c.to_string(),
        None => fallback.to_string(),
    }
}

/// `#rgb`/`#rrggbb`/`#rrggbbaa` with only hex digits.
fn is_hex_color(c: &str) -> bool {
    let body = c.strip_prefix('#');
    matches!(body, Some(b) if (b.len() == 3 || b.len() == 6 || b.len() == 8) && b.chars().all(|ch| ch.is_ascii_hexdigit()))
}

/// Formats a picked colour's 0.0-1.0 RGB components (as a `gtk::gdk::RGBA`
/// carries them, `f32`) as the `#rrggbb` string the map stores. Components are
/// clamped, so a slightly out-of-range value from a colour dialog can't
/// produce a malformed colour.
pub fn rgba_to_hex(r: f32, g: f32, b: f32) -> String {
    let byte = |c: f32| (f64::from(c).clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{:02x}{:02x}{:02x}", byte(r), byte(g), byte(b))
}

/// Parses a `#rgb`/`#rrggbb`/`#rrggbbaa` hex colour to an `(r, g, b)` tuple in
/// 0..=1 for Cairo, falling back to a neutral grey on anything unparseable.
/// Callers that paint every frame parse once when their data is set.
pub fn rgb(color: &str) -> (f64, f64, f64) {
    let body = color.trim_start_matches('#');
    let expanded = if body.len() == 3 && body.chars().all(|c| c.is_ascii_hexdigit()) {
        body.chars().flat_map(|c| [c, c]).collect::<String>()
    } else {
        body.to_string()
    };
    let rgba = gtk::gdk::RGBA::parse(format!("#{expanded}")).unwrap_or_else(|_| gtk::gdk::RGBA::new(0.6, 0.6, 0.6, 1.0));
    (f64::from(rgba.red()), f64::from(rgba.green()), f64::from(rgba.blue()))
}

/// The colour a calendar falls back to when the user asks for "Automatic
/// colour" (item 3.13 / CAL-32): its server-advertised colour when there is
/// one, else the palette colour at its position in `index` (wrapping), the
/// same rule [`assign_missing`] applies on first sight. Kept pure so the
/// window's action handler and its tests agree on the result.
pub fn automatic_color(server_color: Option<&str>, index: usize) -> String {
    resolve_color(server_color, CALENDAR_PALETTE[index % CALENDAR_PALETTE.len()])
}

/// Assigns a colour to every calendar in `calendars` that doesn't have one in
/// `map` yet, then persists if anything changed. Assignment is deterministic:
/// calendars are considered in sorted-id order, each missing one either taking
/// its server-advertised colour or the palette colour at its sorted position.
/// Existing entries are left untouched, so a colour a user has seen (or a
/// server has set) survives repeated refreshes.
pub fn assign_missing(map: &mut CalendarColorMap, calendars: &[CalendarInfo]) {
    let mut sorted: Vec<&CalendarInfo> = calendars.iter().collect();
    sorted.sort_by_key(|c| c.id.0.clone());
    let mut added = false;
    for (index, calendar) in sorted.iter().enumerate() {
        if map.contains_key(&calendar.id) {
            continue;
        }
        let fallback = CALENDAR_PALETTE[index % CALENDAR_PALETTE.len()];
        let color = resolve_color(calendar.color.as_deref(), fallback);
        map.insert(calendar.id.clone(), color);
        added = true;
    }
    if added {
        save(map);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lookout_core::AccountId;

    fn info(id: &str, color: Option<&str>) -> CalendarInfo {
        let account_id = AccountId("account".to_string());
        CalendarInfo {
            id: CalendarId(id.to_string()),
            account_id,
            display_name: id.to_string(),
            color: color.map(|c| c.to_string()),
            href: format!("/{id}"),
            supports_tasks: false,
            owner: None,
        }
    }

    #[test]
    fn resolve_color_prefers_valid_server_color() {
        assert_eq!(resolve_color(Some("#3584e4"), "#ff0000"), "#3584e4");
        assert_eq!(resolve_color(Some("#ABCDEF"), "#ff0000"), "#ABCDEF");
        assert_eq!(resolve_color(Some("#11223344"), "#ff0000"), "#11223344");
    }

    #[test]
    fn resolve_color_rejects_malformed_server_color() {
        assert_eq!(resolve_color(Some("3584e4"), "#ff0000"), "#ff0000");
        assert_eq!(resolve_color(Some("#xyzzy"), "#ff0000"), "#ff0000");
        assert_eq!(resolve_color(Some("#12"), "#ff0000"), "#ff0000");
        assert_eq!(resolve_color(None, "#ff0000"), "#ff0000");
    }

    #[test]
    fn assign_missing_prefers_server_color_then_palette() {
        let mut map = CalendarColorMap::new();
        let calendars = [info("b", Some("#00ff00")), info("a", None)];
        assign_missing(&mut map, &calendars);
        assert_eq!(map.get(&CalendarId("b".to_string())).map(String::as_str), Some("#00ff00"));
        // "a" sorts before "b", so it takes palette[0] (blue).
        assert_eq!(map.get(&CalendarId("a".to_string())).map(String::as_str), Some(CALENDAR_PALETTE[0]));
    }

    #[test]
    fn assign_missing_is_idempotent() {
        let mut map = CalendarColorMap::new();
        let calendars = [info("a", None), info("b", None)];
        assign_missing(&mut map, &calendars);
        let before = map.clone();
        assign_missing(&mut map, &calendars);
        assert_eq!(map, before);
    }

    #[test]
    fn palette_wraps_round_robin() {
        let mut map = CalendarColorMap::new();
        let calendars: Vec<CalendarInfo> = (0..(CALENDAR_PALETTE.len() + 1)).map(|i| info(&format!("c{i:02}"), None)).collect();
        assign_missing(&mut map, &calendars);
        assert_eq!(map.get(&CalendarId("c10".to_string())).map(String::as_str), Some(CALENDAR_PALETTE[0]));
    }

    #[test]
    fn rgba_to_hex_formats_and_clamps() {
        // The colour dialog hands components back as 0.0-1.0 floats; the map
        // stores hex, so the conversion must produce the `#rrggbb` string the
        // CSS class names are built from.
        assert_eq!(rgba_to_hex(1.0, 1.0, 1.0), "#ffffff");
        assert_eq!(rgba_to_hex(0.0, 0.0, 0.0), "#000000");
        assert_eq!(rgba_to_hex(0.2, 0.5, 0.25), "#338040");
        // Out-of-range components clamp rather than producing a malformed hex.
        assert_eq!(rgba_to_hex(-0.5, 1.5, 0.0), "#00ff00");
    }

    #[test]
    fn automatic_color_prefers_the_server_then_the_palette_position() {
        assert_eq!(automatic_color(Some("#00ff00"), 3), "#00ff00");
        assert_eq!(automatic_color(None, 1), CALENDAR_PALETTE[1]);
        // The palette wraps for an index past its end (a long calendar list).
        assert_eq!(automatic_color(None, CALENDAR_PALETTE.len()), CALENDAR_PALETTE[0]);
    }

    #[test]
    fn an_unreadable_colour_file_is_preserved_before_defaults_take_over() {
        let dir = std::env::temp_dir().join(format!("lookout-colors-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("calendar-colors.json");
        std::fs::write(&path, "not json").unwrap();

        assert!(load_at(&path).is_empty());
        assert!(!path.exists(), "the unparseable file is moved out of the way");
        let backup = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .find(|path| path.file_name().is_some_and(|name| name.to_string_lossy().contains(".corrupt-")))
            .expect("a corrupt copy was kept");
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), "not json");

        // A good file still round-trips through the atomic writer.
        let mut map = CalendarColorMap::new();
        map.insert(CalendarId("cal".into()), "#3584e4".into());
        save_at(&path, &map);
        assert_eq!(load_at(&path), map);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rgb_parses_hex_forms_and_falls_back_to_grey() {
        let close = |(r, g, b): (f64, f64, f64), (er, eg, eb): (f64, f64, f64)| (r - er).abs() < 1e-3 && (g - eg).abs() < 1e-3 && (b - eb).abs() < 1e-3;
        assert!(close(rgb("#fff"), (1.0, 1.0, 1.0)));
        assert!(close(rgb("#3584e4"), (0x35 as f64 / 255.0, 0x84 as f64 / 255.0, 0xe4 as f64 / 255.0)));
        assert!(close(rgb("#ff000080"), (1.0, 0.0, 0.0)));
        assert!(close(rgb("not a colour"), (0.6, 0.6, 0.6)));
        assert!(close(rgb(""), (0.6, 0.6, 0.6)));
    }
}
