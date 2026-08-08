//! Bundled named-color theming.
//!
//! Every colour literal the app's CSS rules use is a `lookout-*` named
//! colour defined in `data/themes/base.css`, overridable by the bundled
//! flat-token themes (`flat-dark.css`, `flat-light.css`) and by a custom
//! accent colour. [`ThemeManager`] concatenates base + theme + accent into a
//! single display-level `CssProvider` at APPLICATION priority and reloads it
//! whenever the selection changes, so switching themes is live.
//!
//! The scheme-aware half of the palette (the tokens that reference
//! `@theme_*` / `@accent_color`) re-resolves automatically when the system
//! switches light/dark; the fixed half (translucent panes, toolbar bands,
//! the amber flag) keeps its values by design, as documented in `base.css`.
//!
//! The bundled CSS is read from the GResource bundle, with `include_str!`
//! fallbacks so dev builds without `glib-compile-resources` still get the
//! themes (same best-effort pattern as the bundled backgrounds).

use gtk::gdk;

/// Theme ids as stored in GSettings `theme-id`; also the file names under
/// `data/themes/` (with `.css` appended) except `system`, which is
/// `base.css` alone. In UI order (Config → Appearance → "Theme").
pub const THEMES: [&str; 3] = ["system", "flat-dark", "flat-light"];

/// The default theme, and the fallback for an unknown stored id.
pub const DEFAULT_THEME: &str = "flat-dark";

/// The GSettings key names (mirrored in `settings.rs` constants).
pub const THEME_ID_KEY: &str = "theme-id";
pub const ACCENT_COLOR_KEY: &str = "accent-color";

/// `include_str!` fallbacks: used only when the compiled GResource bundle
/// isn't available (missing `glib-compile-resources` at build time).
static BASE_CSS: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../data/themes/base.css"));
static FLAT_DARK_CSS: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../data/themes/flat-dark.css"));
static FLAT_LIGHT_CSS: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../data/themes/flat-light.css"));

/// Holds the one display-level provider the whole theme stack lives in.
/// Created once in `build_window` before any other provider registers, so
/// the app's rule providers (also APPLICATION priority) can reference the
/// tokens here without ever being shadowed by them.
pub struct ThemeManager {
    provider: gtk::CssProvider,
}

impl ThemeManager {
    /// Creates the provider, applies the default theme, and registers it on
    /// the default display (a no-op when GTK has no display).
    pub fn install() -> std::rc::Rc<ThemeManager> {
        let manager = std::rc::Rc::new(ThemeManager {
            provider: gtk::CssProvider::new(),
        });
        manager.apply(DEFAULT_THEME, None);
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::style_context_add_provider_for_display(&display, &manager.provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
        }
        manager
    }

    /// Reloads the stack: `base.css`, then the selected theme's overrides
    /// (nothing for `system`), then the custom accent when one is set.
    pub fn apply(&self, theme_id: &str, accent: Option<&str>) {
        let mut css = bundled_css("base.css");
        if theme_id != "system" {
            let themed = bundled_css(&format!("{theme_id}.css"));
            if !themed.is_empty() {
                css.push('\n');
                css.push_str(&themed);
            }
        }
        if let Some(hex) = accent.filter(|hex| !hex.is_empty()) {
            css.push('\n');
            css.push_str(&accent_css(hex));
        }
        self.provider.load_from_string(&css);
    }
}

/// A theme file's contents: the GResource copy when the bundle is live,
/// otherwise the `include_str!` constant. Unknown ids yield an empty string,
/// which `apply` skips, leaving the base palette in place.
fn bundled_css(name: &str) -> String {
    let path = format!("/io/github/gavindi/Lookout/themes/{name}");
    if let Some(bytes) = crate::resources::bytes(&path) {
        return String::from_utf8_lossy(bytes.as_ref()).into_owned();
    }
    match name {
        "base.css" => BASE_CSS.to_string(),
        "flat-dark.css" => FLAT_DARK_CSS.to_string(),
        "flat-light.css" => FLAT_LIGHT_CSS.to_string(),
        _ => String::new(),
    }
}

/// The accent override: redefines the whole accent family so widgets that
/// derive from `accent_bg_color`/`accent_hover_color`/`accent_active_color`
/// follow the picker too, not just text drawn with `@accent_color`. Applied
/// at APPLICATION priority, so it beats the libadwaita theme's own accent
/// definitions. The `shade()` factors are a scheme-neutral approximation of
/// the theme's light/dark accents.
fn accent_css(hex: &str) -> String {
    format!(
        "/* Custom accent (Config → Appearance) */\n\
         @define-color accent_color {hex};\n\
         @define-color accent_bg_color shade({hex}, 0.9);\n\
         @define-color accent_fg_color white;\n\
         @define-color accent_hover_color shade({hex}, 0.95);\n\
         @define-color accent_active_color shade({hex}, 0.8);\n"
    )
}

/// Parses a stored accent (`accent-color` GSettings value: a CSS colour
/// string as `gdk::RGBA::to_str` produces, or empty = follow system).
pub fn accent_rgba(stored: &str) -> Option<gdk::RGBA> {
    if stored.is_empty() {
        return None;
    }
    gdk::RGBA::parse(stored).ok()
}

/// The storage form of a picked accent, for the `accent-color` GSettings key.
pub fn rgba_to_stored(rgba: &gdk::RGBA) -> String {
    rgba.to_str().to_string()
}

/// Index of a theme id in [`THEMES`] (for the config `ComboRow`); unknown
/// ids fall back to the default theme's index.
pub fn theme_index(theme_id: &str) -> u32 {
    let default = THEMES.iter().position(|t| *t == DEFAULT_THEME).unwrap_or(0);
    THEMES.iter().position(|t| *t == theme_id).unwrap_or(default) as u32
}

/// Theme id at a [`THEMES`] index (from the config `ComboRow`); out-of-range
/// indexes fall back to the default theme.
pub fn theme_at(index: u32) -> &'static str {
    THEMES.get(index as usize).copied().unwrap_or(DEFAULT_THEME)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gtk::prelude::*;

    /// GTK's display-dependent tests can only run when the host has a display
    /// AND this test's thread is the one that initialized GTK - gtk::init()
    /// panics (it doesn't return an error) if another test thread got there
    /// first, so the suite skips instead of failing on whichever thread the
    /// harness happened to use. Same self-skipping convention as
    /// `calendar_view.rs`'s display tests.
    fn gtk_ok() -> bool {
        if gtk::is_initialized() && !gtk::is_initialized_main_thread() {
            return false;
        }
        gtk::init().is_ok()
    }

    #[test]
    fn theme_index_maps_ids_to_row_positions() {
        assert_eq!(theme_index("system"), 0);
        assert_eq!(theme_index("flat-dark"), 1);
        assert_eq!(theme_index("flat-light"), 2);
        assert_eq!(theme_at(0), "system");
        assert_eq!(theme_at(1), "flat-dark");
        assert_eq!(theme_at(2), "flat-light");
        assert_eq!(theme_index("no-such-theme"), 1, "unknown ids land on the default");
        assert_eq!(theme_at(99), DEFAULT_THEME, "out-of-range indexes land on the default");
    }

    #[test]
    fn accent_storage_round_trips() {
        // Byte-exact values: the stored string is 8-bit quantized.
        let rgba = gdk::RGBA::new(0.2, 0.4, 0.8, 1.0);
        let stored = rgba_to_stored(&rgba);
        let parsed = accent_rgba(&stored).expect("stored accent should parse back");
        assert!((parsed.red() - 0.2).abs() < 0.001);
        assert!((parsed.green() - 0.4).abs() < 0.001);
        assert!((parsed.blue() - 0.8).abs() < 0.001);
        assert!(accent_rgba("").is_none(), "empty accent means follow the system");
        assert!(accent_rgba("not-a-colour").is_none(), "garbage accent is ignored");
    }

    #[test]
    fn bundled_css_falls_back_without_a_bundle() {
        assert!(bundled_css("base.css").contains("lookout-pane-bg"));
        assert!(bundled_css("flat-dark.css").contains("lookout-unread"));
        assert!(bundled_css("flat-light.css").contains("lookout-pane-bg"));
        assert!(bundled_css("flat-light.css").contains("alpha(@theme_bg_color, 0.85)"));
        assert!(bundled_css("no-such.css").is_empty());
    }

    #[test]
    fn theme_stack_parses_as_valid_gtk_css() {
        // `load_from_string` swallows parse errors (logging them to GLib), so
        // assert on `to_str()`: a provider that failed to parse the rules
        // round-trips to an empty string. Skipped when the test host
        // has no display (or GTK belongs to another thread).
        if !gtk_ok() {
            return;
        }
        let manager = ThemeManager::install();
        for theme in THEMES {
            manager.apply(theme, None);
            assert!(!manager.provider.to_str().is_empty(), "{theme} theme did not parse");
        }
        manager.apply(DEFAULT_THEME, Some("rgb(53,132,228)"));
        let css = manager.provider.to_str();
        assert!(!css.is_empty(), "accent-carrying theme did not parse");
    }

    #[test]
    fn theme_stack_resolves_on_widget_style_contexts() {
        // The app's rules reference `@lookout-*` tokens defined only in the
        // theme provider, and a user-picked accent must beat libadwaita's on
        // real widgets. Uses the deprecated GtkStyleContext lookup because
        // gtk4-rs 0.11 does not wrap gtk_widget_css_lookup_color; skipped
        // when the test host has no display (or GTK belongs to another
        // thread).
        if !gtk_ok() {
            return;
        }
        let manager = ThemeManager::install();
        manager.apply("flat-dark", None);
        let window = gtk::Window::builder().default_width(40).default_height(40).build();
        let button = gtk::ToggleButton::new();
        window.set_child(Some(&button));
        window.present();
        for _ in 0..10 {
            while gtk::glib::MainContext::default().iteration(false) {}
        }
        use gtk::glib::prelude::Cast;
        use gtk::glib::translate::ToGlibPtr;
        let widget = button.upcast_ref::<gtk::Widget>();
        let context = unsafe { gtk::ffi::gtk_widget_get_style_context(widget.to_glib_none().0) };
        assert!(!context.is_null(), "no style context for the button");

        let mut color = gtk::gdk::ffi::GdkRGBA {
            red: 0.0,
            green: 0.0,
            blue: 0.0,
            alpha: 0.0,
        };
        let resolved = unsafe { gtk::ffi::gtk_style_context_lookup_color(context, c"lookout_unread".as_ptr(), &mut color) };
        assert!(resolved != 0, "lookout-unread did not resolve on the widget's style context");
        assert!((color.red - 0x4d_u32 as f32 / 255.0).abs() < 0.01, "flat-dark unread red = {}", color.red);
        assert!((color.green - 0x9d_u32 as f32 / 255.0).abs() < 0.01, "flat-dark unread green = {}", color.green);
        assert!((color.blue - 0xff_u32 as f32 / 255.0).abs() < 0.01, "flat-dark unread blue = {}", color.blue);

        manager.apply(DEFAULT_THEME, Some("rgb(18,52,86)"));
        let mut accent = gtk::gdk::ffi::GdkRGBA {
            red: 0.0,
            green: 0.0,
            blue: 0.0,
            alpha: 0.0,
        };
        let resolved = unsafe { gtk::ffi::gtk_style_context_lookup_color(context, c"accent_color".as_ptr(), &mut accent) };
        assert!(resolved != 0, "accent_color did not resolve on the widget's style context");
        assert!((accent.red - 18.0_f32 / 255.0).abs() < 0.01, "accent red = {}", accent.red);
        assert!((accent.green - 52.0_f32 / 255.0).abs() < 0.01, "accent green = {}", accent.green);
        assert!((accent.blue - 86.0_f32 / 255.0).abs() < 0.01, "accent blue = {}", accent.blue);
    }
}
