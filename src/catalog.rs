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

/// Changes the configured server list **and** tells the sync thread,
/// which is the pairing that was got wrong.
///
/// `spawn_background_sync` keeps its own `servers` vec and its own
/// clients, and `servers_rx` is the only thing that can change either;
/// `retain_live_clients` runs off that list every pass. Three sites
/// mutated `cfg.navidrome_servers` and only one of them sent, so
/// deleting a profile left it being polled every 45s, re-merging its
/// catalog, toasting offline/online for a profile that is gone, and
/// holding the last `Arc<Client>` so the superseded password was never
/// zeroized. Adding one indexed it once and then never auto-resynced,
/// so new albums did not appear until a restart.
///
/// Takes the edit as a closure so the two halves cannot be done
/// separately: there is no way to reach the vec through here without
/// the send. Persisting stays the caller's, because the add paths also
/// warn about a plaintext password and the delete path does not.
pub fn edit_servers(
    cfg: &mut config::Config,
    servers_tx: &mpsc::Sender<Vec<config::NavidromeConfig>>,
    edit: impl FnOnce(&mut Vec<config::NavidromeConfig>),
) {
    edit(&mut cfg.navidrome_servers);
    // a dead receiver only means the sync thread is gone (shutting
    // down), the same stance every other channel in here takes.
    let _ = servers_tx.send(cfg.navidrome_servers.clone());
}

/// The catalog shape fetched over the wire, kept separate from `App` so
/// a background thread can fetch it without touching app state (which
/// isn't `Send` across the mpv/terminal borrow it lives behind).
pub struct FetchedCatalog {
    client: Arc<subsonic::Client>,
    artists: Vec<subsonic::Artist>,
    albums: Vec<subsonic::Album>,
    playlists: Vec<subsonic::Playlist>,
    /// Why this catalog couldn't be written to the startup cache, if
    /// it couldn't. Carried rather than reported here because this
    /// runs on a thread with no notification area; `cache_warning` is
    /// what `merge_catalog_into_app` hands the caller.
    cache_warning: Option<String>,
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
    // frame on. a no-op when nothing changed.
    //
    // not fatal to the fetch: this catalog is good, and refusing it
    // over a cache write would leave the corpus with nothing. but not
    // swallowed either, since the startup cache is the whole reason
    // the next launch paints a library instead of "connecting...",
    // and a read-only or full cache dir used to lose it in silence.
    // same stance as `config::save_or_warn`.
    let cache_warning = cache::save_catalog(&client, &artists, &albums, &playlists)
        .err()
        .map(|e| cache_warning_for(&nav.url, &e));
    Ok(FetchedCatalog {
        client,
        artists,
        albums,
        playlists,
        cache_warning,
    })
}

/// One wording for the one failure, so the sync path and the
/// add-a-profile path say the same thing. Names the server but
/// nothing else about it; the error itself carries the path.
fn cache_warning_for(url: &str, error: &impl std::fmt::Display) -> String {
    format!(
        "couldn't cache {url}'s library, so the next start has to wait on \
         the network for it: {error}"
    )
}

/// Merges a fetched catalog into the corpus and immediately resyncs
/// whatever's displayed; the one place this happens, so every caller
/// (startup, manual reconnect, add-profile, background auto-sync) gets
/// it for free instead of having to remember it.
///
/// Returns the warning the caller owes the user, same idiom as
/// `graphics::ArtProbe::poll`: this runs where there is an `App` but
/// not always a notification area, and the fetch itself happened on a
/// thread that had neither.
pub fn merge_catalog_into_app(app: &mut app::App, catalog: FetchedCatalog) -> Option<String> {
    let FetchedCatalog {
        client,
        artists,
        albums,
        playlists,
        cache_warning,
    } = catalog;
    app.corpus.reconcile_server(
        cache::ServerCatalog {
            artists,
            albums,
            playlists,
        },
        &client,
    );
    app.resync_results();
    cache_warning
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
    // this path's `Ok` is shown to the user verbatim, so a failed
    // cache write is said here rather than needing a channel.
    Ok(match merge_catalog_into_app(app, catalog) {
        Some(warning) => format!("{summary}; {warning}"),
        None => summary,
    })
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

/// What a fetch's outcome may and may not be read as. Named for what
/// it tests and nothing else: this module used to also hold the
/// `retain_live_clients` tests below, which are about a different
/// function on a different thread.
#[cfg(test)]
mod fetch_outcome_tests {
    use super::*;

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

    /// A server that genuinely has nothing is still a legitimate answer
    /// and must stay distinguishable from a failure, since that is the
    /// case reconciliation exists to handle.
    #[test]
    fn a_genuinely_empty_server_still_succeeds() {
        let empty: thread::Result<anyhow::Result<Vec<u8>>> = Ok(Ok(Vec::new()));
        assert_eq!(fetched(empty, "playlists").unwrap(), Vec::<u8>::new());
    }

    /// A catalog can only *fail* to be cached behind a live server, so
    /// what is reachable here is the carry: a fetch that could not be
    /// persisted still reconciles into the corpus (refusing it would
    /// leave the library empty over a cache write), and the warning
    /// comes back out for the caller to push.
    #[test]
    fn a_catalog_that_could_not_be_cached_still_merges_and_hands_the_warning_back() {
        let profile = nav_for_merge();
        let client = Arc::new(subsonic::Client::new(&profile).unwrap());
        let mut app = app::App::new(crate::search::Corpus::new());

        let warning = merge_catalog_into_app(
            &mut app,
            FetchedCatalog {
                client,
                artists: vec![subsonic::Artist {
                    id: "ar-1".into(),
                    name: "Bladee".into(),
                }],
                albums: Vec::new(),
                playlists: Vec::new(),
                cache_warning: Some("disk full".to_string()),
            },
        );
        assert_eq!(
            warning.as_deref(),
            Some("disk full"),
            "a failed cache write must reach the caller, not stop at the merge"
        );
        app.query = "bladee".into();
        app.resync_results();
        assert_eq!(
            app.result_count(),
            1,
            "the catalog itself is good; a cache write must not cost the library"
        );
    }

    fn nav_for_merge() -> config::NavidromeConfig {
        config::NavidromeConfig {
            url: "https://example.invalid".into(),
            username: "ana".into(),
            password: Some("pw".into()),
            password_file: None,
            max_bitrate_kbps: None,
            transcode_format: None,
        }
    }

    /// And the wording itself, which both call sites share: the sync
    /// path pushes it as a notification and `index_navidrome_server`
    /// appends it to the summary it returns.
    #[test]
    fn a_catalog_that_could_not_be_cached_says_so() {
        let warning = cache_warning_for(
            "https://music.example.invalid",
            &"No space left on device (os error 28)",
        );
        assert!(
            warning.contains("https://music.example.invalid"),
            "which server: {warning}"
        );
        assert!(warning.contains("next start"), "what it costs: {warning}");
        assert!(warning.contains("os error 28"), "and the cause: {warning}");
    }
}

/// Changing the configured server list, which is one step and not
/// two. Its own module: this is the seam between the main thread and
/// the sync thread, where `kept_client_tests` below is about what the
/// sync thread then does with the list it was handed.
#[cfg(test)]
mod server_list_tests {
    use super::*;

    fn nav(url: &str) -> config::NavidromeConfig {
        config::NavidromeConfig {
            url: url.into(),
            username: "ana".into(),
            password: Some("pw".into()),
            password_file: None,
            max_bitrate_kbps: None,
            transcode_format: None,
        }
    }

    fn with(servers: Vec<config::NavidromeConfig>) -> config::Config {
        config::Config {
            navidrome_servers: servers,
            ..Default::default()
        }
    }

    /// Removing a profile has to reach the sync thread, which is the
    /// only way it can stop polling one. Asserted all the way through
    /// to the client being dropped, because that is what the symptoms
    /// are: a profile you deleted still re-merging its catalog and
    /// still toasting, and its password never zeroized.
    #[test]
    fn removing_a_profile_reaches_the_sync_thread_and_drops_its_client() {
        let (tx, rx) = mpsc::channel();
        let mut cfg = with(vec![nav("https://a.invalid"), nav("https://b.invalid")]);
        let mut clients = vec![
            Arc::new(subsonic::Client::new(&nav("https://a.invalid")).unwrap()),
            Arc::new(subsonic::Client::new(&nav("https://b.invalid")).unwrap()),
        ];

        edit_servers(&mut cfg, &tx, |servers| {
            servers.remove(0);
        });

        assert_eq!(cfg.navidrome_servers.len(), 1, "the edit itself happened");
        let pushed = rx
            .try_recv()
            .expect("the sync thread is never told, so it keeps polling a deleted profile");
        assert_eq!(pushed.len(), 1);
        assert_eq!(pushed[0].url, "https://b.invalid");

        // what the thread does with it, and the reason the send
        // matters: `retain_live_clients` runs off this list every pass.
        retain_live_clients(&mut clients, &pushed);
        assert_eq!(clients.len(), 1, "the removed profile's client has to go");
        assert!(clients[0].matches(&pushed[0]));
    }

    /// And the other direction, which fails quietly rather than
    /// loudly: a profile added at runtime indexed once and was never
    /// auto-resynced, so new albums did not appear until a restart.
    #[test]
    fn adding_a_profile_reaches_the_sync_thread() {
        let (tx, rx) = mpsc::channel();
        let mut cfg = with(vec![nav("https://a.invalid")]);

        edit_servers(&mut cfg, &tx, |servers| {
            servers.push(nav("https://b.invalid"));
        });

        let pushed = rx.try_recv().expect("a new profile is never synced");
        assert_eq!(pushed.len(), 2);
        assert_eq!(pushed[1].url, "https://b.invalid");
    }

    /// The point of the closure: the list the thread is told about is
    /// the list after the edit, not a copy taken before it.
    #[test]
    fn the_pushed_list_is_the_edited_one() {
        let (tx, rx) = mpsc::channel();
        let mut cfg = with(vec![nav("https://a.invalid")]);
        edit_servers(&mut cfg, &tx, |servers| servers.clear());
        assert!(
            rx.try_recv().expect("something is pushed").is_empty(),
            "the last profile was removed, so the thread has nothing to poll"
        );
    }
}

/// The clients the sync thread keeps across passes, and when it has
/// to let one go. Split out of the fetch tests above: this is
/// `core::retain_live_clients`, which is also what zeroizes a
/// superseded password when it drops the last `Arc` to a client.
///
/// `retain_live_clients`' single production call site is
/// `spawn_background_sync`, which runs it off the `servers` list
/// `servers_rx` hands it, so **a removal is only reachable once every
/// site that mutates `cfg.navidrome_servers` goes through
/// `edit_servers`**; see its doc for what one that does not costs. An
/// *edit* of an existing profile is still unreachable at runtime for a
/// second reason, which is not this one: nothing in the ui edits a
/// profile in place, so a credential change means editing config.toml
/// and restarting.
#[cfg(test)]
mod kept_client_tests {
    use super::*;
    use crate::subsonic;

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
}
