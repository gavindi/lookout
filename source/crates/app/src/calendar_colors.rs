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

use std::collections::HashMap;
use std::path::PathBuf;

use lookout_core::{CalendarId, CalendarInfo};

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

fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| {
        let home = std::env::var_os("HOME").unwrap_or_else(|| std::env::var_os("USERPROFILE").unwrap_or_default());
        PathBuf::from(home).join(".config")
    })
}

/// `$XDG_CONFIG_HOME/lookout/calendar-colors.json` (or the equivalent
/// `~/.config` path when `XDG_CONFIG_HOME` is unset).
pub fn colors_path() -> PathBuf {
    config_dir().join("lookout").join("calendar-colors.json")
}

/// Loads the saved `CalendarId` -> colour map. Any failure (missing file, bad
/// JSON, unreadable dir) yields an empty map - colours are cosmetic, so a
/// broken file should never take the app down.
pub fn load() -> CalendarColorMap {
    match std::fs::read_to_string(colors_path()) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
            tracing::warn!(path = %colors_path().display(), "ignoring unreadable calendar-colors.json: {e}");
            CalendarColorMap::new()
        }),
        Err(_) => CalendarColorMap::new(),
    }
}

/// Writes the map out (creating the directory as needed). Best-effort: a
/// read-only home or disk error only logs a warning.
pub fn save(map: &CalendarColorMap) {
    let path = colors_path();
    if let Some(dir) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            tracing::warn!(path = %path.display(), "could not create config dir: {e}");
            return;
        }
    }
    match serde_json::to_string_pretty(map) {
        Ok(text) => {
            if let Err(e) = std::fs::write(&path, text) {
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

/// Parses a `#rgb`/`#rrggbb`/`#rrggbbaa` colour to `(r, g, b)` (alpha ignored).
fn parse_hex_rgb(c: &str) -> Option<(u8, u8, u8)> {
    let body = c.strip_prefix('#')?;
    let body = match body.len() {
        3 => return parse_expanded_hex(body),
        6 => body,
        8 => &body[..6],
        _ => return None,
    };
    if body.chars().any(|ch| !ch.is_ascii_hexdigit()) {
        return None;
    }
    Some((
        u8::from_str_radix(&body[0..2], 16).ok()?,
        u8::from_str_radix(&body[2..4], 16).ok()?,
        u8::from_str_radix(&body[4..6], 16).ok()?,
    ))
}

/// `#abc` expands to `#aabbcc`.
fn parse_expanded_hex(body: &str) -> Option<(u8, u8, u8)> {
    if body.chars().any(|ch| !ch.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |ch: char| u8::from_str_radix(&ch.to_string().repeat(2), 16).ok();
    let mut chars = body.chars();
    Some((byte(chars.next()?)?, byte(chars.next()?)?, byte(chars.next()?)?))
}

/// A foreground colour that stays readable against `background` as an event
/// chip: `"white"` for dark backgrounds, `"black"` for light ones. Uses the
/// standard sRGB-relative luminance, so bright calendar colours (yellows,
/// light greens) still get legible text.
pub fn readable_foreground(background: &str) -> &'static str {
    let Some((r, g, b)) = parse_hex_rgb(background) else { return "white" };
    let lin = |c: u8| {
        let c = c as f64 / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    let luminance = 0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b);
    if luminance > 0.4 {
        "black"
    } else {
        "white"
    }
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
            id: CalendarId(format!("{id}")),
            account_id,
            display_name: id.to_string(),
            color: color.map(|c| c.to_string()),
            href: format!("/{id}"),
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
    fn readable_foreground_contrasts_with_background() {
        assert_eq!(readable_foreground("#ffffff"), "black");
        assert_eq!(readable_foreground("#000000"), "white");
        assert_eq!(readable_foreground("#3584e4"), "white");
        assert_eq!(readable_foreground("#e5a50a"), "black");
        assert_eq!(readable_foreground("#abc"), "black");
        assert_eq!(readable_foreground("not-a-color"), "white");
    }
}
