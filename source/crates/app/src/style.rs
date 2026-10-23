//! The app's static GTK stylesheet.
//!
//! Every rule that is fixed at compile time lives in `source/data/styles/`,
//! split by the component it belongs to. `include_str!` bundles them (the
//! same convention `theme.rs` uses for its bundled themes) and [`install`]
//! registers the concatenation once, at APPLICATION priority, so the app has
//! one static provider instead of one per view.
//!
//! CSS that has to be generated at runtime stays with its owner:
//! `ThemeManager` (named colours and the accent override), `PrivacyMode`
//! (the blur toggle), `folder_menu::palette_css` (the folder palette, which
//! is appended here) and the calendar sidebar's per-colour chip rules.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::sync::Once;

/// The concatenated static rules. Order within the sheet is irrelevant (no
/// selector is defined twice), so files are grouped by component for
/// readability. Kept in source order roughly from the window shell outwards.
const APP_CSS: &str = concat!(
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../data/styles/shell.css")),
    "\n",
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../data/styles/folders.css")),
    "\n",
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../data/styles/message-header.css")),
    "\n",
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../data/styles/message-list.css")),
    "\n",
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../data/styles/recipients.css")),
    "\n",
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../data/styles/ribbon.css")),
    "\n",
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../data/styles/file-page.css")),
    "\n",
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../data/styles/density.css")),
    "\n",
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../data/styles/search.css")),
    "\n",
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../data/styles/calendar.css")),
    "\n",
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../data/styles/tasks.css")),
    "\n",
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../data/styles/lookout.css")),
);

/// Registers the static app stylesheet on the default display, once per
/// process. Safe to call from every view builder: the first call installs,
/// later ones are no-ops.
pub(crate) fn install() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let mut css = String::with_capacity(APP_CSS.len() + 2048);
        css.push_str(APP_CSS);
        // The folder palette is generated from `CALENDAR_PALETTE`, so it is
        // the one static-looking block that can't be an `include_str!` file.
        css.push_str(&crate::folder_menu::palette_css());

        let provider = gtk::CssProvider::new();
        provider.load_from_string(&css);
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::style_context_add_provider_for_display(&display, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
        }
    });
}
