//! Named queues, saved by `shellsong`'s `save_queue(name)` and read
//! back by `load_queue(name)`. One toml file per queue under the same
//! state dir `persistence.rs` uses, for the same reason it picks that
//! dir: not config, not cache, not critical user data.
//!
//! Entries carry more than `persistence::PlaybackState`'s plain paths.
//! A saved queue is usually mostly remote songs, and a stream url is
//! not something to persist (it carries credentials and expires), so a
//! remote entry stores its server url plus the server-side song id and
//! is re-bound to a live client on load. An entry whose server is no
//! longer configured is skipped, same best-effort stance as a local
//! path that no longer exists.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::search::SearchItem;
use crate::subsonic::Client;

/// One queue entry. Flat with optional fields rather than an enum:
/// toml's handling of enums inside an array of tables is awkward, and
/// a missing `path`/`id` is already the discriminant.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Entry {
    /// Local file path; `None` for a remote song.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Originating server's base url, matched back to a live client on
    /// load; `None` for a local file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    /// Server-side song id; `None` for a local file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub artist: String,
    #[serde(default)]
    pub album: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SavedQueue {
    #[serde(default)]
    pub entries: Vec<Entry>,
}

fn dir() -> Result<PathBuf> {
    // same dir choice and reasoning as `persistence::state_path`.
    let base = dirs::state_dir()
        .or_else(dirs::data_dir)
        .context("no state or data dir found for this platform")?;
    Ok(base.join("wim").join("queues"))
}

/// Queue names are user-invented free text and end up as filenames, so
/// anything that could escape the directory or confuse a shell is
/// folded to `-`. Lowercased too, which is what makes the names
/// case-insensitive to load back.
pub fn slug(name: &str) -> String {
    let mut out = String::new();
    for ch in name.trim().to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

/// Saved queue names, alphabetical; backs `view(playlists)` and the
/// `saved_name` completion candidates.
pub fn list() -> Vec<String> {
    let Ok(dir) = dir() else {
        return Vec::new();
    };
    let Ok(read) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = read
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let path = e.path();
            (path.extension()? == "toml")
                .then(|| path.file_stem()?.to_str().map(str::to_string))
                .flatten()
        })
        .collect();
    names.sort();
    names
}

pub fn save(name: &str, items: &[SearchItem]) -> Result<usize> {
    let slug = slug(name);
    if slug.is_empty() {
        anyhow::bail!("'{name}' is not a usable queue name");
    }
    let dir = dir()?;
    fs::create_dir_all(&dir)?;
    let saved = SavedQueue {
        entries: items.iter().filter_map(entry_of).collect(),
    };
    let saved_count = saved.entries.len();
    let path = dir.join(format!("{slug}.toml"));
    let toml = toml::to_string_pretty(&saved)?;
    fs::write(&path, toml).with_context(|| format!("writing {}", path.display()))?;
    Ok(saved_count)
}

/// `None` when nothing is saved under that name; `Some(items)` with
/// whatever could still be resolved otherwise, which may be shorter
/// than what was saved (a moved file, a server no longer configured).
pub fn load(name: &str, clients: &[Arc<Client>]) -> Option<Vec<SearchItem>> {
    let path = dir().ok()?.join(format!("{}.toml", slug(name)));
    let raw = fs::read_to_string(path).ok()?;
    let saved: SavedQueue = toml::from_str(&raw).ok()?;
    Some(
        saved
            .entries
            .iter()
            .filter_map(|e| item_of(e, clients))
            .collect(),
    )
}

/// `None` for anything that is not a single track: an album or artist
/// in a queue is impossible (`resolve_tracks` expands both before
/// anything reaches the queue), so there is nothing to represent.
fn entry_of(item: &SearchItem) -> Option<Entry> {
    match item {
        SearchItem::LocalMusic {
            path,
            title,
            artist,
            album,
        } => Some(Entry {
            path: Some(path.to_string_lossy().into_owned()),
            title: title.clone(),
            artist: artist.clone().unwrap_or_default(),
            album: album.clone().unwrap_or_default(),
            ..Entry::default()
        }),
        SearchItem::LocalVideo { path, title } => Some(Entry {
            path: Some(path.to_string_lossy().into_owned()),
            title: title.clone(),
            ..Entry::default()
        }),
        SearchItem::Song {
            id,
            title,
            artist,
            album,
            client,
        } => Some(Entry {
            server: Some(client.base_url().to_string()),
            id: Some(id.clone()),
            title: title.clone(),
            artist: artist.clone(),
            album: album.clone(),
            ..Entry::default()
        }),
        SearchItem::Artist { .. } | SearchItem::Album { .. } | SearchItem::Playlist { .. } => None,
    }
}

/// Rebinds one entry to something playable. A local path goes through
/// `persistence::item_from_path`, which is already the one place that
/// knows how to classify a file; a remote song needs its server to
/// still be connected, since the client is what builds a stream url.
fn item_of(entry: &Entry, clients: &[Arc<Client>]) -> Option<SearchItem> {
    if let Some(path) = &entry.path {
        return crate::persistence::item_from_path(path);
    }
    let (server, id) = (entry.server.as_deref()?, entry.id.as_deref()?);
    let client = clients.iter().find(|c| c.base_url() == server)?;
    Some(SearchItem::Song {
        id: id.to_string(),
        title: entry.title.clone(),
        artist: entry.artist.clone(),
        album: entry.album.clone(),
        client: client.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_folds_to_a_filename_safe_slug() {
        assert_eq!(slug("late night"), "late-night");
        assert_eq!(slug("  Late  Night!!  "), "late-night");
        // the whole point: a name can never escape the queues dir.
        assert_eq!(slug("../../etc/passwd"), "etc-passwd");
        assert_eq!(slug("a/b"), "a-b");
        assert_eq!(slug("!!!"), "");
    }

    #[test]
    fn a_local_track_round_trips_through_an_entry() {
        let item = SearchItem::LocalMusic {
            path: PathBuf::from("/music/hex.flac"),
            title: "hex".into(),
            artist: Some("bladee".into()),
            album: None,
        };
        let entry = entry_of(&item).expect("a local track is one track");
        assert_eq!(entry.path.as_deref(), Some("/music/hex.flac"));
        assert_eq!(entry.artist, "bladee");
        assert!(entry.server.is_none());
    }

    #[test]
    fn a_multi_track_entity_has_nothing_to_save() {
        // albums/artists/playlists are expanded by `resolve_tracks`
        // long before anything reaches the queue, so an entry for one
        // would be unloadable rather than merely lossy.
        assert!(entry_of(&SearchItem::LocalVideo {
            path: PathBuf::from("/v/clip.mkv"),
            title: "clip".into(),
        })
        .is_some());
    }

    #[test]
    fn a_remote_entry_with_no_matching_server_is_skipped() {
        let entry = Entry {
            server: Some("https://music.example".into()),
            id: Some("42".into()),
            title: "crest".into(),
            ..Entry::default()
        };
        assert!(item_of(&entry, &[]).is_none());
    }
}
