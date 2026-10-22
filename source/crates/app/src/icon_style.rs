//! Optional Fluent System Icons (OUTLOOK_PARITY.md, UX-80).
//!
//! Lookout normally uses the desktop's GNOME icon theme. This module adds a
//! second, opt-in pack made of Microsoft's Fluent System Icons, imported under
//! [`crate::resources`] and aliased to the icon names the app already asks the
//! theme for (see `source/data/icons/fluent`). Because it is a partial GTK
//! icon theme that inherits the user's current theme, switching packs changes
//! every `GtkImage` without touching a single call site, and any icon the pack
//! does not override keeps its normal GNOME look.
//!
//! GTK only discovers a *named* theme through an `index.theme` on a filesystem
//! search path (GResource paths are scanned for unthemed and hicolor icons
//! only), so the bundled aliases are materialised into the user's cache
//! directory when Fluent is selected. The generated `index.theme` inherits the
//! theme that was active before the switch.
//!
//! The switch is stored in the `icon-style` GSettings key. The View ribbon's
//! **Icons ▾** menu and Config → Appearance both drive one `win.icon-style`
//! action, exactly like `ribbon-style`.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};

use gtk::{gio, glib};

/// The generated theme's directory name and `gtk-icon-theme-name` value.
pub(crate) const THEME_NAME: &str = "Lookout-Fluent";

/// The bundled aliases, under the GResource prefix registered by
/// [`crate::resources::register`].
const RESOURCE_ICON_DIR: &str = "/io/github/gavindi/Lookout/icons/Lookout-Fluent/scalable/actions";

/// Whether Fluent glyphs are in force. Used by the two-state icons whose
/// artwork is not symbolic (`window::support::pin_icon_name`).
static FLUENT_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Which icon pack is in force (`icon-style` in GSettings).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum IconStyle {
    /// The desktop's own icon theme - the default, so upgrading changes
    /// nothing.
    #[default]
    Gnome,
    /// Microsoft's Fluent System Icons, aliased over the GNOME names.
    Fluent,
}

impl IconStyle {
    /// In menu order.
    pub(crate) const ALL: [IconStyle; 2] = [IconStyle::Gnome, IconStyle::Fluent];

    /// The value stored in GSettings and carried by the `win.icon-style`
    /// stateful action.
    pub(crate) fn code(self) -> &'static str {
        match self {
            IconStyle::Gnome => "gnome",
            IconStyle::Fluent => "fluent",
        }
    }

    /// An unknown saved value reads as the default rather than failing.
    pub(crate) fn from_code(code: &str) -> IconStyle {
        IconStyle::ALL.into_iter().find(|style| style.code().eq_ignore_ascii_case(code.trim())).unwrap_or_default()
    }

    /// The label shown by the View ribbon menu, Config → Appearance and the
    /// action's tooltip.
    pub(crate) fn label(self) -> &'static str {
        match self {
            IconStyle::Gnome => "GNOME icons",
            IconStyle::Fluent => "Fluent icons",
        }
    }

    /// Its position in [`IconStyle::ALL`], i.e. in the menu and the Config row.
    pub(crate) fn index(self) -> u32 {
        IconStyle::ALL.iter().position(|style| *style == self).unwrap_or(0) as u32
    }
}

/// Whether Fluent glyphs are in force (a cheap check for icon choices made
/// outside the theme, such as Pin's bundled artwork).
pub(crate) fn is_fluent() -> bool {
    FLUENT_ACTIVE.load(Ordering::Relaxed)
}

/// Shared icon-pack state. One instance lives in `window::state::UiState`; it
/// remembers the theme to restore and the pack in force.
#[derive(Clone)]
pub(crate) struct IconStyleState {
    inner: Rc<Inner>,
}

struct Inner {
    style: Cell<IconStyle>,
    /// The `gtk-icon-theme-name` in force before Fluent was first selected.
    original_theme: RefCell<Option<String>>,
}

impl IconStyleState {
    /// Create the state and apply `style` before the window's widgets are
    /// built. A failure (no display, an unwritable cache) leaves GNOME icons
    /// in force and is only logged: icons are never worth failing startup for.
    pub(crate) fn new(style: IconStyle) -> Self {
        let original_theme = gtk::Settings::default().and_then(|settings| settings.gtk_icon_theme_name()).map(|name| name.to_string());
        let state = IconStyleState {
            inner: Rc::new(Inner {
                style: Cell::new(IconStyle::Gnome),
                original_theme: RefCell::new(original_theme),
            }),
        };
        if style != IconStyle::Gnome {
            if let Err(error) = state.apply(style) {
                tracing::warn!(%error, "Fluent icons unavailable; keeping GNOME icons");
            }
        }
        state
    }

    /// The pack actually in force (GNOME if a Fluent switch failed).
    pub(crate) fn style(&self) -> IconStyle {
        self.inner.style.get()
    }

    /// Switch packs. Returns the reason when Fluent could not be enabled, in
    /// which case the previous pack stays in force.
    pub(crate) fn set_style(&self, style: IconStyle) -> Result<(), String> {
        if self.inner.style.get() == style {
            return Ok(());
        }
        self.apply(style)
    }

    fn apply(&self, style: IconStyle) -> Result<(), String> {
        let settings = gtk::Settings::default().ok_or_else(|| "GTK has no settings object".to_string())?;
        match style {
            IconStyle::Gnome => {
                let original = self.inner.original_theme.borrow().clone().unwrap_or_else(|| "Adwaita".to_string());
                settings.set_gtk_icon_theme_name(Some(&original));
            }
            IconStyle::Fluent => {
                let inherited = self
                    .inner
                    .original_theme
                    .borrow()
                    .clone()
                    .filter(|name| name != THEME_NAME)
                    .unwrap_or_else(|| "Adwaita".to_string());
                let theme_root = materialise(&inherited)?;
                let display = gtk::gdk::Display::default().ok_or_else(|| "GTK has no display".to_string())?;
                // GTK looks for `<search-path>/<theme-name>/index.theme`.
                if let Some(parent) = theme_root.parent() {
                    let icons = gtk::IconTheme::for_display(&display);
                    if !icons.search_path().iter().any(|path| path == parent) {
                        icons.add_search_path(parent);
                    }
                }
                settings.set_gtk_icon_theme_name(Some(THEME_NAME));
            }
        }
        self.inner.style.set(style);
        FLUENT_ACTIVE.store(style == IconStyle::Fluent, Ordering::Relaxed);
        Ok(())
    }
}

/// Write the bundled aliases and an `index.theme` into the cache, and return
/// the theme directory (the search path is its parent). Everything is
/// rewritten on every switch: the pack is small, and this keeps a regenerated
/// icon or a changed desktop theme from leaving a stale cache behind.
fn materialise(inherited_theme: &str) -> Result<PathBuf, String> {
    let root = glib::user_cache_dir().join("lookout").join("fluent-icons");
    let theme = root.join(THEME_NAME);
    let actions = theme.join("scalable").join("actions");
    // Drop the previous materialisation so an alias removed from the manifest
    // cannot linger in the live theme.
    let _ = std::fs::remove_dir_all(&actions);
    std::fs::create_dir_all(&actions).map_err(|error| format!("could not create {}: {error}", actions.display()))?;

    let names = gio::resources_enumerate_children(RESOURCE_ICON_DIR, gio::ResourceLookupFlags::NONE)
        .map_err(|error| format!("Fluent icons are missing from the resource bundle: {error}"))?;
    for name in names {
        let resource = format!("{RESOURCE_ICON_DIR}/{name}");
        let bytes = gio::resources_lookup_data(&resource, gio::ResourceLookupFlags::NONE).map_err(|error| format!("could not read {resource}: {error}"))?;
        let target = actions.join(name.as_str());
        std::fs::write(&target, bytes.as_ref()).map_err(|error| format!("could not write {}: {error}", target.display()))?;
    }

    let index = format!(
        "[Icon Theme]\n\
         Name=Lookout Fluent Icons\n\
         Comment=Microsoft Fluent System Icons, aliased for Lookout (MIT)\n\
         Inherits={inherited_theme},Adwaita,hicolor\n\
         Directories=scalable/actions\n\
         \n\
         [scalable/actions]\n\
         Size=24\n\
         Context=Actions\n\
         Type=Scalable\n\
         MinSize=8\n\
         MaxSize=512\n"
    );
    let index_path = theme.join("index.theme");
    std::fs::write(&index_path, index).map_err(|error| format!("could not write {}: {error}", index_path.display()))?;
    Ok(theme)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_round_trip_and_unknown_values_fall_back_to_gnome() {
        for style in IconStyle::ALL {
            assert_eq!(IconStyle::from_code(style.code()), style);
            assert_eq!(IconStyle::ALL[style.index() as usize], style);
        }
        assert_eq!(IconStyle::from_code(""), IconStyle::Gnome);
        assert_eq!(IconStyle::from_code("Fluent"), IconStyle::Fluent);
        assert_eq!(IconStyle::from_code("something-else"), IconStyle::Gnome);
    }

    #[test]
    fn every_alias_is_symbolic() {
        let manifest = include_str!("../../../data/icons/fluent/manifest.tsv");
        for line in manifest.lines().filter(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#')) {
            let alias = line.split('\t').next().unwrap_or_default();
            assert!(alias.ends_with("-symbolic"), "{alias} must end in -symbolic so GTK recolours it");
        }
    }
}
