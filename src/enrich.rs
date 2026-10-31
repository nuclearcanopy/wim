//! Background enrichment: the network answers that make the
//! suggestions menu fill in rather than only improve on the next open.
//!
//! Third step of main.rs's decomposition (see ROADMAP), and the most
//! self-contained part of its channel plumbing: one job at a time goes
//! out over `job_rx`, its answers come back over `event_tx`, and the
//! main loop's only involvement is draining them. Nothing in here
//! knows about panes, focus or the input tiers.

use std::collections::HashSet;
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;

use crate::cache::{self, CacheIndex};
use crate::subsonic;
use crate::suggest;

/// How many top-affinity artists get offered to the enrichment
/// fetcher. `suggest::Tuning::similar_seed_artists` is 5, so the two
/// network-fed generators never read past the top five; 8 is headroom
/// for the affinity ranking shifting as listening does, without
/// paying a round trip for the rest of the library.
pub const ENRICH_ARTIST_TARGETS: usize = 8;

/// `getTopSongs` rows asked for per artist. The engine's rank falloff
/// has already discounted rank 20 to under a third of rank 1, so
/// asking deeper buys nothing that could place in the menu.
pub const ENRICH_TOP_SONGS: usize = 20;

/// `getAlbum` fetches per enrichment job, and the real bound on the
/// work: one job runs at a time and the next is only picked once it
/// finishes, so opening the menu costs twelve album round trips, not
/// one per album in the library. The warm-up then continues in
/// batches for as long as the menu stays open.
pub const ENRICH_ALBUMS_PER_JOB: usize = 12;

/// Requests in flight within one job; same order of magnitude as
/// `subsonic`'s own album fetch concurrency, and for the same reason:
/// these are independent GETs and nothing should wait out twelve
/// serial latencies.
pub const ENRICH_CONCURRENCY: usize = 4;

/// One enrichment answer from the background fetcher. The engine is
/// re-run when any of these lands, so an open menu fills in rather
/// than only improving on the next open.
pub enum EnrichmentEvent {
    /// `getArtistInfo2`, recorded even when the similar list came back
    /// empty so `EnrichmentCache::needs_similar` stops asking. Enables
    /// `SimilarArtist`.
    ArtistInfo {
        id: String,
        info: subsonic::ArtistInfo,
    },
    /// `getTopSongs`. The songs are ingested as well as recorded: song
    /// titles are deliberately never eagerly indexed (see
    /// `fetch_navidrome_catalog`), so without that the returned ids
    /// would resolve against nothing and `PopularUnplayed` could not
    /// fire. Enables `PopularUnplayed`.
    TopSongs {
        artist: String,
        songs: Vec<subsonic::Song>,
        client: Arc<subsonic::Client>,
    },
    /// `getAlbum`: a real track total and real track order, which is
    /// all `TrackGap` and `AlbumNeighbour` were ever missing.
    Album {
        /// The album's *credited* artist, carried through from the
        /// target so the tracklist lands on the catalog slot it was
        /// fetched for. A compilation's track one has a different
        /// artist, which used to file it somewhere nothing read.
        artist: String,
        name: String,
        songs: Vec<subsonic::Song>,
        client: Arc<subsonic::Client>,
    },
    /// Every fetch in one job has finished, successes and failures
    /// alike; what takes the menu title back out of "so far".
    JobDone,
}

/// One bounded batch of fetches. The main thread picks the targets (it
/// owns the catalog and the play history) and the background thread
/// only performs them.
pub struct EnrichmentJob {
    pub artists: Vec<ArtistFetch>,
    albums: Vec<suggest::AlbumTarget>,
}

pub struct ArtistFetch {
    target: suggest::ArtistTarget,
    client: Arc<subsonic::Client>,
    /// Which halves are still unanswered. A negative answer counts as
    /// answered, so a server with no last.fm metadata behind it is
    /// asked once and then left alone.
    similar: bool,
    top: bool,
}

/// One independent unit of network work.
pub enum Fetch {
    Similar {
        id: String,
        client: Arc<subsonic::Client>,
    },
    Top {
        name: String,
        client: Arc<subsonic::Client>,
    },
    Album(suggest::AlbumTarget),
}

impl EnrichmentJob {
    pub fn is_empty(&self) -> bool {
        self.artists.is_empty() && self.albums.is_empty()
    }

    /// Flattened, artists first: they are at most two calls each and
    /// each one unlocks a whole generator, while album coverage only
    /// ever improves incrementally.
    fn fetches(&self) -> Vec<Fetch> {
        let mut out = Vec::new();
        for artist in &self.artists {
            if artist.similar {
                out.push(Fetch::Similar {
                    id: artist.target.id.clone(),
                    client: artist.client.clone(),
                });
            }
            if artist.top {
                out.push(Fetch::Top {
                    name: artist.target.name.clone(),
                    client: artist.client.clone(),
                });
            }
        }
        out.extend(self.albums.iter().cloned().map(Fetch::Album));
        out
    }
}

/// `None` on any failure for the artist fetches, whose
/// already-asked state the `EnrichmentCache` records on receipt.
///
/// An album fetch always reports, with an empty `songs` on failure:
/// the catalog's own `asked` flag is set from this event, so a silent
/// `None` would leave the album looking never-asked and re-offer it
/// every cycle.
pub fn run_enrichment_fetch(fetch: &Fetch) -> Option<EnrichmentEvent> {
    match fetch {
        Fetch::Similar { id, client } => Some(EnrichmentEvent::ArtistInfo {
            id: id.clone(),
            info: client.get_artist_info(id).ok()?,
        }),
        Fetch::Top { name, client } => Some(EnrichmentEvent::TopSongs {
            artist: name.clone(),
            songs: client.get_top_songs(name, ENRICH_TOP_SONGS).ok()?,
            client: client.clone(),
        }),
        Fetch::Album(target) => Some(EnrichmentEvent::Album {
            artist: target.artist.clone(),
            name: target.name.clone(),
            songs: target
                .client
                .get_album(&target.id)
                .map(|a| a.songs)
                .unwrap_or_default(),
            client: target.client.clone(),
        }),
    }
}

/// Runs forever on its own thread, performing whichever enrichment
/// fetches the main thread asked for and reporting each answer back
/// over `event_tx`. Same shape as `spawn_background_sync` and
/// `spawn_background_search`: pure network I/O here, every piece of
/// app state stays on the main thread. These are the calls behind
/// `TrackGap`, `AlbumNeighbour`, `SimilarArtist` and `PopularUnplayed`
/// (see docs/suggestions.md's host wiring section), so none of them
/// may ever happen on the render or input path.
pub fn spawn_background_enrichment(
    job_rx: mpsc::Receiver<EnrichmentJob>,
    event_tx: mpsc::Sender<EnrichmentEvent>,
) {
    thread::spawn(move || {
        while let Ok(job) = job_rx.recv() {
            let fetches = job.fetches();
            let workers = ENRICH_CONCURRENCY.min(fetches.len());
            if workers > 0 {
                let fetches = &fetches;
                thread::scope(|scope| {
                    for worker in 0..workers {
                        let event_tx = event_tx.clone();
                        scope.spawn(move || {
                            for fetch in fetches.iter().skip(worker).step_by(workers) {
                                let Some(event) = run_enrichment_fetch(fetch) else {
                                    continue;
                                };
                                if event_tx.send(event).is_err() {
                                    return; // main thread is gone (shutting down)
                                }
                            }
                        });
                    }
                });
            }
            if event_tx.send(EnrichmentEvent::JobDone).is_err() {
                return;
            }
        }
    });
}

/// The next batch worth fetching: the top-affinity artists whose
/// similar-artist/top-song answers are still unknown, plus the
/// highest-affinity played albums whose tracklist has never been asked
/// for. Pure, so the selection and ordering are testable without a
/// server.
///
/// What stops an album being asked about in a loop is the catalog's
/// own `asked` flag, set on every reply including the empty ones, so
/// a failed fetch is remembered rather than retried.
pub fn next_enrichment_job(
    catalog: &suggest::Catalog,
    cache: &CacheIndex,
    starred: &HashSet<String>,
    enrichment: &suggest::EnrichmentCache,
) -> EnrichmentJob {
    let request = suggest::Request {
        enrichment,
        ..suggest::Request::new(catalog, cache, starred, cache::now_secs())
    };
    // one model build for both halves, not two.
    let plan = suggest::enrichment_plan(&request, ENRICH_ARTIST_TARGETS, ENRICH_ALBUMS_PER_JOB);
    let artists = plan
        .artists
        .into_iter()
        .filter_map(|target| {
            let similar = enrichment.needs_similar(&target.id);
            let top = enrichment.needs_top_songs(&target.name);
            if !similar && !top {
                return None;
            }
            let client = catalog.artist_client(&target.name)?;
            Some(ArtistFetch {
                target,
                client,
                similar,
                top,
            })
        })
        .collect();
    // no over-asking and trimming any more: the engine excludes
    // already-asked albums itself, so the cap is just the cap.
    EnrichmentJob {
        artists,
        albums: plan.albums,
    }
}

#[cfg(test)]
mod enrichment_plumbing_tests {
    use super::*;
    use crate::browse::fixtures::test_client;
    use crate::search;

    /// `days` consecutive distinct days ending today, one play each.
    /// `CacheIndex::record_play` stamps `last_played_secs` off the wall
    /// clock, so the day labels have to come off the same clock or the
    /// recency decay and the day count disagree.
    fn play(cache: &mut CacheIndex, id: &str, days: u64) {
        let now = cache::now_secs();
        for back in (0..days).rev() {
            cache.record_play(id, &cache::day_of(now - back * 86_400));
        }
    }

    fn song(id: &str, artist: &str, album: &str) -> subsonic::Song {
        subsonic::Song {
            id: id.into(),
            title: format!("track {id}"),
            artist: artist.into(),
            album: album.into(),
            duration: None,
            genres: Vec::new(),
        }
    }

    fn catalog_of(
        artists: Vec<subsonic::Artist>,
        albums: Vec<subsonic::Album>,
        songs: Vec<subsonic::Song>,
    ) -> suggest::Catalog {
        let mut corpus = search::Corpus::new();
        corpus.add_subsonic_search_result(
            subsonic::SearchResult {
                artists,
                albums,
                songs,
            },
            &test_client(),
        );
        suggest::Catalog::from_corpus(&corpus)
    }

    fn two_artists() -> suggest::Catalog {
        catalog_of(
            vec![
                subsonic::Artist {
                    id: "ar-bladee".into(),
                    name: "Bladee".into(),
                },
                subsonic::Artist {
                    id: "ar-burial".into(),
                    name: "Burial".into(),
                },
            ],
            Vec::new(),
            vec![
                song("b1", "Bladee", "Eversince"),
                song("u1", "Burial", "Untrue"),
            ],
        )
    }

    /// One album per index, each with one song, so album affinity
    /// orders exactly as the play counts do.
    fn many_albums(n: usize) -> suggest::Catalog {
        catalog_of(
            Vec::new(),
            (0..n)
                .map(|i| subsonic::Album {
                    id: format!("al-{i}"),
                    name: format!("album {i}"),
                    artist: "Bladee".into(),
                })
                .collect(),
            (0..n)
                .map(|i| song(&format!("s{i}"), "Bladee", &format!("album {i}")))
                .collect(),
        )
    }

    fn job(
        catalog: &suggest::Catalog,
        cache: &CacheIndex,
        enrichment: &suggest::EnrichmentCache,
    ) -> EnrichmentJob {
        next_enrichment_job(catalog, cache, &HashSet::new(), enrichment)
    }

    #[test]
    fn the_job_takes_the_top_affinity_artists_in_order() {
        let catalog = two_artists();
        let mut cache = CacheIndex::default();
        play(&mut cache, "b1", 5);
        play(&mut cache, "u1", 2);
        let job = job(&catalog, &cache, &suggest::EnrichmentCache::new());
        let names: Vec<&str> = job.artists.iter().map(|a| a.target.name.as_str()).collect();
        assert_eq!(names, ["Bladee", "Burial"]);
        assert!(
            job.artists.iter().all(|a| a.similar && a.top),
            "nothing asked yet, so both halves are still open"
        );
    }

    #[test]
    fn an_answered_artist_is_not_asked_about_again() {
        let catalog = two_artists();
        let mut cache = CacheIndex::default();
        play(&mut cache, "b1", 5);
        play(&mut cache, "u1", 2);
        let mut enrichment = suggest::EnrichmentCache::new();
        // both halves answered, both of them negatively.
        enrichment.add_artist_info("ar-bladee", &subsonic::ArtistInfo::default());
        enrichment.add_top_songs("Bladee", &[]);
        // and one half answered for the other artist.
        enrichment.add_top_songs("Burial", &[song("x", "Burial", "Untrue")]);

        let job = job(&catalog, &cache, &enrichment);
        let names: Vec<&str> = job.artists.iter().map(|a| a.target.name.as_str()).collect();
        assert_eq!(
            names,
            ["Burial"],
            "an empty answer is an answer; re-asking it every open is unbounded cost"
        );
        assert!(job.artists[0].similar);
        assert!(!job.artists[0].top);
        assert_eq!(
            job.fetches().len(),
            1,
            "only the half that is actually still open gets fetched"
        );
    }

    #[test]
    fn the_album_cap_holds_and_asked_albums_drop_out() {
        let mut catalog = many_albums(ENRICH_ALBUMS_PER_JOB * 2);
        let mut cache = CacheIndex::default();
        // later albums are played on more days, so affinity order is
        // the reverse of the id order and a plain "first n" would fail.
        for i in 0..ENRICH_ALBUMS_PER_JOB * 2 {
            play(&mut cache, &format!("s{i}"), i as u64 + 1);
        }
        let enrichment = suggest::EnrichmentCache::new();
        let first = job(&catalog, &cache, &enrichment);
        assert_eq!(first.albums.len(), ENRICH_ALBUMS_PER_JOB);
        let names: Vec<&str> = first.albums.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(
            names[0],
            format!("album {}", ENRICH_ALBUMS_PER_JOB * 2 - 1),
            "highest affinity first: {names:?}"
        );

        // marking them asked on the catalog is what a reply does, and
        // is the only thing that stops a re-ask now.
        let asked: Vec<String> = first.albums.iter().map(|a| a.name.clone()).collect();
        for album in &first.albums {
            catalog.mark_album_asked(&album.artist, &album.name);
        }
        let second = job(&catalog, &cache, &enrichment);
        assert_eq!(second.albums.len(), ENRICH_ALBUMS_PER_JOB);
        assert!(
            second.albums.iter().all(|a| !asked.contains(&a.name)),
            "a batch never re-asks what the last one already asked"
        );

        for album in &second.albums {
            catalog.mark_album_asked(&album.artist, &album.name);
        }
        assert!(
            job(&catalog, &cache, &enrichment).is_empty(),
            "nothing left to ask means no job at all, not an empty one fired forever"
        );
    }

    #[test]
    fn artists_are_fetched_before_albums() {
        // artists are two calls each and each unlocks a whole
        // generator; album coverage only ever improves incrementally.
        let catalog = catalog_of(
            vec![subsonic::Artist {
                id: "ar-bladee".into(),
                name: "Bladee".into(),
            }],
            vec![subsonic::Album {
                id: "al-1".into(),
                name: "Eversince".into(),
                artist: "Bladee".into(),
            }],
            vec![song("b1", "Bladee", "Eversince")],
        );
        let mut cache = CacheIndex::default();
        play(&mut cache, "b1", 4);
        let fetches = job(&catalog, &cache, &suggest::EnrichmentCache::new()).fetches();
        assert!(matches!(fetches[0], Fetch::Similar { .. }));
        assert!(matches!(fetches[1], Fetch::Top { .. }));
        assert!(matches!(fetches[2], Fetch::Album(_)));
        assert_eq!(fetches.len(), 3);
    }
}
