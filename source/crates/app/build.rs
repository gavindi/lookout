//! Compiles the GSettings schema (`data/gschema/*.gschema.xml`) into
//! `$OUT_DIR/gschemas.compiled`, where `settings.rs` picks it up at runtime
//! when the schema isn't installed system-wide. Best-effort: a missing
//! `glib-compile-schemas` binary only costs the compiled bundle, and the
//! settings store falls back to session-only state.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    let schema_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR")).join("../../data/gschema");
    println!("cargo:rerun-if-changed={}", schema_dir.display());

    let out_dir = std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR");
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
}
