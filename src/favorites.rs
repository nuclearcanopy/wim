//! Local-only favorites, for files navidrome doesn't know about.
//! Remote items use navidrome's own star/unstar api instead (see
//! `subsonic::Client::star`/`unstar`); this is purely the fallback so
//! starring still works on local music/video.

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
    favorites_path()
        .ok()
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|raw| toml::from_str(&raw).ok())
        .unwrap_or_default()
}

pub fn save(favorites: &Favorites) -> Result<()> {
    let path = favorites_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let toml = toml::to_string_pretty(favorites)?;
    fs::write(&path, toml).with_context(|| format!("writing {}", path.display()))?;
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
