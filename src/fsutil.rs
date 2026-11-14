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
    write_replacing(&target, contents, 0o600)
}

/// For a file the user is meant to read from outside wim (a subtitle
/// sidecar beside a video): mode 0644, and a symlink already sitting at
/// `path` is *replaced* by the new regular file rather than written
/// through, so a link planted in a shared media directory cannot aim the
/// write at another file.
pub fn atomic_write_beside(path: &Path, contents: &[u8]) -> Result<()> {
    write_replacing(path, contents, 0o644)
}

fn write_replacing(target: &Path, contents: &[u8], mode: u32) -> Result<()> {
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
            options.mode(mode);
        }
        #[cfg(not(unix))]
        let _ = mode;
        let mut file = options
            .open(&temp)
            .with_context(|| format!("writing {}", target.display()))?;
        file.write_all(contents)?;
        file.sync_all()?;
        fs::rename(&temp, target).with_context(|| format!("replacing {}", target.display()))?;
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

/// An exclusive advisory lock on `<path>.lock`, released on drop. Taken
/// with a bounded wait: a holder that died took its lock with it (flock
/// goes when the process does), and one that is merely slow must not hang
/// a save forever, so after the wait the caller proceeds unlocked.
struct FileLock(#[allow(dead_code)] Option<fs::File>);

impl FileLock {
    fn acquire(path: &Path) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            use std::os::unix::io::AsRawFd;
            let mut lock = path.as_os_str().to_owned();
            lock.push(".lock");
            let Ok(file) = fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .mode(0o600)
                .open(PathBuf::from(lock))
            else {
                return FileLock(None);
            };
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            loop {
                // SAFETY: flock on a descriptor we own.
                if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                    return FileLock(Some(file));
                }
                if std::time::Instant::now() >= deadline {
                    return FileLock(None);
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            FileLock(None)
        }
    }
}

/// Read-merge-write of a shared file. Under an exclusive lock, `update` is
/// given what is on disk *right now* (`None` when the file is absent) and
/// returns the bytes to store, which are written atomically. Two wims that
/// each hold a stale in-memory copy therefore combine their changes
/// instead of the later save erasing the earlier one: `update` is where a
/// store applies only *its own* changes to what the other one wrote.
pub fn locked_update(
    path: &Path,
    update: impl FnOnce(Option<Vec<u8>>) -> Result<Vec<u8>>,
) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let _lock = FileLock::acquire(path);
    let current = fs::read(path).ok();
    let next = update(current)?;
    atomic_write(path, &next)
}

/// A file written by a *newer* wim than this one understands: keep a copy
/// beside it (`<name>.v<N>.bak`, once) before this wim's own format
/// replaces it, so downgrading and upgrading again loses nothing.
pub fn keep_newer_copy(path: &Path, found: u32) {
    let mut backup = path.as_os_str().to_owned();
    backup.push(format!(".v{found}.bak"));
    let backup = PathBuf::from(backup);
    if !backup.exists() {
        let _ = fs::copy(path, backup);
    }
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

    #[cfg(unix)]
    #[test]
    fn beside_replaces_a_planted_symlink_instead_of_writing_through_it() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let dir = scratch("beside");
        let victim = dir.join("victim.txt");
        fs::write(&victim, "precious").unwrap();
        let sidecar = dir.join("movie.srt");
        symlink(&victim, &sidecar).unwrap();
        atomic_write_beside(&sidecar, b"1\n00:00:01,000 --> 00:00:02,000\nhi\n").unwrap();
        assert_eq!(fs::read_to_string(&victim).unwrap(), "precious");
        assert!(!fs::symlink_metadata(&sidecar)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            fs::metadata(&sidecar).unwrap().permissions().mode() & 0o777,
            0o644
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_locked_updates_lose_nothing() {
        let dir = scratch("locked");
        let file = dir.join("counter.txt");
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let file = file.clone();
                std::thread::spawn(move || {
                    for _ in 0..25 {
                        locked_update(&file, |current| {
                            let n: u32 = current
                                .and_then(|b| String::from_utf8(b).ok())
                                .and_then(|s| s.trim().parse().ok())
                                .unwrap_or(0);
                            Ok((n + 1).to_string().into_bytes())
                        })
                        .unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(fs::read_to_string(&file).unwrap().trim(), "200");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_newer_copy_is_kept_once() {
        let dir = scratch("newer");
        let file = dir.join("state.toml");
        fs::write(&file, "version = 9\n").unwrap();
        keep_newer_copy(&file, 9);
        fs::write(&file, "version = 1\n").unwrap();
        keep_newer_copy(&file, 9);
        assert_eq!(
            fs::read_to_string(dir.join("state.toml.v9.bak")).unwrap(),
            "version = 9\n"
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

/// Result of `claim_instance_lock`.
pub enum InstanceLock {
    /// We hold it for as long as the file lives.
    Held(fs::File),
    /// Another wim already holds it.
    Contended,
    /// Locking is unavailable here; carry on without a guarantee.
    Unavailable,
}

/// Takes an advisory exclusive lock on `<dir>/wim.lock`, released when the
/// returned file is dropped (or the process dies). Two wims writing the
/// same state files lose each other's updates; this cannot make that safe,
/// but it lets the second one say so instead of failing silently.
pub fn claim_instance_lock(dir: &Path) -> InstanceLock {
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        if fs::create_dir_all(dir).is_err() {
            return InstanceLock::Unavailable;
        }
        let Ok(file) = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join("wim.lock"))
        else {
            return InstanceLock::Unavailable;
        };
        // SAFETY: flock on a file descriptor we own.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc == 0 {
            return InstanceLock::Held(file);
        }
        if std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock {
            InstanceLock::Contended
        } else {
            InstanceLock::Unavailable
        }
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        InstanceLock::Unavailable
    }
}

#[cfg(all(test, unix))]
mod instance_lock_tests {
    use super::*;

    #[test]
    fn a_second_claim_is_contended_until_the_first_is_dropped() {
        let dir = std::env::temp_dir().join(format!("wim-lock-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let first = claim_instance_lock(&dir);
        assert!(matches!(first, InstanceLock::Held(_)));
        assert!(matches!(claim_instance_lock(&dir), InstanceLock::Contended));
        drop(first);
        assert!(matches!(claim_instance_lock(&dir), InstanceLock::Held(_)));
        let _ = fs::remove_dir_all(&dir);
    }
}
