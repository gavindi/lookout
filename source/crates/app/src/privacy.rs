//! Privacy / demo mode: blur sensitive text for screenshots.
//!
//! Config → Advanced → "Blur sensitive information" turns this on. When it is
//! on, email addresses, account names and people's names are blurred
//! throughout the interface (message list, reading pane, folder tree,
//! contacts, calendar, Settings account rows, ...) so a screenshot for a demo
//! or a marketing page doesn't leak personal details.
//!
//! The mechanism is a single display-level `gtk::CssProvider` at APPLICATION
//! priority that carries either nothing (off) or a `filter: blur(...)` rule
//! (on). Widgets that show sensitive text are tagged once, at construction,
//! with [`SENSITIVE_CLASS`]; toggling the setting reloads the provider, which
//! restyles every tagged widget live. Because the rule is class-based rather
//! than ancestry-based, it reaches the main window, dialogs, popovers and
//! pop-out windows alike.
//!
//! Two classes exist because libadwaita rows (`adw::ActionRow` and its
//! subclasses) put their text in internal `label.title`/`label.subtitle`
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
/// Blur radius, in pixels, applied to sensitive text. Large enough that the
/// text is not readable (nor trivially recoverable).
pub const BLUR_RADIUS_PX: u32 = 6;

/// Tags a leaf widget as sensitive: its whole rendering is blurred while
/// privacy mode is on.
pub fn mark<W: IsA<gtk::Widget>>(widget: &W) {
    widget.add_css_class(SENSITIVE_CLASS);
}

/// Tags a libadwaita row as sensitive: only its title/subtitle labels are
/// blurred while privacy mode is on, so the row's controls stay usable.
pub fn mark_row<W: IsA<gtk::Widget>>(row: &W) {
    row.add_css_class(SENSITIVE_ROW_CLASS);
}

/// Holds the one display-level provider privacy mode toggles.
pub struct PrivacyMode {
    provider: gtk::CssProvider,
}

impl PrivacyMode {
    /// Creates the provider, starts it disabled, and registers it on the
    /// default display (a no-op when GTK has no display).
    pub fn install() -> Rc<PrivacyMode> {
        let mode = Rc::new(PrivacyMode {
            provider: gtk::CssProvider::new(),
        });
        mode.set_enabled(false);
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::style_context_add_provider_for_display(&display, &mode.provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
        }
        mode
    }

    /// Turns the blur on or off by reloading the provider: the blur rule while
    /// on, an empty stylesheet while off. Reloading restyles every tagged
    /// widget immediately.
    pub fn set_enabled(&self, enabled: bool) {
        if enabled {
            self.provider.load_from_string(&blur_css());
        } else {
            self.provider.load_from_string("");
        }
    }
}

/// The CSS the provider carries while privacy mode is on.
fn blur_css() -> String {
    format!(
        "/* Privacy mode (Config → Advanced → Blur sensitive information) */\n\
         .{SENSITIVE_CLASS} {{ filter: blur({BLUR_RADIUS_PX}px); }}\n\
         .{SENSITIVE_ROW_CLASS} .title,\n\
         .{SENSITIVE_ROW_CLASS} .subtitle {{ filter: blur({BLUR_RADIUS_PX}px); }}\n"
    )
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
    fn css_mentions_both_classes_and_a_radius() {
        let css = blur_css();
        assert!(css.contains(SENSITIVE_CLASS));
        assert!(css.contains(SENSITIVE_ROW_CLASS));
        assert!(css.contains(".title"));
        assert!(css.contains(".subtitle"));
        assert!(css.contains(&format!("{BLUR_RADIUS_PX}px")));
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
    }

    #[test]
    fn toggling_loads_and_clears_the_blur_css() {
        // `load_from_string` swallows parse errors (logging them to GLib), so
        // assert on `to_str()`: a provider that failed to parse round-trips to
        // an empty string.
        if !gtk_ok() {
            return;
        }
        let mode = PrivacyMode::install();
        mode.set_enabled(true);
        let css = mode.provider.to_str();
        assert!(!css.is_empty(), "blur CSS did not parse");
        assert!(css.contains(SENSITIVE_CLASS));
        mode.set_enabled(false);
        assert!(mode.provider.to_str().is_empty(), "disabling should clear the provider");
    }
}
