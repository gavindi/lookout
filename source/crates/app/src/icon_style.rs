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
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use gtk::{gio, glib};

/// The generated theme's directory name and `gtk-icon-theme-name` value.
pub(crate) const THEME_NAME: &str = "Lookout-Fluent";

/// The bundled aliases, under the GResource prefix registered by
/// [`crate::resources::register`].
const RESOURCE_ICON_DIR: &str = "/io/github/gavindi/Lookout/icons/Lookout-Fluent/scalable/actions";

/// Whether Fluent glyphs are in force. Used by the two-state icons whose
/// artwork is not symbolic (`window::support::pin_icon_name`).
static FLUENT_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Names the staging and retired directories of a theme swap uniquely within
/// this process (the pid separates processes), as `fsutil::write_atomic` does.
static SWAP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The prefix of a swap's staging (`tmp-`) and retired (`old-`) siblings of
/// the theme directory. Hidden, so GTK never takes one for a theme.
const SWAP_PREFIX: &str = ".Lookout-Fluent.";

/// A staging or retired sibling older than this is a crash's leftover (a swap
/// takes milliseconds), so it is safe to delete even if another Lookout
/// instance shares the cache.
const LEFTOVER_MAX_AGE: Duration = Duration::from_secs(10 * 60);

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
/// the theme directory (the search path is its parent). The whole pack is
/// rebuilt on every switch - it is small, and this keeps a regenerated icon,
/// a removed alias or a changed desktop theme from leaving a stale cache
/// behind - but never in place: see [`write_theme`].
fn materialise(inherited_theme: &str) -> Result<PathBuf, String> {
    let names = gio::resources_enumerate_children(RESOURCE_ICON_DIR, gio::ResourceLookupFlags::NONE)
        .map_err(|error| format!("Fluent icons are missing from the resource bundle: {error}"))?;
    let mut icons = Vec::with_capacity(names.len());
    for name in names {
        let resource = format!("{RESOURCE_ICON_DIR}/{name}");
        let bytes = gio::resources_lookup_data(&resource, gio::ResourceLookupFlags::NONE).map_err(|error| format!("could not read {resource}: {error}"))?;
        icons.push((name.to_string(), bytes));
    }
    write_theme(&glib::user_cache_dir().join("lookout").join("fluent-icons"), inherited_theme, &icons)
}

/// Builds the theme in a hidden sibling of `root/Lookout-Fluent`, then
/// renames it into place (retiring the previous copy the same way), so GTK -
/// in this process or in another running Lookout - only ever scans a
/// complete theme, never a half-deleted or half-written one. If the swap
/// fails, the previous theme is put back.
fn write_theme(root: &Path, inherited_theme: &str, icons: &[(String, glib::Bytes)]) -> Result<PathBuf, String> {
    std::fs::create_dir_all(root).map_err(|error| format!("could not create {}: {error}", root.display()))?;
    sweep_leftovers(root, LEFTOVER_MAX_AGE);

    let suffix = format!("{}-{}", std::process::id(), SWAP_COUNTER.fetch_add(1, Ordering::Relaxed));
    let staging = root.join(format!("{SWAP_PREFIX}tmp-{suffix}"));
    if let Err(error) = build_theme(&staging, inherited_theme, icons) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error);
    }

    let theme = root.join(THEME_NAME);
    let retired = root.join(format!("{SWAP_PREFIX}old-{suffix}"));
    let had_previous = match std::fs::rename(&theme, &retired) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(format!("could not retire {}: {error}", theme.display()));
        }
    };
    if let Err(error) = std::fs::rename(&staging, &theme) {
        if had_previous {
            let _ = std::fs::rename(&retired, &theme);
        }
        let _ = std::fs::remove_dir_all(&staging);
        return Err(format!("could not install {}: {error}", theme.display()));
    }
    if had_previous {
        let _ = std::fs::remove_dir_all(&retired);
    }
    Ok(theme)
}

/// Writes a complete theme (`scalable/actions/*` plus `index.theme`) into
/// the empty directory `dir`.
fn build_theme(dir: &Path, inherited_theme: &str, icons: &[(String, glib::Bytes)]) -> Result<(), String> {
    let actions = dir.join("scalable").join("actions");
    std::fs::create_dir_all(&actions).map_err(|error| format!("could not create {}: {error}", actions.display()))?;
    for (name, bytes) in icons {
        let target = actions.join(name);
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
    let index_path = dir.join("index.theme");
    std::fs::write(&index_path, index).map_err(|error| format!("could not write {}: {error}", index_path.display()))
}

/// Deletes the staging and retired siblings a crashed swap left in `root`,
/// once they are older than `max_age`. Best-effort.
fn sweep_leftovers(root: &Path, max_age: Duration) {
    let Ok(entries) = std::fs::read_dir(root) else { return };
    for entry in entries.flatten() {
        if !entry.file_name().to_string_lossy().starts_with(SWAP_PREFIX) {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age >= max_age);
        if stale {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
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

    fn icons(names: &[&str]) -> Vec<(String, glib::Bytes)> {
        names.iter().map(|name| (name.to_string(), glib::Bytes::from(name.as_bytes()))).collect()
    }

    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn write_theme_replaces_the_pack_and_leaves_no_staging_dirs() {
        let root = std::env::temp_dir().join(format!("lookout-icon-style-test-{}", std::process::id())).join("replace");
        let _ = std::fs::remove_dir_all(&root);

        let theme = write_theme(&root, "Yaru", &icons(&["a-symbolic.svg", "b-symbolic.svg"])).expect("first write");
        assert_eq!(theme, root.join(THEME_NAME));
        assert_eq!(entries(&theme.join("scalable").join("actions")), ["a-symbolic.svg", "b-symbolic.svg"]);

        let theme = write_theme(&root, "Adwaita-dark", &icons(&["a-symbolic.svg"])).expect("second write");
        assert_eq!(entries(&theme.join("scalable").join("actions")), ["a-symbolic.svg"], "a dropped alias must not linger");
        let index = std::fs::read_to_string(theme.join("index.theme")).unwrap();
        assert!(index.contains("Inherits=Adwaita-dark,Adwaita,hicolor"), "{index}");
        assert_eq!(entries(&root), [THEME_NAME], "no staging or retired directory is left behind");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn write_theme_sweeps_a_crashed_swaps_leftovers() {
        let root = std::env::temp_dir().join(format!("lookout-icon-style-test-{}", std::process::id())).join("sweep");
        let _ = std::fs::remove_dir_all(&root);
        let leftover = root.join(format!("{SWAP_PREFIX}tmp-1-0")).join("scalable");
        std::fs::create_dir_all(&leftover).unwrap();

        // Fresh leftovers may belong to another instance mid-swap: kept.
        sweep_leftovers(&root, LEFTOVER_MAX_AGE);
        assert_eq!(entries(&root), [format!("{SWAP_PREFIX}tmp-1-0")]);
        // Old enough ones are a crash's, and go.
        sweep_leftovers(&root, Duration::ZERO);
        assert!(entries(&root).is_empty());
        let _ = std::fs::remove_dir_all(&root);
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
