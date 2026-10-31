//! Fetching navidrome catalogs, and the thread that keeps them fresh.
//!
//! The other half of main.rs's channel plumbing (see ROADMAP), and the
//! same shape as `enrich`: pure network I/O on its own thread, results
//! handed back over a channel, because `App`, mpv and the terminal are
//! none of them `Send` across the borrow they live behind. Nothing in
//! here touches app state directly except `merge_catalog_into_app`,
//! which is the one function the main thread calls *with* its `App`.

use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::cache;
use crate::core::{client_for, retain_live_clients};
use crate::{app, config, subsonic, trace};

/// How often the background thread (see `spawn_background_sync`)
/// re-fetches every configured server, so navidrome stays "constantly
/// updating" without the user ever pressing a manual refresh key.
pub const AUTO_SYNC_INTERVAL: Duration = Duration::from_secs(45);

/// A completed background sync attempt for one server, tagged by url
/// since several servers' results arrive interleaved on the same
/// channel with no other way to tell them apart. Carries the starred
/// song ids too: they used to be fetched synchronously before the
/// first frame, and they ride the connection this thread is already
/// making rather than needing one of their own.
pub struct BackgroundSyncResult {
    pub url: String,
    pub result: NavidromeEverything,
}

/// Runs forever on its own thread, re-fetching every server's catalog
/// on a timer and reporting each attempt back over `result_tx`; the
/// main thread owns `App`/mpv/the terminal, none of which are `Send`
/// across this boundary, so this only ever does pure network I/O and
/// hands the result back rather than touching app state directly.
/// `servers_rx` lets the main thread push an updated server list (add/
/// remove a profile) without restarting the thread. This is also the
/// initial connect: it fetches first and waits afterwards, so a
/// configured server is reached as fast as the network allows without
/// anything on the main thread blocking on it.
pub fn spawn_background_sync(
    initial_servers: Vec<config::NavidromeConfig>,
    result_tx: mpsc::Sender<BackgroundSyncResult>,
    servers_rx: mpsc::Receiver<Vec<config::NavidromeConfig>>,
) {
    thread::spawn(move || {
        let mut servers = initial_servers;
        let mut first_pass = true;
        // kept across passes: `Client::new` re-reads `password_file`
        // off disk and starts with a cold connection pool, and this
        // loop runs every 45 seconds for the life of the app, so
        // rebuilding per pass is pure waste. the sync thread cannot see
        // the corpus's clients (different thread), so it keeps its own.
        let mut clients: Vec<Arc<subsonic::Client>> = Vec::new();
        loop {
            retain_live_clients(&mut clients, &servers);
            // independent per-server requests, so run them
            // concurrently rather than making every server wait for
            // the one before it.
            let results: Vec<BackgroundSyncResult> = thread::scope(|scope| {
                servers
                    .iter()
                    .map(|nav| {
                        (
                            nav.url.clone(),
                            scope.spawn(|| fetch_navidrome_everything(nav, &clients)),
                        )
                    })
                    .collect::<Vec<_>>()
                    .into_iter()
                    .map(|(url, handle)| BackgroundSyncResult {
                        url,
                        result: handle
                            .join()
                            .unwrap_or_else(|_| Err("panicked while syncing".to_string())),
                    })
                    .collect()
            });
            // take delivery of any client this pass had to build, so
            // the next one reuses it.
            for r in &results {
                if let Ok((catalog, _)) = &r.result {
                    if !clients.iter().any(|c| Arc::ptr_eq(c, &catalog.client)) {
                        clients.push(catalog.client.clone());
                    }
                }
            }
            if first_pass && !servers.is_empty() {
                trace::mark("navidrome: first sync landed");
                first_pass = false;
            }
            for result in results {
                if result_tx.send(result).is_err() {
                    return; // main thread is gone (shutting down)
                }
            }
            match servers_rx.recv_timeout(AUTO_SYNC_INTERVAL) {
                Ok(updated) => servers = updated,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    });
}

/// The catalog shape fetched over the wire, kept separate from `App` so
/// a background thread can fetch it without touching app state (which
/// isn't `Send` across the mpv/terminal borrow it lives behind).
pub struct FetchedCatalog {
    client: Arc<subsonic::Client>,
    artists: Vec<subsonic::Artist>,
    albums: Vec<subsonic::Album>,
    playlists: Vec<subsonic::Playlist>,
}

impl FetchedCatalog {
    fn summary(&self) -> String {
        format!(
            "{} artists, {} albums, {} playlists indexed",
            self.artists.len(),
            self.albums.len(),
            self.playlists.len()
        )
    }
}

/// Pings before fetching; a typo'd url/password surfaces as an error
/// rather than silently returning an empty catalog. Pure network I/O,
/// safe to call from any thread.
/// One catalog fetch's outcome. A panicked thread and a failed request
/// are the same thing to the caller: we do not know what this server
/// has, so nothing may be reconciled against it.
pub fn fetched<T>(
    joined: thread::Result<anyhow::Result<Vec<T>>>,
    what: &str,
) -> Result<Vec<T>, String> {
    match joined {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(format!("{what} failed: {e}")),
        Err(_) => Err(format!("{what} panicked")),
    }
}

pub fn fetch_navidrome_catalog(
    nav: &config::NavidromeConfig,
    live: &[Arc<subsonic::Client>],
) -> Result<FetchedCatalog, String> {
    // no `ping` first: a failing fetch below says everything a ping
    // would have, and the ping was a whole extra serial round trip on
    // every pass (one per server per 45s sync, so ~80 wasted requests
    // an hour). `Client::new` can still fail on a bad config, which is
    // local and free to check.
    let client = client_for(nav, live).map_err(|e| format!("connection failed: {e}"))?;
    // getArtists returns everything in one call, getAlbumList2 is
    // paginated (see subsonic.rs). songs stay fetched on demand (per
    // album/playlist/artist opened) rather than indexed eagerly; a
    // library this size could be enormous and isn't needed up front
    // for search to work on artists/albums. these three are
    // independent requests; fetching them concurrently instead of one
    // after another is most of the difference between this feeling
    // instant and feeling like three round-trips stacked up.
    //
    // every one of these is propagated, never defaulted to empty.
    // `Corpus::reconcile_server` drops what a server no longer lists,
    // so an empty list is a *claim* that the server has nothing; a
    // failed request that defaulted to empty would make a network blip
    // delete that server's whole library from the corpus and then
    // persist the deletion through `save_catalog`.
    let (artists, albums, playlists) = thread::scope(|scope| {
        let artists = scope.spawn(|| client.all_artists());
        let albums = scope.spawn(|| client.all_albums());
        let playlists = scope.spawn(|| client.playlists());
        (
            fetched(artists.join(), "artists"),
            fetched(albums.join(), "albums"),
            fetched(playlists.join(), "playlists"),
        )
    });
    let (artists, albums, playlists) = (artists?, albums?, playlists?);
    // persisted here, not where it gets merged: every caller of this
    // is already off the main thread (or already blocking on the
    // network), and serializing a catalog is not something to spend a
    // frame on. best-effort, and a no-op when nothing changed.
    let _ = cache::save_catalog(&client, &artists, &albums, &playlists);
    Ok(FetchedCatalog {
        client,
        artists,
        albums,
        playlists,
    })
}

/// Merges a fetched catalog into the corpus and immediately resyncs
/// whatever's displayed; the one place this happens, so every caller
/// (startup, manual reconnect, add-profile, background auto-sync) gets
/// it for free instead of having to remember it.
pub fn merge_catalog_into_app(app: &mut app::App, catalog: FetchedCatalog) {
    app.corpus.reconcile_server(
        cache::ServerCatalog {
            artists: catalog.artists,
            albums: catalog.albums,
            playlists: catalog.playlists,
        },
        &catalog.client,
    );
    app.resync_results();
}

type NavidromeEverything = Result<(FetchedCatalog, Vec<String>), String>;

/// Catalog + starred-song-ids for one server, fetched together so
/// the startup multi-server pass (see `main`) only connects once per
/// server rather than once for the catalog and again for starred
/// songs. Pure network I/O, safe to call from any thread.
pub fn fetch_navidrome_everything(
    nav: &config::NavidromeConfig,
    live: &[Arc<subsonic::Client>],
) -> NavidromeEverything {
    let catalog = fetch_navidrome_catalog(nav, live)?;
    let starred = catalog
        .client
        .get_starred_songs()
        .map(|songs| songs.into_iter().map(|s| s.id).collect())
        .unwrap_or_default();
    Ok((catalog, starred))
}

/// Shared by onboarding, the profile manager's add flow, and reconnect
/// on an existing profile; so none of them can drift out of sync.
/// Doesn't touch `Config` itself: callers decide whether/how to
/// persist (append a new profile vs. just refreshing an existing one).
pub fn index_navidrome_server(
    app: &mut app::App,
    nav: &config::NavidromeConfig,
) -> Result<String, String> {
    // `&[]` on purpose: this runs when a profile is added or
    // reconnected after an edit, where the whole point is to build a
    // client from the credentials as they are now rather than reuse one
    // carrying the old ones.
    let catalog = fetch_navidrome_catalog(nav, &[])?;
    let summary = catalog.summary();
    merge_catalog_into_app(app, catalog);
    Ok(summary)
}

/// Plain connectivity check (ping only, no catalog fetch) for every
/// configured server at once, so opening the profile manager shows
/// where each one stands immediately rather than making you select
/// each one individually to find out.
pub fn test_navidrome_connectivity(
    servers: &[config::NavidromeConfig],
    live: &[Arc<subsonic::Client>],
) -> Vec<Option<bool>> {
    thread::scope(|scope| {
        servers
            .iter()
            // pinging the client the app actually uses is a better test
            // than pinging a throwaway: it exercises the same pool and
            // the same resolved credentials.
            .map(|nav| scope.spawn(|| client_for(nav, live).is_ok_and(|c| c.ping().is_ok())))
            .collect::<Vec<_>>()
            .into_iter()
            .map(|handle| Some(handle.join().unwrap_or(false)))
            .collect()
    })
}

#[cfg(test)]
mod catalog_fetch_tests {
    use super::*;
    use crate::subsonic;

    #[test]
    fn a_failed_fetch_is_an_error_not_an_empty_catalog() {
        let failed: thread::Result<anyhow::Result<Vec<u8>>> =
            Ok(Err(anyhow::anyhow!("502 bad gateway")));
        let got = fetched(failed, "artists");
        assert!(got.is_err(), "a failed fetch must not read as empty");
        assert!(got.unwrap_err().contains("502"), "the cause should survive");
    }

    #[test]
    fn a_panicked_fetch_thread_is_also_an_error() {
        let panicked: thread::Result<anyhow::Result<Vec<u8>>> =
            Err(Box::new("thread died") as Box<dyn std::any::Any + Send>);
        assert!(fetched(panicked, "albums").is_err());
    }

    fn nav(url: &str, user: &str) -> config::NavidromeConfig {
        config::NavidromeConfig {
            url: url.into(),
            username: user.into(),
            password: Some("pw".into()),
            password_file: None,
            max_bitrate_kbps: None,
            transcode_format: None,
        }
    }

    /// The sync thread reuses clients across passes, so an edited
    /// profile must not keep syncing through the old one. Dropping it
    /// here is also what zeroizes the superseded password.
    #[test]
    fn an_edited_profile_loses_its_kept_client() {
        let profile = nav("https://example.invalid", "ana");
        let mut clients = vec![Arc::new(subsonic::Client::new(&profile).unwrap())];
        let mut servers = vec![profile];

        retain_live_clients(&mut clients, &servers);
        assert_eq!(clients.len(), 1, "an unchanged profile keeps its client");

        servers[0].max_bitrate_kbps = Some(128);
        retain_live_clients(&mut clients, &servers);
        assert!(
            clients.is_empty(),
            "a changed stream option is an edit; the old client must go"
        );
    }

    #[test]
    fn a_removed_profile_loses_its_kept_client() {
        let profile = nav("https://example.invalid", "ana");
        let mut clients = vec![Arc::new(subsonic::Client::new(&profile).unwrap())];
        retain_live_clients(&mut clients, &[]);
        assert!(clients.is_empty());
    }

    /// Several servers configured: each keeps its own, and only the one
    /// that went away is dropped.
    #[test]
    fn retaining_is_per_server_not_all_or_nothing() {
        let a = nav("https://a.invalid", "ana");
        let b = nav("https://b.invalid", "bob");
        let mut clients = vec![
            Arc::new(subsonic::Client::new(&a).unwrap()),
            Arc::new(subsonic::Client::new(&b).unwrap()),
        ];
        retain_live_clients(&mut clients, std::slice::from_ref(&a));
        assert_eq!(clients.len(), 1);
        assert!(clients[0].matches(&a));
    }

    /// A server that genuinely has nothing is still a legitimate answer
    /// and must stay distinguishable from a failure, since that is the
    /// case reconciliation exists to handle.
    #[test]
    fn a_genuinely_empty_server_still_succeeds() {
        let empty: thread::Result<anyhow::Result<Vec<u8>>> = Ok(Ok(Vec::new()));
        assert_eq!(fetched(empty, "playlists").unwrap(), Vec::<u8>::new());
    }
}
