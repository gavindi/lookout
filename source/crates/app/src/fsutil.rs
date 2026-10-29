//! Atomic file writes and preservation of unreadable configuration files.
//!
//! The app's JSON config files are rewritten on every settings change, and a
//! half-written file from a crash must never be the only copy left. Writes go
//! to a uniquely named sibling and are renamed into place once complete, so a
//! concurrent reader (or a crash) sees either the old file or the new one.
//! `backup_corrupt` is the read-side companion: an unparseable file is moved
//! aside rather than silently replaced by defaults on the next save.

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
/// permissions, so a token file written at `0o600` stays private across saves
/// while a plain config keeps whatever mode the user already has.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    let dir = path.parent().filter(|dir| !dir.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir)?;
    let file_name = path.file_name().and_then(|name| name.to_str()).unwrap_or("file");
    let effective_mode = fs::metadata(path).map(|metadata| permission_bits(&metadata)).unwrap_or(mode);

    let mut last_collision = None;
    for _ in 0..8 {
        let tmp = dir.join(format!(".{file_name}.tmp-{}-{}", std::process::id(), TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        set_mode(&mut options, effective_mode);
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
pub(crate) fn backup_corrupt(path: &Path) -> io::Result<PathBuf> {
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
