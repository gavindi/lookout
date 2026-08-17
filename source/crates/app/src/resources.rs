//! Compile-time GResource bundle (`data/resources.gresource.xml` →
//! `$OUT_DIR/gres.bin` by `build.rs`) covering the nav-rail icons, bundled
//! window backgrounds and the app icon. Best-effort: when the build couldn't
//! produce a real bundle (missing `glib-compile-resources`), the bundle isn't
//! registered and callers fall back to their `include_bytes!` constants.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use gtk::gio;
use gtk::glib;

/// The embedded bundle, or an empty marker when `build.rs` couldn't compile
/// one (the marker's first bytes won't match the GResource magic).
static BUNDLE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/gres.bin"));

/// Icon-theme resource path; themed lookups for `io.github.gavindi.Lookout`
/// (window icon, drags, dialogs) resolve against the bundle once this is
/// registered on the display's icon theme.
pub const ICON_RESOURCE_PATH: &str = "/io/github/gavindi/Lookout/icons";

/// Compiled GResources are GVariant databases; their 8-byte file magic is
/// `GVariant` (the literal `GResource` header belongs to the uncompiled XML
/// source format, which never reaches the bundle).
const RESOURCE_MAGIC: &[u8] = b"GVariant";

/// Registers the embedded bundle (when present) and points the display's
/// icon theme at its `icons/` directory. Safe to call more than once.
pub fn register(display: &gtk::gdk::Display) {
    if !is_real_bundle() {
        tracing::warn!("gres.bin is not a compiled GResource; bundled assets fall back to include_bytes! constants");
        return;
    }
    let resource = match gio::Resource::from_data(&glib::Bytes::from_static(BUNDLE)) {
        Ok(resource) => resource,
        Err(err) => {
            tracing::warn!(error = %err, "embedded GResource failed to parse; falling back to include_bytes! constants");
            return;
        }
    };
    gio::resources_register(&resource);
    gtk::IconTheme::for_display(display).add_resource_path(ICON_RESOURCE_PATH);
}

/// Bytes of a bundle resource (e.g. `/io/github/gavindi/Lookout/icons/email-1.svg`).
pub fn bytes(path: &str) -> Option<glib::Bytes> {
    if !is_real_bundle() {
        return None;
    }
    match gio::resources_lookup_data(path, gio::ResourceLookupFlags::NONE) {
        Ok(bytes) => Some(bytes),
        Err(err) => {
            tracing::warn!(path, error = %err, "missing bundled resource");
            None
        }
    }
}

fn is_real_bundle() -> bool {
    BUNDLE.len() >= RESOURCE_MAGIC.len() && &BUNDLE[..RESOURCE_MAGIC.len()] == RESOURCE_MAGIC
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_bundle_parses_when_compiled() {
        // A build without glib-compile-resources embeds an empty marker, and
        // the whole point is that it keeps working - skip then.
        if !is_real_bundle() {
            eprintln!("skipping: gres.bin is an empty marker (glib-compile-resources unavailable at build time)");
            return;
        }
        let resource = gio::Resource::from_data(&glib::Bytes::from_static(BUNDLE)).expect("compiled bundle should parse as a GResource");
        let bytes = resource
            .lookup_data("/io/github/gavindi/Lookout/icons/email-1.svg", gio::ResourceLookupFlags::NONE)
            .expect("nav-rail icon should be in the bundle");
        assert!(!bytes.is_empty());
        // The pop-out/pop-in window buttons' icons ride along in the same
        // bundle, so `themed_icon_name` resolves them as plain icon names.
        for name in ["popin1.svg", "popout1.svg"] {
            let path = format!("/io/github/gavindi/Lookout/icons/{name}");
            let bytes = match resource.lookup_data(&path, gio::ResourceLookupFlags::NONE) {
                Ok(bytes) => bytes,
                Err(err) => panic!("{name} should be in the bundle: {err}"),
            };
            assert!(!bytes.is_empty());
        }
    }
}
