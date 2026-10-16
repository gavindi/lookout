//! Privacy / demo mode: blur text for screenshots.
//!
//! The title bar's cycling toggle picks one of three [`BlurMode`]s (there is
//! no Settings row for it - the header button is the control):
//!
//! - [`BlurMode::Off`] - nothing is blurred;
//! - [`BlurMode::Sensitive`] - email addresses, account names and people's
//!   names are blurred, everywhere the app has tagged them (message list,
//!   reading pane, folder tree, contacts, calendar, Settings account rows,
//!   ...);
//! - [`BlurMode::All`] - every piece of text is blurred, including message
//!   bodies, so nothing in a screenshot is readable.
//!
//! The mechanism is a single display-level `gtk::CssProvider` at APPLICATION
//! priority whose contents depend on the mode. Sensitive mode needs the app to
//! have tagged widgets with [`SENSITIVE_CLASS`] / [`SENSITIVE_ROW_CLASS`] (see
//! [`mark`] / [`mark_row`]); All mode is broad - it blurs every GTK text node
//! (`label`, `entry`, `textview`, `spinbutton`) plus the content views tagged
//! with [`BLUR_ALL_CLASS`] (see [`mark_blur_all`]), which is how WebKit message
//! bodies get covered. Switching modes reloads the provider, which restyles
//! every widget live. Because the rules are class/element based rather than
//! ancestry-based, they reach the main window, dialogs, popovers and pop-out
//! windows alike.
//!
//! Two sensitive-mode classes exist because libadwaita rows (`adw::ActionRow`
//! and its subclasses) put their text in internal `label.title`/`label.subtitle`
//! nodes: blurring the row widget itself would also blur the switch/button in
//! its suffix. [`mark_row`] tags the row and the CSS blurs only those two
//! labels, leaving the row's controls sharp.
//!
//! `filter: blur()` has been supported by GTK well before the 4.14 floor this
//! app requires (only CSS custom properties need 4.16), so no version guard is
//! needed.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::rc::Rc;

use gtk::prelude::*;

/// CSS class applied to a leaf widget (a label, avatar, chip, ...) whose whole
/// rendering is sensitive.
pub const SENSITIVE_CLASS: &str = "lookout-sensitive";
/// CSS class applied to a libadwaita row whose title/subtitle are sensitive;
/// only those two labels blur, not the row's switch/button suffix.
pub const SENSITIVE_ROW_CLASS: &str = "lookout-sensitive-row";
/// CSS class applied to a content view (a WebKit message body, ...) that
/// cannot be reached by the `label`/`entry`/`textview` selectors, so the "All
/// text" mode has to blur the whole widget.
pub const BLUR_ALL_CLASS: &str = "lookout-blur-all";
/// Blur radius, in pixels, applied to blurred text. Large enough that the text
/// is not readable (nor trivially recoverable).
pub const BLUR_RADIUS_PX: u32 = 6;

/// How much the privacy blur hides.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum BlurMode {
    /// Nothing blurred.
    #[default]
    Off,
    /// Only the app's tagged sensitive text (addresses, account names, names).
    Sensitive,
    /// Every piece of text, message bodies included.
    All,
}

impl BlurMode {
    /// Parses a `settings::PRIVACY_BLUR_MODE` value; anything unknown is `Off`.
    pub fn from_setting(value: &str) -> BlurMode {
        match value {
            "sensitive" => BlurMode::Sensitive,
            "all" => BlurMode::All,
            _ => BlurMode::Off,
        }
    }

    /// The stored form, written to `settings::PRIVACY_BLUR_MODE`.
    pub fn as_setting(self) -> &'static str {
        match self {
            BlurMode::Off => "off",
            BlurMode::Sensitive => "sensitive",
            BlurMode::All => "all",
        }
    }

    /// The next mode in the title bar's cycle: Off → Sensitive → All → Off.
    pub fn next(self) -> BlurMode {
        match self {
            BlurMode::Off => BlurMode::Sensitive,
            BlurMode::Sensitive => BlurMode::All,
            BlurMode::All => BlurMode::Off,
        }
    }
}

/// Tags a leaf widget as sensitive: its whole rendering is blurred while the
/// mode is [`BlurMode::Sensitive`] or [`BlurMode::All`].
pub fn mark<W: IsA<gtk::Widget>>(widget: &W) {
    widget.add_css_class(SENSITIVE_CLASS);
}

/// Tags a libadwaita row as sensitive: only its title/subtitle labels are
/// blurred while the mode is [`BlurMode::Sensitive`] or [`BlurMode::All`], so
/// the row's controls stay usable.
pub fn mark_row<W: IsA<gtk::Widget>>(row: &W) {
    row.add_css_class(SENSITIVE_ROW_CLASS);
}

/// Tags a content view (a WebKit message body or editor, ...) whose whole
/// rendering is blurred by [`BlurMode::All`] - the GTK text selectors can't
/// reach inside a `WebView`.
pub fn mark_blur_all<W: IsA<gtk::Widget>>(widget: &W) {
    widget.add_css_class(BLUR_ALL_CLASS);
}

/// Holds the one display-level provider privacy blur toggles.
pub struct PrivacyMode {
    provider: gtk::CssProvider,
}

impl PrivacyMode {
    /// Creates the provider, starts it in [`BlurMode::Off`], and registers it
    /// on the default display (a no-op when GTK has no display).
    pub fn install() -> Rc<PrivacyMode> {
        let mode = Rc::new(PrivacyMode {
            provider: gtk::CssProvider::new(),
        });
        mode.set_mode(BlurMode::Off);
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::style_context_add_provider_for_display(&display, &mode.provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
        }
        mode
    }

    /// Switches the blur on, off or "everything" by reloading the provider.
    /// Reloading restyles every tagged widget immediately.
    pub fn set_mode(&self, mode: BlurMode) {
        self.provider.load_from_string(&blur_css(mode));
    }
}

/// The CSS the provider carries for `mode`.
fn blur_css(mode: BlurMode) -> String {
    match mode {
        BlurMode::Off => String::new(),
        BlurMode::Sensitive => format!(
            "/* Privacy blur: sensitive information (Config → Advanced) */\n\
             .{SENSITIVE_CLASS} {{ filter: blur({BLUR_RADIUS_PX}px); }}\n\
             .{SENSITIVE_ROW_CLASS} .title,\n\
             .{SENSITIVE_ROW_CLASS} .subtitle {{ filter: blur({BLUR_RADIUS_PX}px); }}\n"
        ),
        BlurMode::All => format!(
            "/* Privacy blur: all text (Config → Advanced) */\n\
             label,\n\
             entry,\n\
             textview,\n\
             spinbutton {{ filter: blur({BLUR_RADIUS_PX}px); }}\n\
             .{BLUR_ALL_CLASS} {{ filter: blur({BLUR_RADIUS_PX}px); }}\n"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GTK's display-dependent tests can only run when the host has a display
    /// AND this test's thread is the one that initialized GTK - same
    /// self-skipping convention as `theme.rs` and `calendar_view.rs`, routed
    /// through the shared `gtk_test::gtk_ready` lock.
    fn gtk_ok() -> bool {
        crate::gtk_test::gtk_ready()
    }

    #[test]
    fn cycle_visits_every_mode_and_returns_to_off() {
        let start = BlurMode::Off;
        let next = start.next();
        assert_eq!(next, BlurMode::Sensitive);
        let next = next.next();
        assert_eq!(next, BlurMode::All);
        assert_eq!(next.next(), BlurMode::Off, "the cycle wraps back to off");
    }

    #[test]
    fn setting_values_round_trip() {
        for mode in [BlurMode::Off, BlurMode::Sensitive, BlurMode::All] {
            assert_eq!(BlurMode::from_setting(mode.as_setting()), mode);
        }
        assert_eq!(BlurMode::from_setting("no-such-mode"), BlurMode::Off, "unknown settings are off");
        assert_eq!(BlurMode::default(), BlurMode::Off);
    }

    #[test]
    fn css_is_empty_off_and_covers_each_mode_on() {
        assert!(blur_css(BlurMode::Off).is_empty());
        let sensitive = blur_css(BlurMode::Sensitive);
        assert!(sensitive.contains(SENSITIVE_CLASS));
        assert!(sensitive.contains(SENSITIVE_ROW_CLASS));
        assert!(!sensitive.contains(BLUR_ALL_CLASS));
        let all = blur_css(BlurMode::All);
        for selector in ["label", "entry", "textview", "spinbutton"] {
            assert!(all.contains(selector), "all-text CSS should blur {selector}");
        }
        assert!(all.contains(BLUR_ALL_CLASS));
        assert!(all.contains(&format!("{BLUR_RADIUS_PX}px")));
    }

    #[test]
    fn mark_adds_the_expected_classes() {
        if !gtk_ok() {
            return;
        }
        let label = gtk::Label::new(Some("a@example.com"));
        mark(&label);
        assert!(label.has_css_class(SENSITIVE_CLASS));
        let row = gtk::ListBoxRow::new();
        mark_row(&row);
        assert!(row.has_css_class(SENSITIVE_ROW_CLASS));
        let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
        mark_blur_all(&body);
        assert!(body.has_css_class(BLUR_ALL_CLASS));
    }

    #[test]
    fn switching_modes_loads_and_clears_the_provider() {
        // `load_from_string` swallows parse errors (logging them to GLib), so
        // assert on `to_str()`: a provider that failed to parse round-trips to
        // an empty string.
        if !gtk_ok() {
            return;
        }
        let mode = PrivacyMode::install();
        mode.set_mode(BlurMode::Sensitive);
        let css = mode.provider.to_str();
        assert!(!css.is_empty(), "sensitive CSS did not parse");
        assert!(css.contains(SENSITIVE_CLASS));
        mode.set_mode(BlurMode::All);
        let css = mode.provider.to_str();
        assert!(!css.is_empty(), "all-text CSS did not parse");
        assert!(css.contains(BLUR_ALL_CLASS));
        mode.set_mode(BlurMode::Off);
        assert!(mode.provider.to_str().is_empty(), "off should clear the provider");
    }
}
