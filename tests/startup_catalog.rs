use std::sync::Arc;

use wim::app::App;
use wim::config::NavidromeConfig;
use wim::library::{LocalTrack, MediaKind};
use wim::search::{Corpus, SearchItem};
use wim::subsonic::{Artist, Client, SearchResult};

fn client(url: &str) -> Arc<Client> {
    Arc::new(
        Client::new(&NavidromeConfig {
            url: url.to_string(),
            username: "fixture".into(),
            password: Some("fixture".into()),
            password_file: None,
            max_bitrate_kbps: None,
            transcode_format: None,
        })
        .unwrap(),
    )
}

fn add_artist(corpus: &mut Corpus, client: &Arc<Client>, id: &str, name: &str) {
    corpus.add_subsonic_search_result(
        SearchResult {
            artists: vec![Artist {
                id: id.into(),
                name: name.into(),
            }],
            ..SearchResult::default()
        },
        client,
    );
}

#[test]
fn warmed_rows_merge_in_batches_without_replacing_local_or_newer_results() {
    let a = client("https://a.invalid");
    let b = client("https://b.invalid");
    let mut warm = Corpus::new();
    add_artist(&mut warm, &a, "old", "removed by live sync");
    add_artist(&mut warm, &b, "same", "old cached name");
    add_artist(&mut warm, &b, "new", "cached discovery");
    let mut cached = warm.into_cached_entries();

    let mut app = App::new(Corpus::new());
    app.corpus.add_local_tracks([LocalTrack {
        path: "/fixture/local.flac".into(),
        title: "local discovery".into(),
        artist: None,
        album: None,
        duration_secs: None,
        kind: MediaKind::Music,
    }]);
    add_artist(&mut app.corpus, &b, "same", "fresh live name");
    app.set_query("discovery");

    // A successful later sync for A makes its cached rows stale. The
    // first batch must not resurrect them, even when they have unique ids.
    assert_eq!(
        app.corpus
            .merge_cached_batch(&mut cached, 1, |c| c.base_url() == b.base_url()),
        0
    );
    assert!(!cached.is_empty());
    assert_eq!(
        app.corpus
            .merge_cached_batch(&mut cached, 8, |c| c.base_url() == b.base_url()),
        1
    );
    assert!(cached.is_empty());
    app.resync_results();

    let names: Vec<&str> = app
        .corpus
        .items()
        .map(|item| match item {
            SearchItem::Artist { name, .. } => name.as_str(),
            SearchItem::LocalMusic { title, .. } => title.as_str(),
            _ => unreachable!(),
        })
        .collect();
    assert!(names.contains(&"local discovery"));
    assert!(names.contains(&"fresh live name"));
    assert!(names.contains(&"cached discovery"));
    assert!(!names.contains(&"old cached name"));
    assert!(!names.contains(&"removed by live sync"));
    assert_eq!(app.results().len(), 2, "the live query is re-ranked");
}
