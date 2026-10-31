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

/// What an entry represents.
///
/// Added because "a missing `path`/`id` is already the discriminant"
/// stopped being safe to rely on. An entry that is neither a local
/// file nor a remote song (a timed silence, once the queue timeline
/// lands) serialises to just its title fields, `item_of` returns
/// `None`, and `load` drops it **silently and indistinguishably from a
/// moved file**.
///
/// Written out from now on, and optional on the way in: a queue saved
/// before this has no `kind`, and `Entry::kind` falls back to exactly
/// the old rule (a path means local, a server plus id means remote),
/// so every existing file loads unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Local,
    Remote,
}

/// Why an entry in a saved queue did not come back, so the caller can
/// say so instead of quietly handing over a shorter queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dropped {
    /// A local file that is no longer where it was.
    Moved(String),
    /// A remote song whose server is not configured any more.
    ServerGone(String),
    /// An entry this build does not know how to rebuild: a `kind` from
    /// a newer wim, or an old untagged entry with neither a path nor a
    /// server. Named rather than lumped in with the above, because
    /// "your wim is older than this file" is a different problem from
    /// "your file moved".
    Unrecognised(String),
}

impl Dropped {
    /// One line naming what was lost and why, for a notification.
    ///
    /// Unused in the binary until `load_queue`'s handler in main.rs
    /// adopts `load_report`, which is a `main.rs` change queued behind
    /// the library split; the reasons and their tests are the
    /// deliverable here. Same `cfg_attr` shape `video::Hole` and
    /// `lyrics::Loader` use for a prepared primitive.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn describe(&self) -> String {
        match self {
            Dropped::Moved(title) => format!("{title}: file moved or gone"),
            Dropped::ServerGone(title) => format!("{title}: its server is not configured"),
            Dropped::Unrecognised(title) => format!("{title}: unrecognised entry"),
        }
    }
}

/// A load, including what it could not rebuild.
#[derive(Debug, Clone, Default)]
pub struct Loaded {
    pub items: Vec<SearchItem>,
    pub dropped: Vec<Dropped>,
}

/// One queue entry. Flat with optional fields rather than an enum:
/// toml's handling of enums inside an array of tables is awkward.
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
    /// `None` only in a file written before kinds existed; `kind()`
    /// infers it then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<Kind>,
}

impl Entry {
    /// The explicit kind, or the pre-kinds rule inferred from which
    /// fields are present. `None` when neither applies, which is the
    /// case that used to vanish without trace.
    pub fn kind(&self) -> Option<Kind> {
        if let Some(kind) = self.kind {
            return Some(kind);
        }
        if self.path.is_some() {
            return Some(Kind::Local);
        }
        if self.server.is_some() && self.id.is_some() {
            return Some(Kind::Remote);
        }
        None
    }

    /// What to call this entry in a message about losing it.
    fn label(&self) -> String {
        if self.title.is_empty() {
            "(untitled)".to_string()
        } else {
            self.title.clone()
        }
    }
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
    load_report(name, clients).map(|l| l.items)
}

/// `load`, plus what it could not rebuild and why.
///
/// `load` discards that, so a fifty-track queue whose server is gone
/// came back as twelve tracks with nothing said about the other
/// thirty-eight. Callers that can show a message should prefer this.
pub fn load_report(name: &str, clients: &[Arc<Client>]) -> Option<Loaded> {
    let path = dir().ok()?.join(format!("{}.toml", slug(name)));
    let raw = fs::read_to_string(path).ok()?;
    let saved: SavedQueue = toml::from_str(&raw).ok()?;
    Some(rebuild(&saved, clients))
}

/// Split out from `load_report` so the rebuild rules are testable
/// without touching the filesystem.
fn rebuild(saved: &SavedQueue, clients: &[Arc<Client>]) -> Loaded {
    let mut out = Loaded::default();
    for entry in &saved.entries {
        match item_of(entry, clients) {
            Some(item) => out.items.push(item),
            None => out.dropped.push(match entry.kind() {
                Some(Kind::Local) => Dropped::Moved(entry.label()),
                Some(Kind::Remote) => Dropped::ServerGone(entry.label()),
                None => Dropped::Unrecognised(entry.label()),
            }),
        }
    }
    out
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
            kind: Some(Kind::Local),
            ..Entry::default()
        }),
        SearchItem::LocalVideo { path, title } => Some(Entry {
            path: Some(path.to_string_lossy().into_owned()),
            title: title.clone(),
            kind: Some(Kind::Local),
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
            kind: Some(Kind::Remote),
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

    // ------------------------------------------------- kinds and loss

    fn remote(title: &str, server: &str) -> Entry {
        Entry {
            server: Some(server.into()),
            id: Some("s1".into()),
            title: title.into(),
            kind: Some(Kind::Remote),
            ..Entry::default()
        }
    }

    /// The compatibility promise: a queue saved before kinds existed
    /// has no `kind` field, and must load exactly as it used to. This
    /// is toml written by the old code, pasted verbatim.
    #[test]
    fn a_queue_saved_before_kinds_existed_still_loads() {
        let raw = r#"
[[entries]]
path = "/music/a.flac"
title = "a"
artist = "x"
album = "y"

[[entries]]
server = "https://nav.example"
id = "song-1"
title = "b"
artist = "x"
album = "y"
"#;
        let saved: SavedQueue = toml::from_str(raw).expect("old format must still parse");
        assert_eq!(saved.entries.len(), 2);
        assert_eq!(
            saved.entries[0].kind(),
            Some(Kind::Local),
            "a path still means local"
        );
        assert_eq!(
            saved.entries[1].kind(),
            Some(Kind::Remote),
            "a server plus id still means remote"
        );
    }

    /// The bug this exists to stop: an entry that is neither, which
    /// used to be dropped indistinguishably from a moved file.
    #[test]
    fn an_entry_that_is_neither_is_named_rather_than_vanishing() {
        let entry = Entry {
            title: "a timed silence".into(),
            ..Entry::default()
        };
        assert_eq!(entry.kind(), None);

        let loaded = rebuild(
            &SavedQueue {
                entries: vec![entry],
            },
            &[],
        );
        assert!(loaded.items.is_empty());
        assert_eq!(
            loaded.dropped,
            vec![Dropped::Unrecognised("a timed silence".into())],
            "it has to be reported, not silently skipped"
        );
    }

    /// The present-day loss: a saved queue whose server is no longer
    /// configured came back shorter with nothing said about it.
    #[test]
    fn a_remote_entry_with_no_live_client_is_reported_as_such() {
        let loaded = rebuild(
            &SavedQueue {
                entries: vec![remote("gone", "https://nav.example")],
            },
            &[],
        );
        assert!(loaded.items.is_empty());
        assert_eq!(
            loaded.dropped,
            vec![Dropped::ServerGone("gone".into())],
            "the reason has to survive to the caller"
        );
    }

    #[test]
    fn a_local_entry_whose_file_is_gone_is_reported_as_moved() {
        let entry = Entry {
            path: Some("/nowhere/at/all/missing.flac".into()),
            title: "missing".into(),
            kind: Some(Kind::Local),
            ..Entry::default()
        };
        let loaded = rebuild(
            &SavedQueue {
                entries: vec![entry],
            },
            &[],
        );
        assert!(loaded.items.is_empty());
        assert_eq!(loaded.dropped, vec![Dropped::Moved("missing".into())]);
    }

    /// An entry with no title still has to be nameable in the message.
    #[test]
    fn a_titleless_entry_is_still_describable() {
        let loaded = rebuild(
            &SavedQueue {
                entries: vec![Entry::default()],
            },
            &[],
        );
        assert_eq!(loaded.dropped.len(), 1);
        assert!(loaded.dropped[0].describe().contains("untitled"));
    }

    /// Going forward the kind is written, so a future wim does not
    /// have to infer it.
    #[test]
    fn a_saved_entry_writes_its_kind_explicitly() {
        let toml = toml::to_string_pretty(&SavedQueue {
            entries: vec![remote("b", "https://nav.example")],
        })
        .unwrap();
        assert!(toml.contains("kind = \"remote\""), "{toml}");
    }
}
