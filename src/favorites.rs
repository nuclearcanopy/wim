//! Local-only favorites, for files navidrome doesn't know about.
//! Remote items use navidrome's own star/unstar api instead (see
//! `subsonic::Client::star`/`unstar`); this is purely the fallback so
//! starring still works on local music/video.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// **The field name is the on-disk format.** `load` cannot tell a
/// renamed field from a first launch (a missing key reads as an empty
/// set), so renaming `paths` would drop every local star on every
/// install with nothing said; pinned by
/// `a_favorites_file_written_by_an_older_wim_still_loads` against a
/// literal fixture, the same way `persistence::PlaybackState` is.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Favorites {
    paths: HashSet<String>,
}

impl Favorites {
    pub fn is_starred(&self, path: &str) -> bool {
        self.paths.contains(path)
    }

    /// Returns the new state (true = now starred).
    pub fn toggle(&mut self, path: &str) -> bool {
        if self.paths.remove(path) {
            false
        } else {
            self.paths.insert(path.to_string());
            true
        }
    }

    pub fn paths(&self) -> impl Iterator<Item = &String> {
        self.paths.iter()
    }
}

fn favorites_path() -> Result<PathBuf> {
    let dir = dirs::state_dir()
        .or_else(dirs::data_dir)
        .context("no state or data dir found for this platform")?;
    Ok(dir.join("wim").join("favorites.toml"))
}

/// Best-effort load; a missing or corrupt file just means no
/// favorites yet, not an error worth surfacing to the user.
pub fn load() -> Favorites {
    favorites_path().map(|p| load_from(&p)).unwrap_or_default()
}

pub fn save(favorites: &Favorites) -> Result<()> {
    save_to(&favorites_path()?, favorites)
}

/// `load` against an explicit path, so a test never reads the real
/// `~/.local/state/wim/favorites.toml`; same split as
/// `persistence::load_from` and `library::save_or_warn_to`.
fn load_from(path: &Path) -> Favorites {
    fs::read_to_string(path)
        .ok()
        .and_then(|raw| toml::from_str(&raw).ok())
        .unwrap_or_default()
}

fn save_to(path: &Path, favorites: &Favorites) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let toml = toml::to_string_pretty(favorites)?;
    fs::write(path, toml).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggle_flips_and_reports_the_new_state() {
        let mut f = Favorites::default();
        assert!(f.toggle("/music/a.flac"));
        assert!(f.is_starred("/music/a.flac"));
        assert!(!f.toggle("/music/a.flac"));
        assert!(!f.is_starred("/music/a.flac"));
    }
}

/// The on-disk half, which had no coverage at all: `save`/`load` were
/// untested and `load` swallows every error, so the shape was only
/// ever verified by a user noticing their stars had gone.
#[cfg(test)]
mod on_disk_format {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Its own temp dir per test; nothing here touches the real
    /// `~/.local/state/wim`. Same convention as
    /// `persistence::tests::scratch_dir`.
    fn scratch_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("wim-favorites-{label}-{nanos}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn two_stars() -> Favorites {
        let mut f = Favorites::default();
        f.toggle("/music/Bladee/Crest/01 - Wiking.flac");
        f.toggle("/videos/Blade Runner 2049.mkv");
        f
    }

    #[test]
    fn stars_survive_a_round_trip_through_the_disk() {
        let dir = scratch_dir("roundtrip");
        let path = dir.join("favorites.toml");
        let stars = two_stars();
        save_to(&path, &stars).unwrap();

        let back = load_from(&path);
        assert_eq!(back, stars, "every starred path has to come back");
        assert!(back.is_starred("/videos/Blade Runner 2049.mkv"));
        fs::remove_dir_all(&dir).ok();
    }

    /// The real guard on the serde shape: the literal bytes an older
    /// wim wrote. Rename the field and `load` reads an empty set, so
    /// every local star is gone on the next launch and the first
    /// `save` writes the loss back.
    #[test]
    fn a_favorites_file_written_by_an_older_wim_still_loads() {
        let dir = scratch_dir("fixture");
        let path = dir.join("favorites.toml");
        fs::write(
            &path,
            "paths = [\"/music/Bladee/Crest/01 - Wiking.flac\", \
             \"/videos/Blade Runner 2049.mkv\"]\n",
        )
        .unwrap();

        assert_eq!(
            load_from(&path),
            two_stars(),
            "the saved shape must still load"
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn nothing_saved_yet_is_no_favorites_rather_than_a_failure() {
        let dir = scratch_dir("absent");
        assert_eq!(load_from(&dir.join("favorites.toml")), Favorites::default());
        fs::remove_dir_all(&dir).ok();
    }

    /// `save` reports what it could not write, which is the half the
    /// caller at `core.rs` ~660 currently discards (see the report in
    /// ROADMAP.md's audit): a star stays lit all session over a write
    /// that never happened.
    #[test]
    fn a_write_that_cannot_happen_is_an_error_not_a_silent_no_op() {
        let dir = scratch_dir("unwritable");
        let blocker = dir.join("favorites.toml");
        // a directory where the file goes: `fs::write` cannot replace
        // it, the same shape as a read-only state dir.
        fs::create_dir_all(&blocker).unwrap();
        let err = save_to(&blocker, &two_stars()).expect_err("writing onto a directory fails");
        assert!(
            err.to_string().contains("favorites.toml"),
            "the message has to name the file: {err}"
        );
        fs::remove_dir_all(&dir).ok();
    }
}
