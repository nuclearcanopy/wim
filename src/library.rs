//! Local filesystem music/video scanning. Tags come from `lofty` where
//! available; anything that fails to parse (or video, which we don't tag)
//! falls back to a filename-derived title so it still shows up in search.

use std::path::{Path, PathBuf};

use lofty::file::{AudioFile, TaggedFileExt};
use lofty::prelude::*;
use lofty::probe::Probe;
use walkdir::WalkDir;

const MUSIC_EXTENSIONS: &[&str] = &["mp3", "flac", "ogg", "opus", "m4a", "wav", "aac", "wma"];
const VIDEO_EXTENSIONS: &[&str] = &["mp4", "mkv", "webm", "avi", "mov", "m4v", "ts"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Music,
    Video,
}

#[derive(Debug, Clone)]
pub struct LocalTrack {
    pub path: PathBuf,
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration_secs: Option<u32>,
    pub kind: MediaKind,
}

pub(crate) fn classify(ext: &str) -> Option<MediaKind> {
    let ext = ext.to_ascii_lowercase();
    if MUSIC_EXTENSIONS.contains(&ext.as_str()) {
        Some(MediaKind::Music)
    } else if VIDEO_EXTENSIONS.contains(&ext.as_str()) {
        Some(MediaKind::Video)
    } else {
        None
    }
}

pub(crate) fn filename_title(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

fn read_music_tags(path: &Path) -> LocalTrack {
    let fallback_title = filename_title(path);

    let tagged = Probe::open(path).ok().and_then(|p| p.read().ok());
    let Some(tagged) = tagged else {
        return LocalTrack {
            path: path.to_path_buf(),
            title: fallback_title,
            artist: None,
            album: None,
            duration_secs: None,
            kind: MediaKind::Music,
        };
    };

    let duration_secs = Some(tagged.properties().duration().as_secs() as u32);
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag());

    let title = tag
        .and_then(|t| t.title())
        .map(|c| c.into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or(fallback_title);
    let artist = tag.and_then(|t| t.artist()).map(|c| c.into_owned());
    let album = tag.and_then(|t| t.album()).map(|c| c.into_owned());

    LocalTrack {
        path: path.to_path_buf(),
        title,
        artist,
        album,
        duration_secs,
        kind: MediaKind::Music,
    }
}

fn video_track(path: &Path) -> LocalTrack {
    LocalTrack {
        path: path.to_path_buf(),
        title: filename_title(path),
        artist: None,
        album: None,
        duration_secs: None,
        kind: MediaKind::Video,
    }
}

/// Standard XDG user dirs (`~/Music`, `~/Videos`), used when the config
/// doesn't list any explicit `library_paths`; most people keep their
/// media in the usual home-folder layout and shouldn't have to configure
/// paths just to get started.
pub fn default_library_paths() -> Vec<String> {
    [dirs::audio_dir(), dirs::video_dir()]
        .into_iter()
        .flatten()
        .filter(|p| p.is_dir())
        .map(|p| p.to_string_lossy().into_owned())
        .collect()
}

/// Recursively scans each configured library path, returning every music
/// and video file found. Unreadable tags are not fatal; the file still
/// gets indexed with a filename-derived title. Falls back to
/// `default_library_paths()` when `paths` is empty.
pub fn scan(paths: &[String]) -> Vec<LocalTrack> {
    let owned_defaults;
    let paths = if paths.is_empty() {
        owned_defaults = default_library_paths();
        &owned_defaults
    } else {
        paths
    };

    let mut tracks = Vec::new();
    for root in paths {
        for entry in WalkDir::new(root)
            .follow_links(true)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
                continue;
            };
            match classify(ext) {
                Some(MediaKind::Music) => tracks.push(read_music_tags(path)),
                Some(MediaKind::Video) => tracks.push(video_track(path)),
                None => {}
            }
        }
    }
    tracks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_by_extension_case_insensitively() {
        assert_eq!(classify("MP3"), Some(MediaKind::Music));
        assert_eq!(classify("mkv"), Some(MediaKind::Video));
        assert_eq!(classify("txt"), None);
    }

    #[test]
    fn filename_title_strips_extension() {
        let p = Path::new("/music/Artist/Album/01 - Song Title.flac");
        assert_eq!(filename_title(p), "01 - Song Title");
    }
}
