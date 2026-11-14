//! Crash-safe file replacement for user data.
//!
//! `fs::write` truncates in place, so a kill or power loss mid-write left
//! an empty or partial file (a lost queue, favorites or config). These
//! write a private sibling, sync it, rename over the target and sync the
//! parent, so a reader sees the old file or the new one, never half.

use anyhow::{Context, Result};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

/// Replaces `path` with `contents` atomically, mode 0600 from creation.
/// A symlinked target is followed, so a link is not silently replaced by
/// a regular file.
pub fn atomic_write(path: &Path, contents: &[u8]) -> Result<()> {
    let target = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let dir = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temp = dir.join(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| -> Result<()> {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temp)
            .with_context(|| format!("writing {}", target.display()))?;
        file.write_all(contents)?;
        file.sync_all()?;
        fs::rename(&temp, &target).with_context(|| format!("replacing {}", target.display()))?;
        // the new file is already in place; a filesystem that cannot
        // fsync a directory must not turn that into a reported failure
        #[cfg(unix)]
        if let Ok(handle) = fs::File::open(&dir) {
            let _ = handle.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// Moves an unreadable file aside as `<name>.corrupt` so the next save
/// does not overwrite the only copy of what the user had. Best effort.
pub fn quarantine(path: &Path) {
    let mut aside = path.as_os_str().to_owned();
    aside.push(".corrupt");
    let _ = fs::rename(path, PathBuf::from(aside));
}

#[cfg(test)]
mod atomic_write_tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wim-fsutil-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn replaces_contents_and_leaves_no_temp_file() {
        let dir = scratch("replace");
        let file = dir.join("a.toml");
        atomic_write(&file, b"one").unwrap();
        atomic_write(&file, b"two").unwrap();
        assert_eq!(fs::read(&file).unwrap(), b"two");
        let names: Vec<_> = fs::read_dir(&dir).unwrap().flatten().collect();
        assert_eq!(names.len(), 1, "only the target remains");
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn creates_owner_only_and_follows_symlinks() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let dir = scratch("link");
        let real = dir.join("real.toml");
        let link = dir.join("link.toml");
        atomic_write(&real, b"x").unwrap();
        symlink(&real, &link).unwrap();
        atomic_write(&link, b"y").unwrap();
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read(&real).unwrap(), b"y");
        assert_eq!(
            fs::metadata(&real).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn quarantine_moves_the_file_aside() {
        let dir = scratch("quarantine");
        let file = dir.join("f.toml");
        fs::write(&file, "garbage").unwrap();
        quarantine(&file);
        assert!(!file.exists());
        assert_eq!(fs::read(dir.join("f.toml.corrupt")).unwrap(), b"garbage");
        let _ = fs::remove_dir_all(&dir);
    }
}
