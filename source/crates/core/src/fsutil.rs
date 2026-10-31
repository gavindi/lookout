//! Atomic file writes and preservation of unreadable configuration files.
//!
//! Every file Lookout persists outside SQLite (settings, OAuth tokens, the
//! mail cache's attachment and `.eml` files, the EDS mirror, the autostart
//! entry) goes through `write_atomic` or `write_private`, and a half-written
//! file from a crash must never be the only copy left. Writes go to a
//! uniquely named hidden sibling and are renamed into place once complete,
//! so a concurrent reader (or a crash) sees either the old file or the new
//! one. `backup_corrupt` is the read-side companion: an unparseable file is
//! moved aside rather than silently replaced by defaults on the next save.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Makes each temporary file name unique within this process. Combined with
/// the pid it also survives a stale file left behind by an earlier run with
/// the same pid.
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Writes `bytes` to `path` atomically, creating the parent directory if
/// needed.
///
/// `mode` is the permission bits for a *new* file; an existing file keeps its
/// permissions, so a config written at `0o600` stays private across saves
/// while one the user has opened up keeps whatever mode they gave it. Use
/// `write_private` for a secret, which must be `0o600` whatever is on disk.
pub fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    let effective_mode = fs::metadata(path).map(|metadata| permission_bits(&metadata)).unwrap_or(mode);
    write_with_mode(path, bytes, effective_mode)
}

/// Writes `bytes` to `path` atomically as an owner-only (`0o600`) file,
/// replacing the mode of any existing file. The temporary file is created
/// with that mode, so the bytes are never readable by anyone else, not even
/// for the moment between the write and a later `chmod`.
pub fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write_with_mode(path, bytes, 0o600)
}

fn write_with_mode(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    let dir = path.parent().filter(|dir| !dir.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir)?;
    let file_name = path.file_name().and_then(|name| name.to_str()).unwrap_or("file");

    let mut last_collision = None;
    for _ in 0..8 {
        let tmp = dir.join(temp_name(file_name));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        set_mode(&mut options, mode);
        let mut file = match options.open(&tmp) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                last_collision = Some(e);
                continue;
            }
            Err(e) => return Err(e),
        };
        let result = write_and_rename(&mut file, &tmp, path, bytes);
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        return result;
    }
    Err(last_collision.unwrap_or_else(|| io::Error::new(io::ErrorKind::AlreadyExists, "no free temporary file name")))
}

/// The temporary sibling's name: hidden, and ending in the pid and counter
/// rather than the target's extension, so a crash mid-write never leaves a
/// file a directory scanner takes for a real `.bin`, `.eml` or `.desktop`.
fn temp_name(file_name: &str) -> String {
    format!(".{file_name}.tmp-{}-{}", std::process::id(), TEMP_COUNTER.fetch_add(1, Ordering::Relaxed))
}

fn write_and_rename(file: &mut File, tmp: &Path, path: &Path, bytes: &[u8]) -> io::Result<()> {
    file.write_all(bytes)?;
    // Durability matters here: the rename below is only safe to observe after
    // the contents are on disk, or a crash can leave a complete-looking but
    // empty file.
    file.sync_all()?;
    fs::rename(tmp, path)
}

#[cfg(unix)]
fn permission_bits(metadata: &fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o777
}

#[cfg(not(unix))]
fn permission_bits(_metadata: &fs::Metadata) -> u32 {
    // Non-Unix has no unix mode to preserve; the caller's default is used.
    0
}

#[cfg(unix)]
fn set_mode(options: &mut OpenOptions, mode: u32) {
    use std::os::unix::fs::OpenOptionsExt;
    options.mode(mode & 0o777);
}

#[cfg(not(unix))]
fn set_mode(_options: &mut OpenOptions, _mode: u32) {}

/// Moves an unreadable file aside as `<name>.corrupt-<timestamp>` and returns
/// the backup path. The original path is left free, so the next save writes a
/// fresh file instead of destroying the only copy of the user's data.
pub fn backup_corrupt(path: &Path) -> io::Result<PathBuf> {
    let file_name = path.file_name().and_then(|name| name.to_str()).unwrap_or("file");
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S%.3fZ");
    for attempt in 0..100u32 {
        let name = if attempt == 0 {
            format!("{file_name}.corrupt-{stamp}")
        } else {
            format!("{file_name}.corrupt-{stamp}-{attempt}")
        };
        let target = path.with_file_name(name);
        if target.exists() {
            continue;
        }
        return fs::rename(path, &target).map(|()| target);
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "no free corrupt-backup name"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lookout-fsutil-{tag}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn write_atomic_replaces_the_target_and_leaves_no_temporary_file() {
        let dir = temp_dir("write");
        let path = dir.join("settings.json");
        write_atomic(&path, b"{\"one\":1}", 0o600).unwrap();
        write_atomic(&path, b"{\"two\":2}", 0o600).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "{\"two\":2}");
        let leftovers: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "temporary files left behind: {leftovers:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_writers_never_collide_or_truncate() {
        let dir = temp_dir("concurrent");
        let path = dir.join("settings.json");
        let writers: Vec<_> = (0..8)
            .map(|i| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let body = format!("{{\"writer\":{i},\"padding\":\"{}\"}}", "x".repeat(4096));
                    for _ in 0..20 {
                        write_atomic(&path, body.as_bytes(), 0o600).unwrap();
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().unwrap();
        }
        // The last writer to finish always leaves a complete JSON document.
        let text = fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&text).expect("the surviving file is complete JSON");
        assert!(parsed.get("writer").is_some());
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[cfg(unix)]
    fn set_mode_of(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_uses_the_mode_for_a_new_file_and_keeps_an_existing_files_mode() {
        let dir = temp_dir("mode");
        let fresh = dir.join("fresh.json");
        write_atomic(&fresh, b"{}", 0o600).unwrap();
        assert_eq!(mode_of(&fresh), 0o600);

        let opened_up = dir.join("opened-up.json");
        fs::write(&opened_up, b"{}").unwrap();
        set_mode_of(&opened_up, 0o644);
        write_atomic(&opened_up, b"{\"x\":1}", 0o600).unwrap();
        assert_eq!(mode_of(&opened_up), 0o644);
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn write_private_makes_an_existing_readable_file_owner_only() {
        let dir = temp_dir("private");
        let path = dir.join("token.json");
        fs::write(&path, b"old").unwrap();
        set_mode_of(&path, 0o644);
        write_private(&path, b"new").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert_eq!(mode_of(&path), 0o600);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn temp_names_are_hidden_unique_and_never_end_in_the_targets_extension() {
        let first = temp_name("1234-1-7-2.bin");
        let second = temp_name("1234-1-7-2.bin");
        assert_ne!(first, second);
        for name in [&first, &second] {
            assert!(name.starts_with(".1234-1-7-2.bin.tmp-"), "{name}");
            assert!(!name.ends_with(".bin"), "{name}");
        }
    }

    #[test]
    fn a_failed_write_leaves_the_target_and_no_temporary_file() {
        let dir = temp_dir("failed");
        let path = dir.join("settings.json");
        write_atomic(&path, b"kept", 0o600).unwrap();
        // A directory where the target should be makes the final rename fail
        // after the temporary file was written.
        let blocked = dir.join("blocked");
        fs::create_dir_all(blocked.join("child")).unwrap();
        assert!(write_atomic(&blocked, b"lost", 0o600).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"kept");
        let leftovers: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "temporary files left behind: {leftovers:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn backup_corrupt_preserves_the_original_bytes_and_frees_the_path() {
        let dir = temp_dir("backup");
        let path = dir.join("settings.json");
        fs::write(&path, b"not json").unwrap();
        let backup = backup_corrupt(&path).unwrap();
        assert_eq!(fs::read(&backup).unwrap(), b"not json");
        assert!(!path.exists());
        assert!(backup.file_name().unwrap().to_string_lossy().starts_with("settings.json.corrupt-"));
        let _ = fs::remove_dir_all(&dir);
    }
}
