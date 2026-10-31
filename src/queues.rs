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
    /// A timed silence. Has neither a path nor a server, so it is the
    /// one kind `kind()`'s pre-kinds inference can never guess; a
    /// queue holding one is only loadable because the discriminant is
    /// written explicitly now.
    Silence,
    /// A kind this build cannot rebuild, i.e. one a newer wim wrote.
    /// Only ever produced on the way in, by `lenient_kind`; nothing
    /// serialises it, since `entry_of` builds entries from live items.
    Unknown,
}

/// Reads `kind` without letting one unreadable value cost the whole
/// file. An unknown variant is a hard `toml` error, so a single entry
/// from a newer wim made the other forty-nine unparseable and
/// `load_report` then reported the queue as not existing at all.
///
/// `Kind`'s own `Deserialize` decides what is known, so this list
/// cannot drift from the enum.
fn lenient_kind<'de, D>(de: D) -> Result<Option<Kind>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::IntoDeserializer;
    let Some(raw) = Option::<String>::deserialize(de)? else {
        return Ok(None);
    };
    let known: serde::de::value::StrDeserializer<serde::de::value::Error> =
        raw.as_str().into_deserializer();
    Ok(Some(Kind::deserialize(known).unwrap_or(Kind::Unknown)))
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
    /// One line naming what was lost and why, for the single-loss
    /// case; `summarise` groups instead once there are several.
    pub fn describe(&self) -> String {
        match self {
            Dropped::Moved(title) => format!("{title}: file moved or gone"),
            Dropped::ServerGone(title) => format!("{title}: its server is not configured"),
            Dropped::Unrecognised(title) => format!("{title}: unrecognised entry"),
        }
    }
}

/// What a load lost, in one line, for the status line a command is
/// allowed (docs/shellsong.md R9).
///
/// One loss names itself, because the title is the useful part when
/// there is only one. Several group by reason and count, because
/// forty titles is not a status line and "38 missing" with no reason
/// is the silent-shortening this exists to end.
pub fn summarise(dropped: &[Dropped]) -> String {
    if let [only] = dropped {
        return only.describe();
    }
    let (mut moved, mut gone, mut odd) = (0usize, 0usize, 0usize);
    for d in dropped {
        match d {
            Dropped::Moved(_) => moved += 1,
            Dropped::ServerGone(_) => gone += 1,
            Dropped::Unrecognised(_) => odd += 1,
        }
    }
    let mut parts = Vec::new();
    if gone > 0 {
        parts.push(format!("{gone} with no configured server"));
    }
    if moved > 0 {
        parts.push(format!("{moved} moved or gone"));
    }
    if odd > 0 {
        parts.push(format!("{odd} unrecognised"));
    }
    parts.join(", ")
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
    #[serde(
        default,
        deserialize_with = "lenient_kind",
        skip_serializing_if = "Option::is_none"
    )]
    pub kind: Option<Kind>,
    /// A silence's length. `None` for every other kind, and absent
    /// from the toml for them, so an existing saved queue is byte
    /// identical to what it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secs: Option<u64>,
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
                // a silence always rebuilds (it needs nothing from
                // the filesystem or a server), so that half is
                // unreachable; a kind from a newer wim is the live
                // case, and neither has a reason beyond "this build
                // cannot rebuild it".
                Some(Kind::Silence | Kind::Unknown) | None => Dropped::Unrecognised(entry.label()),
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
        // saved rather than dropped: a silence is part of the shape of
        // a queue, and losing it on save would reorder what plays.
        SearchItem::Silence { secs } => Some(Entry {
            title: "silence".to_string(),
            kind: Some(Kind::Silence),
            secs: Some(*secs),
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
    // the kind decides, not which fields happen to be filled in: an
    // entry from a newer wim can carry a path or a server and still
    // not be rebuildable as either.
    match entry.kind()? {
        // needs nothing from the filesystem or a server, so it is
        // answered before anything is looked up.
        Kind::Silence => Some(SearchItem::Silence {
            secs: entry.secs.unwrap_or_default(),
        }),
        Kind::Local => crate::persistence::item_from_path(entry.path.as_deref()?),
        Kind::Remote => {
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
        Kind::Unknown => None,
    }
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

    /// A silence is part of the shape of a queue, so saving one and
    /// loading it back has to give the same queue; dropping it would
    /// silently reorder what plays.
    #[test]
    fn a_silence_round_trips_through_an_entry() {
        let entry = entry_of(&SearchItem::Silence { secs: 900 }).expect("a silence is savable");
        assert_eq!(entry.kind, Some(Kind::Silence));
        assert_eq!(entry.secs, Some(900));
        assert!(
            entry.path.is_none() && entry.server.is_none(),
            "a silence has neither"
        );

        let back = item_of(&entry, &[]).expect("and loads with no clients at all");
        assert!(matches!(back, SearchItem::Silence { secs: 900 }));
    }

    /// The compatibility half: a silence adds no keys to an entry that
    /// is not one, so every queue saved before this is byte identical.
    #[test]
    fn secs_is_absent_from_a_track_entry() {
        let item = SearchItem::LocalMusic {
            path: PathBuf::from("/music/hex.flac"),
            title: "hex".into(),
            artist: None,
            album: None,
        };
        let entry = entry_of(&item).expect("a local track is one track");
        let toml = toml::to_string(&SavedQueue {
            entries: vec![entry],
        })
        .expect("serialises");
        assert!(!toml.contains("secs"), "{toml}");
    }

    /// The test this replaces asserted only that a *video* is savable,
    /// under this name, so the three arms it claims to cover were
    /// unpinned: an album saved as an entry carries no path, no server
    /// and no kind, so it comes back as `Unrecognised` and a reload is
    /// short by one track with a reason that is not the real one.
    #[test]
    fn a_multi_track_entity_has_nothing_to_save() {
        let client = crate::browse::tests::test_client();
        let multi = [
            SearchItem::Artist {
                id: "ar1".into(),
                name: "bladee".into(),
                client: client.clone(),
            },
            SearchItem::Album {
                id: "al1".into(),
                name: "crest".into(),
                artist: "bladee".into(),
                client: client.clone(),
            },
            SearchItem::Playlist {
                id: "pl1".into(),
                name: "late night".into(),
                client,
            },
        ];
        for item in &multi {
            assert!(
                entry_of(item).is_none(),
                "{} is many tracks, not an entry",
                item.primary_name()
            );
        }
        // the positive control: the loop above would also pass if
        // `entry_of` refused everything.
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

    /// Its own temp dir, so no test here reads or writes the real
    /// queues dir; same convention as `persistence::tests::scratch_dir`.
    fn scratch_dir(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("wim-queues-{label}-{nanos}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

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

    /// A queue written by a *newer* wim must cost the one entry this
    /// build cannot read, not the whole file. An unknown variant is a
    /// hard toml error, so the parse failed outright, `load_report`
    /// answered `None`, and the op reported "no saved queue called x"
    /// through `namehint` with the file sitting right there.
    ///
    /// The unknown entry's `path` points at a file that really exists,
    /// deliberately: the kind has to decide what an entry is, or a
    /// kind this build cannot read is quietly played as a local file.
    #[test]
    fn one_entry_from_a_newer_wim_does_not_cost_the_whole_queue() {
        let dir = scratch_dir("newer-wim");
        let real = dir.join("not actually a track.flac");
        fs::write(&real, "x").unwrap();
        let raw = format!(
            r#"
[[entries]]
title = "silence"
kind = "silence"
secs = 900

[[entries]]
title = "a video essay"
kind = "stream"
path = "{}"
"#,
            real.display()
        );
        let saved: SavedQueue =
            toml::from_str(&raw).expect("one unreadable entry must still parse");
        assert_eq!(saved.entries.len(), 2);
        assert_eq!(saved.entries[1].kind(), Some(Kind::Unknown));

        let loaded = rebuild(&saved, &[]);
        assert!(
            matches!(loaded.items.as_slice(), [SearchItem::Silence { secs: 900 }]),
            "what this build understands still loads"
        );
        assert_eq!(
            loaded.dropped,
            vec![Dropped::Unrecognised("a video essay".into())],
            "and the entry it does not is named, not guessed at"
        );
        fs::remove_dir_all(&dir).ok();
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

    #[test]
    fn one_loss_names_itself_because_the_title_is_the_useful_part() {
        let summary = summarise(&[Dropped::Moved("crest".into())]);
        assert_eq!(summary, "crest: file moved or gone");
    }

    /// Forty titles is not a status line, so several group by reason.
    #[test]
    fn several_losses_group_by_reason_and_count() {
        let dropped = vec![
            Dropped::ServerGone("a".into()),
            Dropped::ServerGone("b".into()),
            Dropped::Moved("c".into()),
        ];
        assert_eq!(
            summarise(&dropped),
            "2 with no configured server, 1 moved or gone"
        );
    }

    #[test]
    fn an_unrecognised_entry_is_counted_separately() {
        let dropped = vec![
            Dropped::Unrecognised("x".into()),
            Dropped::Unrecognised("y".into()),
        ];
        assert_eq!(summarise(&dropped), "2 unrecognised");
    }

    #[test]
    fn nothing_lost_summarises_to_nothing() {
        assert_eq!(summarise(&[]), "");
    }
}
