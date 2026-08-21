//! Global keyboard shortcuts, matched by physical keycode.
//!
//! The binding between a shortcut and a key is *physical*: a `Ctrl+R` chord
//! fires on the physical R key regardless of the active keyboard layout (on
//! a Dvorak layout the same position types `o`, so a keyval-based binding
//! would silently break). Configuration stays *logical*, though - the
//! built-in defaults and the GSettings `shortcuts` overrides are
//! accelerator strings (`<Primary>n`), resolved to hardware keycodes
//! against the running keymap at startup via `gdk::Display::map_keyval`,
//! and a captured chord is translated back the other way with
//! `gdk::Display::translate_key`.
//!
//! The window owns a single `EventControllerKey` and asks
//! [`ShortcutState::action_for`] whether a pressed key belongs to a
//! shortcut; the config screen's Keyboard group edits the same
//! `ShortcutState` (capture, conflict checks, reset), which writes through
//! to GSettings so bindings survive restarts. The parse/serialize helpers
//! are pure and unit-tested; everything keymap-dependent is resolved lazily
//! on the UI thread where the display exists.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::collections::HashMap;

use gtk::gdk;
use gtk::gdk::prelude::DisplayExtManual;

use crate::settings::{self, SettingsStore};

const CONTROL: gdk::ModifierType = gdk::ModifierType::CONTROL_MASK;
const SHIFT: gdk::ModifierType = gdk::ModifierType::SHIFT_MASK;
const ALT: gdk::ModifierType = gdk::ModifierType::ALT_MASK;
const SUPER: gdk::ModifierType = gdk::ModifierType::SUPER_MASK;

/// Builds a modifier mask at const-eval time: bitflags' `|` operator isn't
/// `const`, so combined masks in the defaults table go through this.
const fn mods(parts: &[gdk::ModifierType]) -> gdk::ModifierType {
    let mut bits: u32 = 0;
    let mut i = 0;
    while i < parts.len() {
        bits |= parts[i].bits();
        i += 1;
    }
    gdk::ModifierType::from_bits_retain(bits)
}

/// The modifier bits global shortcuts may use; lock-key and mouse-button
/// bits never participate in a stored chord.
pub const MODIFIER_MASK: gdk::ModifierType = mods(&[CONTROL, SHIFT, ALT, SUPER]);

pub const ACTION_COMPOSE: &str = "compose";
pub const ACTION_REPLY: &str = "reply";
pub const ACTION_REPLY_ALL: &str = "reply-all";
pub const ACTION_FORWARD: &str = "forward";
pub const ACTION_DELETE: &str = "delete";
pub const ACTION_ARCHIVE: &str = "archive";
pub const ACTION_REPORT_JUNK: &str = "report-junk";
pub const ACTION_SNOOZE: &str = "snooze";
/// Pin/Unpin's action id. Named `ACTION_PIN` in code (the app calls this
/// feature "Pin" throughout), but the string value is frozen at `"flag"` -
/// `ShortcutState::persist`/`load` write/match this literal verbatim into
/// GSettings when a user customizes the shortcut (`"flag=<accel>"` entries),
/// so changing the value would silently drop any existing user's
/// customization on upgrade. Only the human-visible `title` below changed.
pub const ACTION_PIN: &str = "flag";
pub const ACTION_MARK_READ: &str = "mark-read";
pub const ACTION_PRINT: &str = "print";
pub const ACTION_SEARCH: &str = "search";
pub const ACTION_CLOSE_PANE: &str = "close-pane";
pub const ACTION_MAIL: &str = "mail";
pub const ACTION_CALENDAR: &str = "calendar";
pub const ACTION_CONTACTS: &str = "contacts";
pub const ACTION_TASKS: &str = "tasks";
pub const ACTION_LOOKOUT: &str = "lookout";
pub const ACTION_CONFIG: &str = "config";

/// One built-in shortcut: an action, its Config-screen title, and its
/// default *logical* chord (a keyval + modifier mask).
pub struct DefaultShortcut {
    pub action: &'static str,
    /// Human name shown in Config → Keyboard shortcuts.
    pub title: &'static str,
    pub keyval: gdk::Key,
    pub modifiers: gdk::ModifierType,
}

/// The shipped bindings. Every chord is configurable from Config →
/// Keyboard shortcuts; the defaults mirror Outlook/Gmail conventions where
/// the app's own Ctrl+F search focus doesn't conflict.
pub const DEFAULT_SHORTCUTS: &[DefaultShortcut] = &[
    DefaultShortcut {
        action: ACTION_COMPOSE,
        title: "New message",
        keyval: gdk::Key::n,
        modifiers: CONTROL,
    },
    DefaultShortcut {
        action: ACTION_REPLY,
        title: "Reply",
        keyval: gdk::Key::r,
        modifiers: CONTROL,
    },
    DefaultShortcut {
        action: ACTION_REPLY_ALL,
        title: "Reply all",
        keyval: gdk::Key::r,
        modifiers: mods(&[CONTROL, SHIFT]),
    },
    DefaultShortcut {
        action: ACTION_FORWARD,
        title: "Forward",
        keyval: gdk::Key::f,
        modifiers: mods(&[CONTROL, SHIFT]),
    },
    DefaultShortcut {
        action: ACTION_DELETE,
        title: "Delete",
        keyval: gdk::Key::Delete,
        modifiers: gdk::ModifierType::empty(),
    },
    DefaultShortcut {
        action: ACTION_ARCHIVE,
        title: "Archive",
        keyval: gdk::Key::e,
        modifiers: CONTROL,
    },
    DefaultShortcut {
        action: ACTION_REPORT_JUNK,
        title: "Report junk",
        keyval: gdk::Key::e,
        modifiers: mods(&[CONTROL, SHIFT]),
    },
    DefaultShortcut {
        action: ACTION_SNOOZE,
        title: "Snooze",
        keyval: gdk::Key::s,
        modifiers: mods(&[CONTROL, SHIFT]),
    },
    DefaultShortcut {
        action: ACTION_PIN,
        title: "Pin / unpin",
        keyval: gdk::Key::g,
        modifiers: mods(&[CONTROL, SHIFT]),
    },
    DefaultShortcut {
        action: ACTION_MARK_READ,
        title: "Mark read / unread",
        keyval: gdk::Key::q,
        modifiers: CONTROL,
    },
    DefaultShortcut {
        action: ACTION_PRINT,
        title: "Print",
        keyval: gdk::Key::p,
        modifiers: CONTROL,
    },
    DefaultShortcut {
        action: ACTION_SEARCH,
        title: "Find",
        keyval: gdk::Key::f,
        modifiers: CONTROL,
    },
    DefaultShortcut {
        action: ACTION_CLOSE_PANE,
        title: "Close reading pane",
        keyval: gdk::Key::Escape,
        modifiers: gdk::ModifierType::empty(),
    },
    DefaultShortcut {
        action: ACTION_MAIL,
        title: "Go to Mail",
        keyval: gdk::Key::_1,
        modifiers: CONTROL,
    },
    DefaultShortcut {
        action: ACTION_CALENDAR,
        title: "Go to Calendar",
        keyval: gdk::Key::_2,
        modifiers: CONTROL,
    },
    DefaultShortcut {
        action: ACTION_CONTACTS,
        title: "Go to People",
        keyval: gdk::Key::_3,
        modifiers: CONTROL,
    },
    DefaultShortcut {
        action: ACTION_TASKS,
        title: "Go to Tasks",
        keyval: gdk::Key::_4,
        modifiers: CONTROL,
    },
    DefaultShortcut {
        action: ACTION_LOOKOUT,
        title: "Go to Lookout",
        keyval: gdk::Key::_5,
        modifiers: CONTROL,
    },
    DefaultShortcut {
        action: ACTION_CONFIG,
        title: "Go to Settings",
        keyval: gdk::Key::_6,
        modifiers: CONTROL,
    },
];

/// The title of an action, for Config rows and conflict messages.
pub fn title_for(action: &'static str) -> &'static str {
    DEFAULT_SHORTCUTS.iter().find(|d| d.action == action).map(|d| d.title).unwrap_or(action)
}

/// Parses an accelerator string (`<Primary>n`, `<Control><Shift>r`, `F5`,
/// `Delete`, `space`) into a logical chord. Mirrors the subset of GTK's
/// accelerator syntax the app stores; `<Primary>` normalizes to `Control`
/// (the two are equivalent on Linux, and the running keymap can't express
/// "primary" anyway). A bare uppercase letter means the letter with Shift.
pub fn parse_accel(s: &str) -> Option<(gdk::ModifierType, gdk::Key)> {
    let mut modifiers = gdk::ModifierType::empty();
    let mut rest = s;
    while let Some(inner) = rest.strip_prefix('<') {
        let close = inner.find('>')?;
        let name = &inner[..close];
        rest = &inner[close + 1..];
        match name.to_ascii_lowercase().as_str() {
            "primary" | "control" | "ctrl" => modifiers |= CONTROL,
            "shift" => modifiers |= SHIFT,
            "alt" => modifiers |= ALT,
            "super" | "mod4" => modifiers |= SUPER,
            _ => return None,
        }
    }
    if rest.is_empty() {
        return None;
    }
    let key = if rest.len() == 1 && rest.chars().next().unwrap().is_ascii_uppercase() {
        modifiers |= SHIFT;
        gdk::Key::from_name(rest.to_ascii_lowercase())?
    } else {
        gdk::Key::from_name(rest)?
    };
    Some((modifiers, key))
}

/// The canonical accelerator string for a logical chord, as stored in
/// GSettings and shown in the Config rows (`<Primary>n`, `<Primary><Shift>r`,
/// `F5`). A letter under Shift is stored lowercase so a captured chord and
/// its default read identically.
pub fn format_accel(modifiers: gdk::ModifierType, key: gdk::Key) -> String {
    let mut out = String::new();
    if modifiers.intersects(CONTROL) {
        out.push_str("<Primary>");
    }
    if modifiers.intersects(ALT) {
        out.push_str("<Alt>");
    }
    if modifiers.intersects(SHIFT) {
        out.push_str("<Shift>");
    }
    if modifiers.intersects(SUPER) {
        out.push_str("<Super>");
    }
    let key = if modifiers.intersects(SHIFT) { key.to_lower() } else { key };
    out.push_str(&key.name().unwrap_or_default());
    out
}

/// Resolves a logical keyval to the physical keycode that produces it in
/// the current keymap, preferring the primary group/level pair.
pub fn keycode_for(display: &gdk::Display, keyval: gdk::Key) -> Option<u32> {
    let entries = display.map_keyval(keyval)?;
    entries.iter().find(|k| k.group() == 0 && k.level() == 0).or_else(|| entries.first()).map(|k| k.keycode())
}

/// Translates a pressed physical key back to the logical chord it produces
/// in the current keymap: the keycode's keyval plus the (masked) modifier
/// state. None for dead keys and other keyvals the keymap can't express.
pub fn chord_from_key(display: &gdk::Display, keycode: u32, state: gdk::ModifierType) -> Option<(gdk::ModifierType, gdk::Key)> {
    let (keyval, _group, _level, _consumed) = display.translate_key(keycode, state & MODIFIER_MASK, 0)?;
    if keyval == gdk::Key::VoidSymbol {
        return None;
    }
    Some((state & MODIFIER_MASK, keyval))
}

/// Keys that may be bound without any modifier: function keys, and editing
/// /navigation keys no entry field types. Everything else requires a
/// modifier so shortcuts can't eat ordinary typing.
fn allowed_without_modifier(key: gdk::Key) -> bool {
    if key == gdk::Key::Delete || key == gdk::Key::Escape || key == gdk::Key::BackSpace || key == gdk::Key::Tab || key == gdk::Key::Return {
        return true;
    }
    let name = key.name().unwrap_or_default().to_string();
    name.len() >= 2 && name.len() <= 3 && name.starts_with('F') && name[1..].parse::<u32>().is_ok()
}

/// Why a captured chord was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaptureError {
    /// No modifier and the key isn't in the modifier-free allowlist.
    NeedsModifier,
    /// The key doesn't produce a keyval in this keymap (dead keys etc.).
    Unmapped,
    /// Another action already holds that key + modifier combination.
    Conflicting { other: &'static str },
}

impl CaptureError {
    /// A user-facing message for the toast.
    pub fn message(&self) -> String {
        match self {
            CaptureError::NeedsModifier => "Shortcut needs a modifier (Ctrl, Shift, Alt or Super) or a function key".into(),
            CaptureError::Unmapped => "That key can't be used as a shortcut".into(),
            CaptureError::Conflicting { other } => {
                format!("Already bound to “{}”", title_for(other))
            }
        }
    }
}

/// The live shortcut table for one window: logical accels (defaults
/// overridden from GSettings) resolved against the current keymap into
/// physical chords.
pub struct ShortcutState {
    /// action -> canonical accelerator string.
    accels: HashMap<&'static str, String>,
    /// Resolved physical chords: `(keycode, modifier bits)` -> action.
    by_chord: HashMap<(u32, u32), &'static str>,
    /// While the Config keyboard group records a chord, the window
    /// dispatcher ignores presses so the capture sees every key.
    pub capturing: bool,
}

impl ShortcutState {
    /// Builds the table from the built-in defaults, overridden by the
    /// GSettings `shortcuts` key. Resolution against the keymap happens in
    /// [`rebuild`](Self::rebuild), which needs a display.
    pub fn load(settings: &SettingsStore) -> Self {
        let overrides: HashMap<String, String> = settings
            .get_strv(settings::SHORTCUTS)
            .iter()
            .filter_map(|entry| entry.split_once('='))
            .map(|(action, accel)| (action.to_string(), accel.to_string()))
            .collect();
        let mut accels = HashMap::new();
        for d in DEFAULT_SHORTCUTS {
            let accel = overrides.get(d.action).cloned().unwrap_or_else(|| format_accel(d.modifiers, d.keyval));
            accels.insert(d.action, accel);
        }
        ShortcutState {
            accels,
            by_chord: HashMap::new(),
            capturing: false,
        }
    }

    /// Re-resolves every logical accel against the current keymap. Called at
    /// startup and after every capture/reset. An accel that can't be parsed
    /// or isn't on this keymap is skipped (logged); its action is simply
    /// inactive until the keymap changes.
    pub fn rebuild(&mut self, display: &gdk::Display) {
        self.by_chord.clear();
        for (action, accel) in &self.accels {
            let Some((modifiers, keyval)) = parse_accel(accel) else {
                tracing::warn!(action, accel, "shortcut not parseable");
                continue;
            };
            let Some(keycode) = keycode_for(display, keyval) else {
                tracing::warn!(action, accel, "shortcut keyval not on this keymap");
                continue;
            };
            let chord = (keycode, (modifiers & MODIFIER_MASK).bits());
            if let Some(previous) = self.by_chord.insert(chord, action) {
                tracing::warn!(action, previous, "shortcuts collide after keymap resolution");
            }
        }
    }

    /// The action bound to a pressed key, if any. `state` is the raw event
    /// modifier state; lock-key and other non-shortcut bits are masked out.
    pub fn action_for(&self, keycode: u32, state: gdk::ModifierType) -> Option<&'static str> {
        if self.capturing {
            return None;
        }
        self.by_chord.get(&(keycode, (state & MODIFIER_MASK).bits())).copied()
    }

    /// The canonical accelerator string currently bound to an action.
    pub fn accel_for(&self, action: &'static str) -> String {
        self.accels.get(action).cloned().unwrap_or_default()
    }

    /// Rebinds an action to the chord the user just pressed: validates it,
    /// refuses conflicts, persists the logical accelerator to GSettings and
    /// re-resolves the table. Returns the canonical string on success.
    pub fn set_chord(&mut self, settings: &SettingsStore, display: &gdk::Display, action: &'static str, keycode: u32, state: gdk::ModifierType) -> Result<String, CaptureError> {
        let (modifiers, keyval) = chord_from_key(display, keycode, state).ok_or(CaptureError::Unmapped)?;
        if (modifiers & MODIFIER_MASK).is_empty() && !allowed_without_modifier(keyval) {
            return Err(CaptureError::NeedsModifier);
        }
        let Some(new_keycode) = keycode_for(display, keyval) else {
            return Err(CaptureError::Unmapped);
        };
        let chord = (new_keycode, (modifiers & MODIFIER_MASK).bits());
        if let Some(holder) = self.by_chord.get(&chord) {
            if *holder != action {
                return Err(CaptureError::Conflicting { other: holder });
            }
        }
        let accel = format_accel(modifiers, keyval);
        self.accels.insert(action, accel.clone());
        self.rebuild(display);
        self.persist(settings);
        Ok(accel)
    }

    /// Restores every action to its built-in default.
    pub fn reset_all(&mut self, settings: &SettingsStore, display: &gdk::Display) {
        for d in DEFAULT_SHORTCUTS {
            self.accels.insert(d.action, format_accel(d.modifiers, d.keyval));
        }
        self.rebuild(display);
        settings.set_strv(settings::SHORTCUTS, Vec::new());
    }

    /// Writes every current accel to GSettings as `action=accel` entries.
    fn persist(&self, settings: &SettingsStore) {
        let mut entries: Vec<String> = self.accels.iter().map(|(action, accel)| format!("{action}={accel}")).collect();
        entries.sort();
        settings.set_strv(settings::SHORTCUTS, entries);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chord(modifiers: gdk::ModifierType, name: &str) -> (gdk::ModifierType, gdk::Key) {
        (modifiers, gdk::Key::from_name(name).unwrap())
    }

    #[test]
    fn parse_plain_key() {
        assert_eq!(parse_accel("n"), Some(chord(empty(), "n")));
        assert_eq!(parse_accel("F5"), Some(chord(empty(), "F5")));
        assert_eq!(parse_accel("Delete"), Some(chord(empty(), "Delete")));
        assert_eq!(parse_accel("space"), Some(chord(empty(), "space")));
        assert_eq!(parse_accel("1"), Some(chord(empty(), "1")));
    }

    #[test]
    fn parse_modifiers() {
        assert_eq!(parse_accel("<Primary>n"), Some(chord(CONTROL, "n")));
        assert_eq!(parse_accel("<Control>n"), Some(chord(CONTROL, "n")));
        assert_eq!(parse_accel("<Ctrl><Shift>r"), Some(chord(CONTROL | SHIFT, "r")));
        assert_eq!(parse_accel("<Alt><Super>p"), Some(chord(ALT | SUPER, "p")));
        assert_eq!(parse_accel("<Shift>r"), Some(chord(SHIFT, "r")));
    }

    #[test]
    fn parse_uppercase_letter_means_shift() {
        assert_eq!(parse_accel("<Primary>R"), Some(chord(CONTROL | SHIFT, "r")));
    }

    #[test]
    fn parse_rejects_malformed() {
        assert!(parse_accel("").is_none());
        assert!(parse_accel("<Primary>").is_none());
        assert!(parse_accel("<Bogus>x").is_none());
        assert!(parse_accel("<Primary").is_none());
        assert!(parse_accel("<Primary>").is_none());
        assert!(parse_accel("<>x").is_none());
        assert!(parse_accel("n><Primary>").is_none());
    }

    #[test]
    fn format_round_trips() {
        for accel in ["<Primary>n", "<Primary><Shift>r", "F5", "Delete", "space", "<Primary>1", "<Primary><Alt><Shift><Super>q"] {
            let (modifiers, key) = parse_accel(accel).unwrap();
            assert_eq!(format_accel(modifiers, key), accel, "canonical form must be stable");
        }
    }

    #[test]
    fn format_lowercases_letters_under_shift() {
        assert_eq!(format_accel(CONTROL | SHIFT, gdk::Key::from_name("R").unwrap()), "<Primary><Shift>r");
    }

    #[test]
    fn defaults_are_unique() {
        let mut actions = std::collections::HashSet::new();
        let mut chords = std::collections::HashSet::new();
        for d in DEFAULT_SHORTCUTS {
            assert!(actions.insert(d.action), "duplicate action {}", d.action);
            let key = d.keyval.to_lower();
            assert!(chords.insert((d.modifiers & MODIFIER_MASK, key)), "duplicate chord for {}", d.action);
        }
        assert_eq!(actions.len(), 19);
    }

    #[test]
    fn defaults_parse() {
        for d in DEFAULT_SHORTCUTS {
            let accel = format_accel(d.modifiers, d.keyval);
            assert_eq!(parse_accel(&accel), Some((d.modifiers, d.keyval.to_lower())));
        }
    }

    #[test]
    fn load_merges_overrides() {
        let store = settings::resolve();
        store.set_strv(settings::SHORTCUTS, vec!["compose=<Primary>m".into()]);
        let state = ShortcutState::load(&store);
        assert_eq!(state.accel_for(ACTION_COMPOSE), "<Primary>m");
        assert_eq!(state.accel_for(ACTION_REPLY), "<Primary>r");
    }

    #[test]
    fn load_ignores_garbage_entries() {
        let store = settings::resolve();
        store.set_strv(settings::SHORTCUTS, vec!["not-an-entry".into(), "=x".into()]);
        let state = ShortcutState::load(&store);
        assert_eq!(state.accel_for(ACTION_COMPOSE), "<Primary>n");
    }

    #[test]
    fn allowed_without_modifier_set() {
        assert!(allowed_without_modifier(gdk::Key::F1));
        assert!(allowed_without_modifier(gdk::Key::F12));
        assert!(allowed_without_modifier(gdk::Key::Delete));
        assert!(allowed_without_modifier(gdk::Key::Escape));
        assert!(allowed_without_modifier(gdk::Key::Tab));
        assert!(!allowed_without_modifier(gdk::Key::n));
        assert!(!allowed_without_modifier(gdk::Key::space));
        assert!(!allowed_without_modifier(gdk::Key::_1));
        assert!(!allowed_without_modifier(gdk::Key::minus));
    }

    #[test]
    fn capture_validation_allowlist_is_pinned() {
        // The capture gate's keymap-dependent half (`NeedsModifier` vs
        // `Unmapped`) can't run headless, so the pure half - the
        // modifier-free allowlist - is pinned by
        // `allowed_without_modifier_set`; the rest is covered by the manual
        // capture smoke test.
        assert!(!allowed_without_modifier(gdk::Key::x));
    }

    fn empty() -> gdk::ModifierType {
        gdk::ModifierType::empty()
    }
}
