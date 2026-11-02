use std::fs;
use std::thread;
use std::time::{Duration, Instant};

use wim::lyrics::{parse_synced, Lyrics};
use wim::lyricspane::LyricsPane;
use wim::onboarding::{Onboarding, Step};

#[test]
fn first_run_setup_includes_lyrics_before_library_paths() {
    let mut setup = Onboarding::new(Vec::new());
    for expected in [
        Step::Controls,
        Step::Navidrome,
        Step::ListenBrainz,
        Step::Lyrics,
        Step::LibraryPaths,
        Step::Done,
    ] {
        setup.advance();
        assert_eq!(setup.step, expected);
    }
}

#[test]
fn lrc_offsets_multiple_timestamps_and_fractional_precision_are_applied() {
    let parsed =
        parse_synced("[ar:Artist]\n[offset:-100]\n[00:01.2][00:03.45]same\n[01:02.003]next");
    assert_eq!(parsed.len(), 3);
    assert!((parsed[0].at - 1.1).abs() < 0.001);
    assert!((parsed[1].at - 3.35).abs() < 0.001);
    assert!((parsed[2].at - 61.903).abs() < 0.001);
    assert_eq!(parsed[0].text, "same");
    assert_eq!(parsed[2].text, "next");
}

#[test]
fn enhanced_lrc_word_timestamps_are_removed_from_display_text() {
    let parsed = parse_synced("[00:12.00]<00:12.00>Hello <00:12.50>there");
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].text, "Hello there");
}

#[test]
fn synced_pane_follows_and_resumes_after_three_seconds_of_manual_scroll() {
    let lyrics = Lyrics {
        synced: Some("[00:01.00]one\n[00:04.00]two\n[00:08.00]three".into()),
        source: "fixture".into(),
        ..Lyrics::default()
    };
    let mut pane = LyricsPane::default();
    pane.open_lyrics(&lyrics, "song".into());
    pane.saw_size(60, 12);

    let now = Instant::now();
    assert_eq!(pane.follow_playback(Some(5.0), now), Some(1));
    pane.set_scroll(0);
    assert!(!pane.following());
    pane.follow_playback(Some(9.0), now + Duration::from_secs(2));
    assert_eq!(pane.scroll(), 0, "manual scroll is respected temporarily");
    pane.follow_playback(Some(9.0), now + Duration::from_secs(4));
    assert!(pane.following());
}

#[test]
fn local_lyrics_are_cached_across_requests() {
    let root = std::env::temp_dir().join(format!("wim-lyrics-cache-{}", std::process::id()));
    fs::remove_dir_all(&root).ok();
    let cache = root.join("cache");
    let media = root.join("media");
    fs::create_dir_all(&media).unwrap();
    let previous_cache = std::env::var_os("XDG_CACHE_HOME");
    std::env::set_var("XDG_CACHE_HOME", &cache);

    let audio = media.join("track.flac");
    let sidecar = media.join("track.lrc");
    fs::write(&audio, "fixture").unwrap();
    fs::write(&sidecar, "[00:01.00]cached lyric").unwrap();
    let client = wim::lyrics::Client::with_providers(vec!["local".into()], Default::default());
    let mut loader = wim::lyrics::Loader::new();

    let fetch = |loader: &mut wim::lyrics::Loader| {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(answer) = loader.poll() {
                return answer.1.unwrap().unwrap();
            }
            assert!(Instant::now() < deadline, "local lyrics fetch timed out");
            thread::sleep(Duration::from_millis(5));
        }
    };

    loader.request_local(&client, "track", "artist", "track", "album", None, &audio);
    assert_eq!(
        fetch(&mut loader).synced.as_deref(),
        Some("[00:01.00]cached lyric")
    );
    fs::remove_file(&sidecar).unwrap();
    loader.request_local(&client, "track", "artist", "track", "album", None, &audio);
    assert_eq!(
        fetch(&mut loader).synced.as_deref(),
        Some("[00:01.00]cached lyric")
    );

    if let Some(previous) = previous_cache {
        std::env::set_var("XDG_CACHE_HOME", previous);
    } else {
        std::env::remove_var("XDG_CACHE_HOME");
    }
    fs::remove_dir_all(root).ok();
}
