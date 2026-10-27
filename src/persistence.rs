//! Resuming playback (queue, position, volume) across restarts is
//! standard in podcast/audiobook apps and most modern music players;
//! not optional polish. State is saved as plain paths; resuming
//! reconstructs minimal `SearchItem`s from them (filename-derived title,
//! no artist/album) rather than depending on a prior library scan still
//! matching, so a moved/renamed library doesn't make resume fail outright
//!; paths that no longer exist are just silently skipped.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::library::{self, MediaKind};
use crate::search::SearchItem;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlaybackState {
    pub now_playing_path: Option<String>,
    pub queue_paths: Vec<String>,
    pub position_secs: Option<f64>,
    pub volume: Option<f64>,
}

fn state_path() -> Result<PathBuf> {
    // state_dir() is Linux-only (XDG_STATE_HOME) per the dirs crate;
    // data_dir() is the next best cross-platform fit for "not config,
    // not cache, but not critical user data either."
    let dir = dirs::state_dir()
        .or_else(dirs::data_dir)
        .context("no state or data dir found for this platform")?;
    Ok(dir.join("wim").join("state.toml"))
}

pub fn save(state: &PlaybackState) -> Result<()> {
    let path = state_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let toml = toml::to_string_pretty(state)?;
    fs::write(&path, toml).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Best-effort load; a missing or corrupt state file just means
/// nothing to resume, not an error worth surfacing to the user.
pub fn load() -> Option<PlaybackState> {
    let path = state_path().ok()?;
    let raw = fs::read_to_string(path).ok()?;
    toml::from_str(&raw).ok()
}

/// Rebuilds a minimal playable `SearchItem` from a saved path, or
/// `None` if the file no longer exists / isn't a recognized media type.
pub fn item_from_path(path_str: &str) -> Option<SearchItem> {
    let path = PathBuf::from(path_str);
    if !path.is_file() {
        return None;
    }
    let ext = path.extension()?.to_str()?;
    let title = library::filename_title(&path);
    match library::classify(ext)? {
        MediaKind::Music => {
            Some(SearchItem::LocalMusic { path, title, artist: None, album: None })
        }
        MediaKind::Video => Some(SearchItem::LocalVideo { path, title }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn item_from_path_rejects_missing_files() {
        assert!(item_from_path("/definitely/does/not/exist.mp3").is_none());
    }

    #[test]
    fn item_from_path_rebuilds_a_minimal_local_music_item() {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let path = std::env::temp_dir().join(format!("wim-test-{nanos}.mp3"));
        fs::write(&path, "not real audio, just needs to exist").unwrap();

        let result = item_from_path(&path.to_string_lossy());
        fs::remove_file(&path).unwrap();

        match result {
            Some(SearchItem::LocalMusic { artist, album, .. }) => {
                assert!(artist.is_none());
                assert!(album.is_none());
            }
            other => panic!("expected LocalMusic, got {other:?}"),
        }
    }
}
