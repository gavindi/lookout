//! Compiles the GSettings schema (`data/gschema/*.gschema.xml`) into
//! `$OUT_DIR/gschemas.compiled`, where `settings.rs` picks it up at runtime
//! when the schema isn't installed system-wide. Best-effort: a missing
//! `glib-compile-schemas` binary only costs the compiled bundle, and the
//! settings store falls back to session-only state.
//!
//! Also compiles the GResource bundle (`data/resources.gresource.xml`:
//! nav-rail icons, bundled backgrounds, app icon) into `$OUT_DIR/gres.bin`.
//! Best-effort too: on failure an empty marker file is written, and
//! `resources.rs` falls back to the `include_bytes!` constants in
//! `window.rs` when the registered bundle isn't a real GResource.

use std::fs::File;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"));
    let data_dir = manifest_dir.join("../../data");
    let out_dir = std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR");

    let schema_dir = data_dir.join("gschema");
    println!("cargo:rerun-if-changed={}", schema_dir.display());
    match Command::new("glib-compile-schemas").arg("--targetdir").arg(&out_dir).arg(&schema_dir).output() {
        // Success is only real success when stderr is empty too:
        // glib-compile-schemas exits 0 even when it ignores a malformed file
        // with an error on stderr (e.g. an enum used before it's defined),
        // which would leave an empty bundle and silently disable GSettings.
        Ok(output) if output.status.success() && output.stderr.is_empty() => {}
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            println!("cargo:warning=glib-compile-schemas failed: {stderr}");
        }
        Err(e) => {
            println!("cargo:warning=glib-compile-schemas unavailable ({e}); GSettings will fall back to session-only state");
        }
    }

    let gresource_xml = data_dir.join("resources.gresource.xml");
    println!("cargo:rerun-if-changed={}", gresource_xml.display());
    println!("cargo:rerun-if-changed={}", data_dir.join("resources").display());
    println!("cargo:rerun-if-changed={}", data_dir.join("themes").display());
    println!("cargo:rerun-if-changed={}", data_dir.join("icons/hicolor/scalable/apps").display());
    let gres_bin = PathBuf::from(&out_dir).join("gres.bin");
    match Command::new("glib-compile-resources")
        .arg("--sourcedir")
        .arg(&data_dir)
        .arg("--target")
        .arg(&gres_bin)
        .arg(&gresource_xml)
        .output()
    {
        Ok(output) if output.status.success() => {}
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            println!("cargo:warning=glib-compile-resources failed: {stderr}");
            let _ = File::create(&gres_bin);
        }
        Err(e) => {
            println!("cargo:warning=glib-compile-resources unavailable ({e}); bundled assets will fall back to include_bytes! constants");
            let _ = File::create(&gres_bin);
        }
    }
}
