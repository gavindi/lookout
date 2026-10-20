//! Lookout's read-only mirror calendar in EDS, which GNOME Shell's clock and
//! GNOME Calendar show alongside the user's own calendars.
//!
//! The source is a `local` calendar with a `CustomFile`, not a calendar whose
//! data lives in EDS's own directory. That is what makes the copy read-only
//! for other apps (`Writable=false`, which GNOME Calendar and Evolution
//! honour) and what lets Lookout write the whole calendar as one iCalendar
//! file. EDS watches the file and reloads it within a fraction of a second,
//! so E2.3 only has to rewrite it atomically; there are no per-event D-Bus
//! writes. EDS creates no data directory for such a source, so removing it
//! leaves nothing behind but the file, which Lookout deletes itself.
//!
//! See EDS_TODO.md, "E0.1 spike notes", "A read-only mirror is possible,
//! through a custom file".

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use zbus::proxy::CacheProperties;

use crate::keyfile::KeyFile;
use crate::proxies::{SourceManagerProxy, SourceRemovableProxy, SourceWritableProxy};
use crate::sources::managed_sources;
use crate::{Eds, Error, Result};

/// The mirror source's fixed UID. `CreateSources` lets the caller choose it,
/// so the same source is found again after a restart.
pub const MIRROR_SOURCE_UID: &str = "io.github.gavindi.Lookout.mirror";

/// The name GNOME Shell and GNOME Calendar show for the mirror.
pub const MIRROR_DISPLAY_NAME: &str = "Lookout";

/// Lookout's accent blue (`@lookout-unread` in `data/themes/base.css`).
/// EDS stores colours as hex strings, so it can't follow the app's live
/// accent; this is the default accent Lookout uses for unread badges.
pub const MIRROR_COLOR: &str = "#4d9dff";

/// The custom-file key-file dialect version. Bumped only if the source's
/// configuration has to change shape.
const MIRROR_VERSION: &str = "1";

/// The marker group and key that identify a source as Lookout's own. Never
/// write to or remove a source that lacks it, even one whose UID matches.
const MARKER_GROUP: &str = "Lookout";
const MARKER_KEY: &str = "Mirror";

/// Finds or creates Lookout's mirror source, pointing at `ics_file`, and
/// returns its UID.
///
/// `ics_file` must be absolute: EDS may run outside Lookout's Flatpak or Snap
/// sandbox, and a relative path would resolve against its working directory.
/// The file's contents are Lookout's to write (E2.3); this only points EDS at
/// it.
///
/// An existing source is only updated when one of the keys Lookout owns has
/// drifted (a moved data directory, an old display name). A source with the
/// mirror UID but without the `[Lookout] Mirror=true` marker is refused with
/// [`Error::SourceNotMirror`], never overwritten.
pub async fn ensure_mirror_source(eds: &Eds, ics_file: &Path) -> Result<String> {
    if !eds.supports_calendars() {
        return Err(Error::CalendarsUnavailable);
    }
    let expected = mirror_key_file(ics_file)?;
    let sources = managed_sources(eds).await?;
    if let Some(source) = sources.iter().find(|s| s.uid == MIRROR_SOURCE_UID) {
        let file = KeyFile::parse(&source.data).map_err(|_| Error::SourceNotMirror(source.uid.clone()))?;
        if !is_mirror(&file) {
            return Err(Error::SourceNotMirror(source.uid.clone()));
        }
        if needs_update(&file, ics_file) {
            let writable = SourceWritableProxy::builder(eds.connection())
                .path(source.path.clone())?
                .cache_properties(CacheProperties::No)
                .build()
                .await?;
            tracing::info!("EDS: updating the Lookout mirror source {MIRROR_SOURCE_UID}");
            writable.write(&expected).await?;
        }
        return Ok(MIRROR_SOURCE_UID.to_string());
    }

    let manager = SourceManagerProxy::new(eds.connection()).await?;
    manager.create_sources(HashMap::from([(MIRROR_SOURCE_UID, expected.as_str())])).await?;
    tracing::info!("EDS: created the Lookout mirror source {MIRROR_SOURCE_UID}");
    Ok(MIRROR_SOURCE_UID.to_string())
}

/// Removes Lookout's mirror source and deletes the `.ics` file it pointed at.
/// Returns `false` when there is no source with the mirror UID, and refuses a
/// source without the marker with [`Error::SourceNotMirror`].
///
/// EDS deletes the `.source` but leaves a custom file alone, so Lookout
/// removes it here. A file that is already gone is not an error, and a file
/// that can't be deleted is only logged: the source is gone either way.
pub async fn remove_mirror_source(eds: &Eds) -> Result<bool> {
    let sources = managed_sources(eds).await?;
    let Some(source) = sources.iter().find(|s| s.uid == MIRROR_SOURCE_UID) else {
        return Ok(false);
    };
    let file = KeyFile::parse(&source.data).map_err(|_| Error::SourceNotMirror(source.uid.clone()))?;
    if !is_mirror(&file) {
        return Err(Error::SourceNotMirror(source.uid.clone()));
    }

    let removable = SourceRemovableProxy::builder(eds.connection())
        .path(source.path.clone())?
        .cache_properties(CacheProperties::No)
        .build()
        .await?;
    removable.remove().await?;
    tracing::info!("EDS: removed the Lookout mirror source {MIRROR_SOURCE_UID}");

    if let Some(path) = file.get("Local Backend", "CustomFile").and_then(path_from_file_uri) {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!("EDS: couldn't delete the mirror file {}: {e}", path.display()),
        }
    }
    Ok(true)
}

/// The key file for the mirror source. Kept pure and separate from the D-Bus
/// calls so the contract (fixed name, selected calendar, read-only custom
/// file, marker) can be tested without a bus.
fn mirror_key_file(ics_file: &Path) -> Result<String> {
    let uri = file_uri(ics_file).ok_or_else(|| Error::InvalidMirrorPath(ics_file.display().to_string()))?;
    Ok(format!(
        "[Data Source]\n\
         DisplayName={MIRROR_DISPLAY_NAME}\n\
         Enabled=true\n\
         Parent=local-stub\n\
         \n\
         [Calendar]\n\
         BackendName=local\n\
         Color={MIRROR_COLOR}\n\
         Selected=true\n\
         \n\
         [Local Backend]\n\
         CustomFile={}\n\
         Writable=false\n\
         \n\
         [{MARKER_GROUP}]\n\
         Mirror=true\n\
         Version={MIRROR_VERSION}\n",
        escape_value(&uri)
    ))
}

/// Whether the parsed key file carries Lookout's marker.
fn is_mirror(file: &KeyFile) -> bool {
    file.get_bool(MARKER_GROUP, MARKER_KEY) == Some(true)
}

/// Whether any key Lookout owns differs from what [`mirror_key_file`] writes.
/// Unrelated keys EDS added are left alone.
fn needs_update(file: &KeyFile, ics_file: &Path) -> bool {
    let Some(uri) = file_uri(ics_file) else { return true };
    file.get("Data Source", "DisplayName") != Some(MIRROR_DISPLAY_NAME)
        || file.get_bool("Data Source", "Enabled") != Some(true)
        || file.get("Data Source", "Parent") != Some("local-stub")
        || file.get("Calendar", "BackendName") != Some("local")
        || !file.get("Calendar", "Color").is_some_and(|c| c.eq_ignore_ascii_case(MIRROR_COLOR))
        || file.get_bool("Calendar", "Selected") != Some(true)
        || file.get("Local Backend", "CustomFile") != Some(uri.as_str())
        || file.get_bool("Local Backend", "Writable") != Some(false)
        || file.get(MARKER_GROUP, "Version") != Some(MIRROR_VERSION)
}

/// A `file://` URI for `path`, percent-encoding everything outside the
/// unreserved set and `/`. `None` for a relative or non-UTF-8 path. Lookout
/// builds the URI itself, so it only needs to handle plain POSIX paths, not
/// hosts, queries or fragments.
fn file_uri(path: &Path) -> Option<String> {
    if !path.is_absolute() {
        return None;
    }
    let text = path.to_str()?;
    let mut uri = String::from("file://");
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'.' | b'_' | b'~') {
            uri.push(byte as char);
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    Some(uri)
}

/// The inverse of [`file_uri`], for deleting the file a mirror source points
/// at. Returns `None` for anything that isn't a plain, absolute `file://`
/// URI, so a hand-edited source can't make Lookout delete an unrelated path.
fn path_from_file_uri(uri: &str) -> Option<PathBuf> {
    let encoded = uri.strip_prefix("file://")?;
    if !encoded.starts_with('/') {
        return None;
    }
    let bytes = encoded.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            decoded.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(decoded).ok().map(PathBuf::from)
}

/// GKeyFile value escapes. The URI is all printable ASCII after
/// percent-encoding, but the helper keeps the writer honest if a future key
/// carries text.
fn escape_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for (index, c) in value.chars().enumerate() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ' ' if index == 0 => out.push_str("\\s"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_file_is_a_selected_read_only_local_calendar_with_the_marker() {
        let text = mirror_key_file(Path::new("/home/me/.local/share/lookout/eds-mirror.ics")).unwrap();
        let file = KeyFile::parse(&text).unwrap();
        assert_eq!(file.get("Data Source", "DisplayName"), Some("Lookout"));
        assert_eq!(file.get_bool("Data Source", "Enabled"), Some(true));
        assert_eq!(file.get("Data Source", "Parent"), Some("local-stub"));
        assert_eq!(file.get("Calendar", "BackendName"), Some("local"));
        assert_eq!(file.get("Calendar", "Color"), Some(MIRROR_COLOR));
        assert_eq!(file.get_bool("Calendar", "Selected"), Some(true));
        assert_eq!(file.get("Local Backend", "CustomFile"), Some("file:///home/me/.local/share/lookout/eds-mirror.ics"));
        assert_eq!(file.get_bool("Local Backend", "Writable"), Some(false));
        assert!(is_mirror(&file));
    }

    #[test]
    fn only_sources_with_the_marker_count() {
        let marked = KeyFile::parse("[Lookout]\nMirror=true\n").unwrap();
        let unmarked = KeyFile::parse("[Data Source]\nDisplayName=Lookout\n\n[Calendar]\nBackendName=local\n").unwrap();
        let false_marker = KeyFile::parse("[Lookout]\nMirror=false\n").unwrap();
        assert!(is_mirror(&marked));
        assert!(!is_mirror(&unmarked));
        assert!(!is_mirror(&false_marker));
    }

    #[test]
    fn the_file_uri_round_trips_and_escapes_awkward_paths() {
        for path in [
            "/home/me/.local/share/lookout/eds-mirror.ics",
            "/home/me/My Files/lookout mirror.ics",
            "/home/me/Émile/100% done #1.ics",
        ] {
            let uri = file_uri(Path::new(path)).unwrap();
            assert!(uri.starts_with("file:///"));
            assert!(!uri.contains(' '), "{uri} should be percent-encoded");
            assert_eq!(path_from_file_uri(&uri).as_deref(), Some(Path::new(path)));
        }
        assert_eq!(file_uri(Path::new("relative.ics")), None);
    }

    #[test]
    fn only_plain_absolute_file_uris_are_delete_candidates() {
        assert_eq!(path_from_file_uri("file:///tmp/a%20b.ics"), Some(PathBuf::from("/tmp/a b.ics")));
        assert_eq!(path_from_file_uri("file://localhost/tmp/a.ics"), None);
        assert_eq!(path_from_file_uri("https://example.com/a.ics"), None);
        assert_eq!(path_from_file_uri("file:///tmp/bad%zz.ics"), None);
        assert_eq!(path_from_file_uri("file:///tmp/truncated%2"), None);
    }

    #[test]
    fn a_relative_mirror_path_is_refused() {
        assert!(matches!(mirror_key_file(Path::new("eds-mirror.ics")), Err(Error::InvalidMirrorPath(_))));
    }

    #[test]
    fn drift_in_a_key_lookout_owns_needs_an_update() {
        let path = Path::new("/home/me/eds-mirror.ics");
        let current = KeyFile::parse(&mirror_key_file(path).unwrap()).unwrap();
        assert!(!needs_update(&current, path));

        let renamed = KeyFile::parse(
            "[Data Source]\nDisplayName=Old\nEnabled=true\nParent=local-stub\n\n[Calendar]\nBackendName=local\nColor=#4d9dff\nSelected=true\n\n[Local Backend]\nCustomFile=file:///home/me/eds-mirror.ics\nWritable=false\n\n[Lookout]\nMirror=true\nVersion=1\n",
        )
        .unwrap();
        assert!(needs_update(&renamed, path));

        let moved = KeyFile::parse(&mirror_key_file(path).unwrap()).unwrap();
        assert!(needs_update(&moved, Path::new("/home/me/other.ics")));
    }
}
