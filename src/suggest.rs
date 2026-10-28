//! The suggestion engine behind the suggestions menu: coverage gaps in
//! the user's own library, weighted by their own affinity. There is no
//! second user's listening data here, so there is no collaborative
//! signal to mine; what there *is* is play history (`cache::CacheEntry`)
//! and library structure, which is exactly enough for "you play this
//! artist constantly and have never opened that album."
//!
//! Three stages, in order: candidate generation (one generator per
//! kind of gap, each tagging its candidates with a machine-readable
//! `Reason`), scoring (per-generator normalisation, then one weight
//! per generator), and diversification (caps plus an MMR-style penalty
//! so one artist cannot own the menu). See docs/suggestions.md for the
//! design record, the weights, and the honest limitations.
//!
//! Pure and deterministic: `now_secs` and `seed` are parameters,
//! nothing here reads a clock or a thread rng, and the one impure
//! input (last.fm-proxied similar artists, `getArtistInfo2`) arrives
//! through the `Enrichment` trait so tests fake it.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::cache::{self, CacheIndex};
use crate::search::{Corpus, SearchItem};
use crate::subsonic::{ArtistInfo, Client, Song};

// ---------------------------------------------------------------------
// tuning
// ---------------------------------------------------------------------

/// Every knob in the engine, in one place. Defaults are what the menu
/// ships with; docs/suggestions.md explains each choice.
#[derive(Debug, Clone)]
pub struct Tuning {
    /// Days after which a play counts half as much. 30 days means last
    /// year's obsession sits ~4000x below this month's, so the menu
    /// tracks what the user is into *now* without ever forgetting.
    pub half_life_days: f32,
    /// A distinct calendar day of listening is worth this much. Ten
    /// plays in one evening is one day of interest, so days are the
    /// primary frequency signal (same reasoning as the auto-download
    /// threshold in `cache.rs`).
    pub distinct_day_weight: f32,
    /// Raw play count only enters logarithmically, as a tiebreak
    /// between tracks with equal day counts.
    pub play_count_weight: f32,
    /// Flat affinity for an explicitly starred track, so an unplayed
    /// star still pulls its artist/album/genre rollups upward.
    pub starred_bonus: f32,
    /// Artist affinity an album gap needs before it is worth showing.
    pub min_artist_affinity: f32,
    /// Genre affinity the under-exploration generator needs.
    pub min_genre_affinity: f32,
    /// Album gaps only fire at or below this played-track fraction.
    pub album_gap_max_coverage: f32,
    /// Track gaps only fire at or above it. The two generators split
    /// the same axis on purpose: below this you do not know the album
    /// well enough for a specific track to be a glaring hole, and the
    /// album-neighbour generator covers that half instead.
    pub track_gap_min_coverage: f32,
    /// Undecayed frequency a track needs to count as "formerly loved".
    pub rediscovery_min_frequency: f32,
    pub rediscovery_min_age_days: f32,
    /// Distinct days a track needs before "daily rotation" applies.
    pub daily_min_days: u32,
    /// Distinct days divided by days since its first play.
    pub daily_min_day_rate: f32,
    /// How many top-affinity artists get used as similarity seeds.
    pub similar_seed_artists: usize,
    /// Each extra rank in a ranked external list (similar artists,
    /// top songs) costs this much relative score.
    pub rank_falloff: f32,
    pub weights: GeneratorWeights,
    /// Hard caps, applied across the whole menu rather than per
    /// section: ten rows from one artist is a failure even when every
    /// row scores well.
    pub max_per_artist: usize,
    pub max_per_album: usize,
    /// MMR-style multiplicative penalties per already-selected item
    /// sharing an attribute.
    pub repeat_artist_penalty: f32,
    pub repeat_album_penalty: f32,
    pub repeat_genre_penalty: f32,
    pub section_max_items: usize,
    pub max_items: usize,
    /// Per-generator candidates kept before the pools are merged; the
    /// diversification pass is quadratic, and nothing past this depth
    /// can survive the caps anyway.
    pub per_generator_candidates: usize,
}

/// Applied after each generator's own scores are normalised to 0..1, so
/// a generator that happens to produce large raw numbers cannot
/// dominate the merged list. Relative importance of the generators
/// lives here and nowhere else.
#[derive(Debug, Clone)]
pub struct GeneratorWeights {
    pub daily_rotation: f32,
    pub album_gap: f32,
    pub track_gap: f32,
    pub album_neighbour: f32,
    pub genre_under_explored: f32,
    pub rediscovery: f32,
    pub starred_unplayed: f32,
    pub similar_artist: f32,
    pub popular_unplayed: f32,
}

impl Default for GeneratorWeights {
    fn default() -> Self {
        GeneratorWeights {
            // quick access to what you are already playing every day
            // is the most reliably useful row on the screen.
            daily_rotation: 1.0,
            // the maintainer's own headline example.
            album_gap: 0.95,
            track_gap: 0.9,
            starred_unplayed: 0.85,
            rediscovery: 0.8,
            similar_artist: 0.75,
            genre_under_explored: 0.7,
            popular_unplayed: 0.65,
            // the weakest signal: adjacency on a tracklist is a real
            // but faint reason to think you would like something.
            album_neighbour: 0.6,
        }
    }
}

impl GeneratorWeights {
    fn for_reason(&self, reason: Reason) -> f32 {
        match reason {
            Reason::DailyRotation => self.daily_rotation,
            Reason::AlbumGap => self.album_gap,
            Reason::TrackGap => self.track_gap,
            Reason::AlbumNeighbour => self.album_neighbour,
            Reason::GenreUnderExplored => self.genre_under_explored,
            Reason::Rediscovery => self.rediscovery,
            Reason::StarredUnplayed => self.starred_unplayed,
            Reason::SimilarArtist => self.similar_artist,
            Reason::PopularUnplayed => self.popular_unplayed,
            // cold start replaces the whole menu, so it never competes
            // with a scored generator for a slot.
            Reason::ColdStartStarred | Reason::ColdStartSample => 1.0,
        }
    }
}

impl Default for Tuning {
    fn default() -> Self {
        Tuning {
            half_life_days: 30.0,
            distinct_day_weight: 1.0,
            play_count_weight: 0.25,
            starred_bonus: 1.0,
            min_artist_affinity: 1.0,
            min_genre_affinity: 2.0,
            album_gap_max_coverage: 0.34,
            track_gap_min_coverage: 0.5,
            rediscovery_min_frequency: 2.0,
            rediscovery_min_age_days: 60.0,
            daily_min_days: 4,
            daily_min_day_rate: 0.5,
            similar_seed_artists: 5,
            rank_falloff: 0.25,
            weights: GeneratorWeights::default(),
            max_per_artist: 3,
            max_per_album: 2,
            repeat_artist_penalty: 0.45,
            repeat_album_penalty: 0.3,
            repeat_genre_penalty: 0.8,
            section_max_items: 6,
            max_items: 36,
            per_generator_candidates: 64,
        }
    }
}

// ---------------------------------------------------------------------
// output shape
// ---------------------------------------------------------------------

/// Which generator fired, so the menu can label, group, and icon rows
/// without re-deriving anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Reason {
    DailyRotation,
    AlbumGap,
    TrackGap,
    AlbumNeighbour,
    GenreUnderExplored,
    Rediscovery,
    StarredUnplayed,
    SimilarArtist,
    PopularUnplayed,
    ColdStartStarred,
    ColdStartSample,
}

impl Reason {
    /// Section heading. Deliberately second person and concrete; the
    /// per-row numbers live in `Suggestion::reason_text`.
    pub fn title(self) -> &'static str {
        match self {
            Reason::DailyRotation => "your daily rotation",
            Reason::AlbumGap => "albums you have never opened",
            Reason::TrackGap => "the tracks you keep skipping",
            Reason::AlbumNeighbour => "right next to what you play",
            Reason::GenreUnderExplored => "unexplored corners of what you love",
            Reason::Rediscovery => "you used to play these constantly",
            Reason::StarredUnplayed => "starred, never played",
            Reason::SimilarArtist => "because you love these artists",
            Reason::PopularUnplayed => "everyone else's favourites, not yours yet",
            Reason::ColdStartStarred => "starred",
            Reason::ColdStartSample => "a place to start",
        }
    }
}

/// One row. `item` is playable/openable exactly like a search result,
/// so the menu reuses the existing enter-key handling.
#[derive(Debug, Clone)]
pub struct Suggestion {
    pub item: SearchItem,
    /// Built from the real numbers that made this fire ("4 of 5 tracks
    /// played"), never a canned string.
    pub reason_text: String,
    /// Weighted, generator-normalised score in 0..=1. Rows arrive
    /// already ordered; diversification reorders without rewriting
    /// this, so the number stays comparable across sections. Part of
    /// the documented output shape, and the one number a menu that
    /// later wants to show "why this ranked here" would read; the
    /// current menu renders rows in the order they arrive.
    #[allow(dead_code)]
    pub score: f32,
}

#[derive(Debug, Clone)]
pub struct Section {
    /// `reason.title()` at build time. The menu calls `title()` on the
    /// reason itself (it needs it for headings it renders without a
    /// section), so this is carried for hosts that only have the
    /// section.
    #[allow(dead_code)]
    pub title: String,
    pub reason: Reason,
    pub items: Vec<Suggestion>,
}

#[derive(Debug, Clone, Default)]
pub struct Suggestions {
    /// Best section first; a section is only present if it has rows.
    pub sections: Vec<Section>,
}

impl Suggestions {
    pub fn is_empty(&self) -> bool {
        self.sections.is_empty()
    }

    pub fn len(&self) -> usize {
        self.sections.iter().map(|s| s.items.len()).sum()
    }

    /// Flattened rows in display order, for a menu that renders one
    /// scrollable list with headers rather than separate panes.
    pub fn flat(&self) -> impl Iterator<Item = (Reason, &Suggestion)> {
        self.sections
            .iter()
            .flat_map(|s| s.items.iter().map(move |i| (s.reason, i)))
    }
}

// ---------------------------------------------------------------------
// injectable network half
// ---------------------------------------------------------------------

/// The only impure inputs: last.fm-proxied similar artists and
/// popularity rankings, both of which cost a network round trip. The
/// engine never fetches; the host fetches in the background and hands
/// over whatever it has, so `suggest` stays pure and instant.
pub trait Enrichment {
    /// Similar artist *names* for an artist id, most similar first.
    /// Names, not ids, because `similarArtist` entries outside the
    /// library have no usable id (see `subsonic::ArtistInfo`).
    fn similar_artists(&self, artist_id: &str) -> &[String] {
        let _ = artist_id;
        &[]
    }

    /// Track ids of an artist's most popular tracks, most popular
    /// first, keyed by the normalised artist name `getTopSongs` takes.
    fn top_songs(&self, artist_name: &str) -> &[String] {
        let _ = artist_name;
        &[]
    }
}

/// Nothing fetched yet (first open, offline, or a host that has not
/// wired the fetches up). The two network-fed generators simply do not
/// fire; everything else still does.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoEnrichment;

impl Enrichment for NoEnrichment {}

/// What the host accumulates in the background and keeps across menu
/// opens. Keyed exactly as `Enrichment` reads it, and persisted: this
/// is `getArtistInfo2`/`getTopSongs` output, which changes about as
/// often as last.fm's similarity graph does, so paying for it again on
/// every launch would be latency for nothing.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct EnrichmentCache {
    /// Artist id to similar artist names, most similar first.
    #[serde(default)]
    similar: HashMap<String, Vec<String>>,
    /// Normalised artist name to popular track ids, most popular first.
    #[serde(default)]
    top: HashMap<String, Vec<String>>,
}

/// Next to the play-history index under the cache dir, not the state
/// dir: this is reconstructable server output with nothing of the
/// user's own in it, which is exactly what `cache.rs` reserves
/// `dirs::cache_dir()` for.
fn enrichment_path() -> Result<PathBuf> {
    Ok(cache::cache_dir()?.join("enrichment.toml"))
}

impl EnrichmentCache {
    // the host loads rather than constructs; `new` is what this
    // module's own tests build their fixtures with.
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self::default()
    }

    /// Best-effort, same leniency as `cache::CacheIndex::load`: a
    /// missing or corrupt file just means nothing fetched yet, which
    /// the two network-fed generators already treat as normal.
    pub fn load() -> Self {
        enrichment_path()
            .ok()
            .and_then(|path| fs::read_to_string(path).ok())
            .and_then(|raw| toml::from_str(&raw).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<()> {
        let path = enrichment_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let toml = toml::to_string_pretty(self)?;
        fs::write(&path, toml).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    /// Records one `getArtistInfo2` response. Stored even when empty,
    /// so `needs_similar` stops asking for an artist last.fm has
    /// nothing on.
    pub fn add_artist_info(&mut self, artist_id: &str, info: &ArtistInfo) {
        self.similar.insert(
            artist_id.to_string(),
            info.similar_artists
                .iter()
                .map(|a| a.name.clone())
                .collect(),
        );
    }

    /// Records one `getTopSongs` response.
    pub fn add_top_songs(&mut self, artist_name: &str, songs: &[Song]) {
        self.top.insert(
            norm(artist_name),
            songs.iter().map(|s| s.id.clone()).collect(),
        );
    }

    pub fn needs_similar(&self, artist_id: &str) -> bool {
        !self.similar.contains_key(artist_id)
    }

    pub fn needs_top_songs(&self, artist_name: &str) -> bool {
        !self.top.contains_key(&norm(artist_name))
    }
}

impl Enrichment for EnrichmentCache {
    fn similar_artists(&self, artist_id: &str) -> &[String] {
        self.similar.get(artist_id).map_or(&[], Vec::as_slice)
    }

    fn top_songs(&self, artist_name: &str) -> &[String] {
        self.top.get(&norm(artist_name)).map_or(&[], Vec::as_slice)
    }
}

// ---------------------------------------------------------------------
// catalog
// ---------------------------------------------------------------------

/// Normalised join key. Remote `SearchItem`s carry album and artist as
/// display *names*, not ids, so names are what tracks, albums, and
/// artists get joined on; casing and stray whitespace must not split
/// one artist in two.
fn norm(s: &str) -> String {
    s.trim().to_lowercase()
}

/// `(normalised artist, normalised album)`.
type AlbumKey = (String, String);

/// One known track. `key` is the play-history key, i.e. the remote
/// song id (what `cache::CacheIndex` is keyed by) or the local path.
#[derive(Debug, Clone)]
pub struct Track {
    pub key: String,
    pub item: SearchItem,
    pub title: String,
    pub artist: String,
    /// Display name; the generators join through `track_album` instead,
    /// so this is only here to keep a `Track` self-describing.
    #[allow(dead_code)]
    pub album: String,
    /// Empty unless the track came in through `ingest_songs`/
    /// `ingest_album`; `SearchItem` does not carry genre.
    pub genres: Vec<String>,
    /// Slot on its album, only known for `ingest_album` tracks; what
    /// the album-neighbour generator needs.
    pub position: Option<usize>,
}

#[derive(Debug, Clone)]
struct AlbumEntry {
    key: AlbumKey,
    name: String,
    artist: String,
    /// `None` for an album only ever seen as a track's album name;
    /// without a `SearchItem::Album` there is no row to suggest.
    item: Option<SearchItem>,
    tracks: Vec<usize>,
    /// Real track count, known only from a full album fetch. Without
    /// it, coverage is measured against the tracks we happen to know.
    track_total: Option<usize>,
}

#[derive(Debug, Clone)]
struct ArtistEntry {
    key: String,
    name: String,
    /// Server-side id, needed to ask for similar artists.
    id: Option<String>,
    item: Option<SearchItem>,
    tracks: Vec<usize>,
    albums: Vec<usize>,
}

/// The engine's view of the library: every track, album, and artist it
/// knows about, joined together. Built by the host from whatever it
/// has (the search corpus always; full album fetches when available),
/// then handed to `suggest` by reference.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    tracks: Vec<Track>,
    track_album: Vec<Option<usize>>,
    track_artist: Vec<Option<usize>>,
    by_key: HashMap<String, usize>,
    albums: Vec<AlbumEntry>,
    by_album: HashMap<AlbumKey, usize>,
    artists: Vec<ArtistEntry>,
    by_artist: HashMap<String, usize>,
}

impl Catalog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_corpus(corpus: &Corpus) -> Self {
        let mut catalog = Self::new();
        catalog.ingest_corpus(corpus);
        catalog
    }

    /// Everything the running app has already indexed: every artist
    /// and album (eagerly fetched at login), plus whatever songs
    /// search or browsing has pulled in. No genres and no track order
    /// come from here; `SearchItem` carries neither.
    pub fn ingest_corpus(&mut self, corpus: &Corpus) {
        for item in corpus.items() {
            self.ingest_item(item);
        }
    }

    fn ingest_item(&mut self, item: &SearchItem) {
        match item {
            SearchItem::Artist { id, name, .. } => {
                let slot = self.artist_slot(name);
                let artist = &mut self.artists[slot];
                artist.id = Some(id.clone());
                artist.item = Some(item.clone());
            }
            SearchItem::Album { name, artist, .. } => {
                let slot = self.album_slot(artist, name);
                self.albums[slot].item = Some(item.clone());
                let artist_slot = self.artist_slot(artist);
                if !self.artists[artist_slot].albums.contains(&slot) {
                    self.artists[artist_slot].albums.push(slot);
                }
            }
            SearchItem::Song {
                id,
                title,
                artist,
                album,
                ..
            } => {
                self.upsert_track(id, item, title, artist, album, &[], None, None);
            }
            SearchItem::LocalMusic {
                path,
                title,
                artist,
                album,
            } => {
                // local files are never in the play-history index at
                // all (see cache.rs), so these only ever matter as
                // cold-start material.
                let key = path.to_string_lossy().into_owned();
                let artist = artist.clone().unwrap_or_default();
                let album = album.clone().unwrap_or_default();
                self.upsert_track(&key, item, title, &artist, &album, &[], None, None);
            }
            SearchItem::LocalVideo { .. } | SearchItem::Playlist { .. } => {}
        }
    }

    /// A flat batch of songs from any subsonic call (`search3`,
    /// `getStarred2`, `getTopSongs`). The cheapest way to get genre
    /// into the catalog.
    pub fn ingest_songs(&mut self, songs: &[Song], client: &Arc<Client>) {
        for song in songs {
            let item = song_item(song, client);
            self.upsert_track(
                &song.id,
                &item,
                &song.title,
                &song.artist,
                &song.album,
                &song.genres,
                None,
                None,
            );
        }
    }

    /// One album's full track list from `getAlbum`, in album order.
    /// The only ingest that yields real album coverage (a known track
    /// total) and track adjacency.
    pub fn ingest_album(&mut self, album_name: &str, songs: &[Song], client: &Arc<Client>) {
        let artist = songs.first().map(|s| s.artist.as_str()).unwrap_or_default();
        let slot = self.album_slot(artist, album_name);
        self.albums[slot].track_total = Some(songs.len());
        for (position, song) in songs.iter().enumerate() {
            let item = song_item(song, client);
            self.upsert_track(
                &song.id,
                &item,
                &song.title,
                &song.artist,
                album_name,
                &song.genres,
                Some(position),
                Some(slot),
            );
        }
    }

    /// Insert or enrich. A track can arrive first from the corpus
    /// (no genre, no position) and again from a full album fetch; the
    /// richer facts win, and nothing already known is dropped.
    #[allow(clippy::too_many_arguments)]
    fn upsert_track(
        &mut self,
        key: &str,
        item: &SearchItem,
        title: &str,
        artist: &str,
        album: &str,
        genres: &[String],
        position: Option<usize>,
        album_slot: Option<usize>,
    ) {
        let index = match self.by_key.get(key) {
            Some(&index) => {
                let track = &mut self.tracks[index];
                if !genres.is_empty() {
                    track.genres = genres.to_vec();
                }
                if position.is_some() {
                    track.position = position;
                }
                index
            }
            None => {
                let index = self.tracks.len();
                self.tracks.push(Track {
                    key: key.to_string(),
                    item: item.clone(),
                    title: title.to_string(),
                    artist: artist.to_string(),
                    album: album.to_string(),
                    genres: genres.to_vec(),
                    position,
                });
                self.track_album.push(None);
                self.track_artist.push(None);
                self.by_key.insert(key.to_string(), index);
                index
            }
        };

        if !artist.is_empty() {
            let slot = self.artist_slot(artist);
            if !self.artists[slot].tracks.contains(&index) {
                self.artists[slot].tracks.push(index);
            }
            self.track_artist[index] = Some(slot);
        }
        if !album.is_empty() {
            let slot = album_slot.unwrap_or_else(|| self.album_slot(artist, album));
            if !self.albums[slot].tracks.contains(&index) {
                self.albums[slot].tracks.push(index);
            }
            self.track_album[index] = Some(slot);
            if !artist.is_empty() {
                let artist_slot = self.artist_slot(artist);
                if !self.artists[artist_slot].albums.contains(&slot) {
                    self.artists[artist_slot].albums.push(slot);
                }
            }
        }
    }

    fn artist_slot(&mut self, name: &str) -> usize {
        let key = norm(name);
        if let Some(&slot) = self.by_artist.get(&key) {
            return slot;
        }
        let slot = self.artists.len();
        self.artists.push(ArtistEntry {
            key: key.clone(),
            name: name.to_string(),
            id: None,
            item: None,
            tracks: Vec::new(),
            albums: Vec::new(),
        });
        self.by_artist.insert(key, slot);
        slot
    }

    fn album_slot(&mut self, artist: &str, name: &str) -> usize {
        let key = (norm(artist), norm(name));
        if let Some(&slot) = self.by_album.get(&key) {
            return slot;
        }
        let slot = self.albums.len();
        self.albums.push(AlbumEntry {
            key: key.clone(),
            name: name.to_string(),
            artist: artist.to_string(),
            item: None,
            tracks: Vec::new(),
            track_total: None,
        });
        self.by_album.insert(key, slot);
        slot
    }

    /// Which server to ask about this artist. `ArtistTarget` carries
    /// only an id and a name, and a corpus spanning several navidrome
    /// servers has more than one place that id could come from, so the
    /// host resolves the client back through the catalog.
    pub fn artist_client(&self, name: &str) -> Option<Arc<Client>> {
        let slot = *self.by_artist.get(&norm(name))?;
        match self.artists[slot].item.as_ref()? {
            SearchItem::Artist { client, .. } => Some(client.clone()),
            _ => None,
        }
    }

    // the three below are assertions this module's own tests make
    // about ingest, not host api; nothing outside needs to ask the
    // catalog its size, since the host only ever hands it to
    // `suggest`.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty() && self.albums.is_empty() && self.artists.is_empty()
    }

    #[allow(dead_code)]
    pub fn track_count(&self) -> usize {
        self.tracks.len()
    }

    #[allow(dead_code)]
    pub fn artist_count(&self) -> usize {
        self.artists.len()
    }
}

fn song_item(song: &Song, client: &Arc<Client>) -> SearchItem {
    SearchItem::Song {
        id: song.id.clone(),
        title: song.title.clone(),
        artist: song.artist.clone(),
        album: song.album.clone(),
        client: client.clone(),
    }
}

// ---------------------------------------------------------------------
// affinity
// ---------------------------------------------------------------------

/// `YYYY-MM-DD` back to a day number (days since the unix epoch), the
/// inverse of `cache::day_of`. Same March-based-year trick, which is
/// what removes every month special case. `None` for anything
/// unparseable, so a hand-edited index can't panic the menu.
fn day_number(ymd: &str) -> Option<i64> {
    let mut parts = ymd.split('-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: i64 = parts.next()?.parse().ok()?;
    let day: i64 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

/// Exponential recency decay, `0.5 ^ (age / half_life)`. Never played
/// (or no timestamp recorded) decays by nothing: there is no evidence
/// of staleness to apply, and the frequency term is already 0 or just
/// the starred bonus.
fn decay(last_played_secs: Option<u64>, now_secs: u64, half_life_days: f32) -> f32 {
    let Some(last) = last_played_secs else {
        return 1.0;
    };
    let age_days = now_secs.saturating_sub(last) as f32 / 86_400.0;
    0.5f32.powf(age_days / half_life_days.max(0.001))
}

/// Distinct days dominate; raw plays only break ties. Ten plays in one
/// evening is one day of interest, which is why the day count is
/// linear and the play count is logarithmic.
fn frequency(days: u32, plays: u32, starred: bool, tuning: &Tuning) -> f32 {
    let mut f = tuning.distinct_day_weight * days as f32
        + tuning.play_count_weight * (1.0 + plays as f32).ln();
    if starred {
        f += tuning.starred_bonus;
    }
    f
}

#[derive(Debug, Clone, Copy, Default)]
struct TrackAffinity {
    /// Undecayed frequency; what "used to love this" is measured on.
    raw: f32,
    /// `raw` after recency decay; what everything ranks on.
    score: f32,
    plays: u32,
    days: u32,
    last_played_secs: Option<u64>,
    first_day: Option<i64>,
    starred: bool,
}

/// An affinity rollup over a set of tracks. Sums, not means: "how much
/// do you listen to this artist" is a total, and a long catalogue
/// genuinely does represent more listening.
#[derive(Debug, Clone, Default)]
struct Roll {
    score: f32,
    raw: f32,
    plays: u32,
    days: u32,
    played_tracks: usize,
    known_tracks: usize,
}

impl Roll {
    fn add(&mut self, track: &TrackAffinity) {
        self.score += track.score;
        self.raw += track.raw;
        self.plays += track.plays;
        self.days += track.days;
        self.known_tracks += 1;
        if track.plays > 0 {
            self.played_tracks += 1;
        }
    }

    /// Played fraction of the album/artist. `total` overrides
    /// `known_tracks` when a full fetch told us the real size.
    fn coverage(&self, total: usize) -> f32 {
        let total = total.max(self.known_tracks).max(1);
        self.played_tracks as f32 / total as f32
    }
}

/// Everything the generators read: the catalog plus its affinity
/// rollups, computed once per `suggest` call.
struct Model<'a> {
    catalog: &'a Catalog,
    tuning: &'a Tuning,
    now_secs: u64,
    today: i64,
    tracks: Vec<TrackAffinity>,
    albums: Vec<Roll>,
    artists: Vec<Roll>,
    genres: HashMap<String, Roll>,
    genre_names: HashMap<String, String>,
    genre_artists: HashMap<String, Vec<usize>>,
    total_plays: u32,
}

impl<'a> Model<'a> {
    fn build(req: &Request<'a>) -> Model<'a> {
        let catalog = req.catalog;
        let tuning = req.tuning;
        let mut tracks = Vec::with_capacity(catalog.tracks.len());
        let mut total_plays = 0u32;
        for track in &catalog.tracks {
            let entry = req.cache.entry(&track.key);
            let plays = entry.map_or(0, |e| e.play_count);
            let days = entry.map_or(0, |e| e.play_days.len() as u32);
            let last_played_secs = entry.and_then(|e| e.last_played_secs);
            let first_day = entry
                .and_then(|e| e.play_days.first())
                .and_then(|d| day_number(d));
            let starred = req.starred.contains(&track.key);
            let raw = frequency(days, plays, starred, tuning);
            total_plays += plays;
            tracks.push(TrackAffinity {
                raw,
                score: raw * decay(last_played_secs, req.now_secs, tuning.half_life_days),
                plays,
                days,
                last_played_secs,
                first_day,
                starred,
            });
        }

        let mut albums = vec![Roll::default(); catalog.albums.len()];
        for (slot, album) in catalog.albums.iter().enumerate() {
            for &index in &album.tracks {
                albums[slot].add(&tracks[index]);
            }
        }
        let mut artists = vec![Roll::default(); catalog.artists.len()];
        for (slot, artist) in catalog.artists.iter().enumerate() {
            for &index in &artist.tracks {
                artists[slot].add(&tracks[index]);
            }
        }

        let mut genres: HashMap<String, Roll> = HashMap::new();
        let mut genre_names: HashMap<String, String> = HashMap::new();
        let mut genre_artists: HashMap<String, Vec<usize>> = HashMap::new();
        for (index, track) in catalog.tracks.iter().enumerate() {
            for genre in &track.genres {
                let key = norm(genre);
                if key.is_empty() {
                    continue;
                }
                genres.entry(key.clone()).or_default().add(&tracks[index]);
                genre_names
                    .entry(key.clone())
                    .or_insert_with(|| genre.clone());
                if let Some(artist) = catalog.track_artist[index] {
                    let owned = genre_artists.entry(key).or_default();
                    if !owned.contains(&artist) {
                        owned.push(artist);
                    }
                }
            }
        }

        Model {
            catalog,
            tuning,
            now_secs: req.now_secs,
            today: (req.now_secs / 86_400) as i64,
            tracks,
            albums,
            artists,
            genres,
            genre_names,
            genre_artists,
            total_plays,
        }
    }

    fn artist_score(&self, slot: Option<usize>) -> f32 {
        slot.map_or(0.0, |s| self.artists[s].score)
    }

    fn age_days(&self, last_played_secs: Option<u64>) -> Option<f32> {
        last_played_secs.map(|last| self.now_secs.saturating_sub(last) as f32 / 86_400.0)
    }

    /// Days since this track's first recorded play, inclusive, which is
    /// the window a distinct-day rate is measured over. Bounded in
    /// practice by `cache::MAX_TRACKED_DAYS`.
    fn span_days(&self, track: &TrackAffinity) -> Option<i64> {
        let first = track.first_day?;
        Some((self.today - first + 1).max(1))
    }
}

// ---------------------------------------------------------------------
// entry point
// ---------------------------------------------------------------------

/// Everything `suggest` reads. Borrowed, so the caller keeps owning
/// the corpus/cache it already has.
pub struct Request<'a> {
    pub catalog: &'a Catalog,
    pub cache: &'a CacheIndex,
    /// Starred keys: remote song ids (`subsonic::Client::get_starred_songs`)
    /// and local favorite paths (`favorites::Favorites::paths`) in one
    /// set, matching `Track::key`.
    pub starred: &'a HashSet<String>,
    /// Unix seconds. A parameter, never a clock read, so every score
    /// in here is reproducible in a test.
    pub now_secs: u64,
    /// Only used by the cold-start stratified sample, so a brand new
    /// library does not show the same three rows every open.
    pub seed: u64,
    pub enrichment: &'a dyn Enrichment,
    pub tuning: &'a Tuning,
}

impl<'a> Request<'a> {
    /// The common case: default tuning, no enrichment fetched yet.
    pub fn new(
        catalog: &'a Catalog,
        cache: &'a CacheIndex,
        starred: &'a HashSet<String>,
        now_secs: u64,
    ) -> Request<'a> {
        Request {
            catalog,
            cache,
            starred,
            now_secs,
            seed: now_secs / 86_400,
            enrichment: &NoEnrichment,
            tuning: &DEFAULT_TUNING,
        }
    }
}

/// So `Request::new` can hand out a `&'static Tuning` without the
/// caller having to keep one alive.
static DEFAULT_TUNING: Tuning = Tuning {
    half_life_days: 30.0,
    distinct_day_weight: 1.0,
    play_count_weight: 0.25,
    starred_bonus: 1.0,
    min_artist_affinity: 1.0,
    min_genre_affinity: 2.0,
    album_gap_max_coverage: 0.34,
    track_gap_min_coverage: 0.5,
    rediscovery_min_frequency: 2.0,
    rediscovery_min_age_days: 60.0,
    daily_min_days: 4,
    daily_min_day_rate: 0.5,
    similar_seed_artists: 5,
    rank_falloff: 0.25,
    weights: GeneratorWeights {
        daily_rotation: 1.0,
        album_gap: 0.95,
        track_gap: 0.9,
        album_neighbour: 0.6,
        genre_under_explored: 0.7,
        rediscovery: 0.8,
        starred_unplayed: 0.85,
        similar_artist: 0.75,
        popular_unplayed: 0.65,
    },
    max_per_artist: 3,
    max_per_album: 2,
    repeat_artist_penalty: 0.45,
    repeat_album_penalty: 0.3,
    repeat_genre_penalty: 0.8,
    section_max_items: 6,
    max_items: 36,
    per_generator_candidates: 64,
};

/// One candidate row before scoring and diversification.
#[derive(Debug, Clone)]
struct Candidate {
    reason: Reason,
    item: SearchItem,
    identity: (u8, String),
    /// Generator-local, pre-normalisation.
    raw: f32,
    /// Set by `normalise`.
    score: f32,
    reason_text: String,
    artist_key: String,
    album_key: Option<AlbumKey>,
    genre_key: Option<String>,
}

impl Candidate {
    fn new(reason: Reason, item: &SearchItem, raw: f32, reason_text: String) -> Candidate {
        let (kind, id) = item.identity();
        Candidate {
            reason,
            item: item.clone(),
            identity: (kind, id.into_owned()),
            raw,
            score: 0.0,
            reason_text,
            artist_key: String::new(),
            album_key: None,
            genre_key: None,
        }
    }

    fn attributes(
        mut self,
        artist_key: String,
        album_key: Option<AlbumKey>,
        genre_key: Option<String>,
    ) -> Candidate {
        self.artist_key = artist_key;
        self.album_key = album_key;
        self.genre_key = genre_key;
        self
    }
}

/// The whole engine. Pure: same inputs, same output, every time.
pub fn suggest(req: &Request<'_>) -> Suggestions {
    let model = Model::build(req);
    if model.total_plays == 0 {
        // nothing played, ever: there is no affinity to weight a gap
        // by, so every scored generator would be guessing.
        return cold_start(&model, req);
    }

    // stage 1: candidate generation, one pool per generator.
    let pools = vec![
        daily_rotation(&model),
        album_gap(&model),
        track_gap(&model),
        album_neighbours(&model),
        genre_under_explored(&model),
        rediscovery(&model),
        starred_unplayed(&model),
        similar_artists(&model, req.enrichment),
        popular_unplayed(&model, req.enrichment),
    ];

    // stage 2: scoring. normalise inside each pool first, so a
    // generator with naturally large raw numbers cannot dominate the
    // merged list, then apply that generator's one weight.
    let mut merged: Vec<Candidate> = Vec::new();
    for mut pool in pools {
        normalise(&mut pool, req.tuning);
        pool.truncate(req.tuning.per_generator_candidates);
        merged.extend(pool);
    }
    dedup(&mut merged);

    // stage 3: diversification.
    let picked = diversify(merged, req.tuning);
    let sections = group(picked, req.tuning);

    if sections.is_empty() {
        // a library with history but no gaps worth showing (tiny, or
        // fully explored) must still not render an empty screen.
        return cold_start(&model, req);
    }
    Suggestions { sections }
}

/// Top-affinity artists with a server-side id, which is what the host
/// should fetch `getArtistInfo2`/`getTopSongs` for before the next
/// open. Exposed separately so the fetching stays out of the engine.
pub fn enrichment_targets(req: &Request<'_>, limit: usize) -> Vec<ArtistTarget> {
    let model = Model::build(req);
    top_artists(&model, limit)
        .into_iter()
        .filter_map(|slot| {
            let artist = &model.catalog.artists[slot];
            Some(ArtistTarget {
                id: artist.id.clone()?,
                name: artist.name.clone(),
            })
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtistTarget {
    pub id: String,
    pub name: String,
}

/// Albums worth one `getAlbum` round trip, highest album affinity
/// first. An album is only a target if the user has played something
/// from it and we do not know its real tracklist yet: without a track
/// total `TrackGap` cannot tell a glaring hole from a thin index, and
/// without track order `AlbumNeighbour` cannot fire at all.
///
/// The "has a play" filter is also what bounds a warm-up on a large
/// library: you have only ever played from a fraction of it, and the
/// rest is already covered by `AlbumGap`, which needs no fetch.
pub fn album_targets(req: &Request<'_>, limit: usize) -> Vec<AlbumTarget> {
    let model = Model::build(req);
    let mut slots: Vec<usize> = (0..model.catalog.albums.len())
        .filter(|&slot| {
            let album = &model.catalog.albums[slot];
            album.track_total.is_none()
                && album.item.is_some()
                && model.albums[slot].played_tracks > 0
        })
        .collect();
    slots.sort_by(|&a, &b| {
        model.albums[b]
            .score
            .total_cmp(&model.albums[a].score)
            .then_with(|| {
                model.catalog.albums[a]
                    .key
                    .cmp(&model.catalog.albums[b].key)
            })
    });
    slots.truncate(limit);
    slots
        .into_iter()
        .filter_map(|slot| {
            let album = &model.catalog.albums[slot];
            match album.item.as_ref()? {
                SearchItem::Album { id, client, .. } => Some(AlbumTarget {
                    id: id.clone(),
                    name: album.name.clone(),
                    client: client.clone(),
                }),
                _ => None,
            }
        })
        .collect()
}

#[derive(Debug, Clone)]
pub struct AlbumTarget {
    pub id: String,
    /// The name the album is already known by, which is the one
    /// `Catalog::ingest_album` must be fed so the fetched tracklist
    /// lands on the existing album rather than beside it.
    pub name: String,
    pub client: Arc<Client>,
}

/// Artist slots by decayed affinity, best first, ties broken on the
/// normalised name so the order never depends on hash iteration.
fn top_artists(model: &Model<'_>, limit: usize) -> Vec<usize> {
    let mut slots: Vec<usize> = (0..model.catalog.artists.len())
        .filter(|&s| model.artists[s].score > 0.0)
        .collect();
    slots.sort_by(|&a, &b| {
        model.artists[b]
            .score
            .total_cmp(&model.artists[a].score)
            .then_with(|| {
                model.catalog.artists[a]
                    .key
                    .cmp(&model.catalog.artists[b].key)
            })
    });
    slots.truncate(limit);
    slots
}

// ---------------------------------------------------------------------
// stage 1: generators
// ---------------------------------------------------------------------

/// Tracks played on a high fraction of the days since you first played
/// them. Not discovery: quick access to what is already in rotation.
fn daily_rotation(model: &Model<'_>) -> Vec<Candidate> {
    let mut out = Vec::new();
    for index in 0..model.catalog.tracks.len() {
        let affinity = &model.tracks[index];
        if affinity.plays == 0 || affinity.days < model.tuning.daily_min_days {
            continue;
        }
        let Some(span) = model.span_days(affinity) else {
            continue;
        };
        let rate = affinity.days as f32 / span as f32;
        if rate < model.tuning.daily_min_day_rate {
            continue;
        }
        let text = format!(
            "played on {} of the last {span} days",
            affinity.days.min(span as u32)
        );
        out.push(track_candidate(
            model,
            Reason::DailyRotation,
            index,
            rate * affinity.days as f32,
            text,
        ));
    }
    out
}

/// High artist affinity, album barely touched. The maintainer's
/// headline example: "you play Bladee a lot and have never played
/// Crest."
fn album_gap(model: &Model<'_>) -> Vec<Candidate> {
    let mut out = Vec::new();
    for (slot, album) in model.catalog.albums.iter().enumerate() {
        let Some(item) = &album.item else {
            continue;
        };
        let artist_slot = model.catalog.by_artist.get(&album.key.0).copied();
        let artist_score = model.artist_score(artist_slot);
        if artist_score < model.tuning.min_artist_affinity {
            continue;
        }
        let roll = &model.albums[slot];
        let total = album.track_total.unwrap_or(roll.known_tracks);
        let coverage = roll.coverage(total);
        if coverage > model.tuning.album_gap_max_coverage {
            continue;
        }
        let artist_roll = artist_slot.map(|s| &model.artists[s]);
        let plays = artist_roll.map_or(0, |r| r.plays);
        let days = artist_roll.map_or(0, |r| r.days);
        let text = if roll.played_tracks == 0 && total == 0 {
            format!(
                "{plays} {} plays across {days} days, nothing from this album",
                album.artist
            )
        } else if roll.played_tracks == 0 {
            format!(
                "{plays} {} plays across {days} days, 0 of {total} tracks from this album",
                album.artist
            )
        } else {
            format!(
                "only {} of {total} tracks played, and {plays} {} plays elsewhere",
                roll.played_tracks, album.artist
            )
        };
        out.push(
            Candidate::new(
                Reason::AlbumGap,
                item,
                artist_score * (1.0 - coverage),
                text,
            )
            .attributes(album.key.0.clone(), Some(album.key.clone()), None),
        );
    }
    out
}

/// High album affinity, specific unplayed tracks. "4 of 5 tracks
/// played" is exactly the signal; the fuller the album, the more
/// glaring the hole.
fn track_gap(model: &Model<'_>) -> Vec<Candidate> {
    let mut out = Vec::new();
    for (slot, album) in model.catalog.albums.iter().enumerate() {
        let roll = &model.albums[slot];
        if roll.score <= 0.0 || roll.played_tracks == 0 {
            continue;
        }
        let total = album.track_total.unwrap_or(roll.known_tracks).max(1);
        if roll.played_tracks >= total {
            continue;
        }
        let coverage = roll.coverage(total);
        if coverage < model.tuning.track_gap_min_coverage {
            continue;
        }
        for &index in &album.tracks {
            if model.tracks[index].plays > 0 {
                continue;
            }
            let text = format!(
                "{} of {total} tracks played from {}, never this one",
                roll.played_tracks, album.name
            );
            out.push(track_candidate(
                model,
                Reason::TrackGap,
                index,
                roll.score * coverage,
                text,
            ));
        }
    }
    out
}

/// Unplayed tracks sitting immediately next to heavily played ones on
/// the same album. Needs track order, so only albums ingested whole
/// can fire this.
fn album_neighbours(model: &Model<'_>) -> Vec<Candidate> {
    let mut out = Vec::new();
    for album in &model.catalog.albums {
        let mut by_position: HashMap<usize, usize> = HashMap::new();
        for &index in &album.tracks {
            if let Some(position) = model.catalog.tracks[index].position {
                by_position.insert(position, index);
            }
        }
        for &index in &album.tracks {
            if model.tracks[index].plays > 0 {
                continue;
            }
            let Some(position) = model.catalog.tracks[index].position else {
                continue;
            };
            let best = [position.checked_sub(1), Some(position + 1)]
                .into_iter()
                .flatten()
                .filter_map(|p| by_position.get(&p))
                .map(|&n| (model.tracks[n].score, n))
                .filter(|(score, n)| *score > 0.0 && model.tracks[*n].plays > 0)
                .max_by(|a, b| a.0.total_cmp(&b.0));
            let Some((score, neighbour)) = best else {
                continue;
            };
            let text = format!(
                "sits right next to \"{}\", which you played on {} days",
                model.catalog.tracks[neighbour].title, model.tracks[neighbour].days
            );
            out.push(track_candidate(
                model,
                Reason::AlbumNeighbour,
                index,
                score,
                text,
            ));
        }
    }
    out
}

/// Within a genre you play a lot, the artist you own but play least.
/// The "least-played metal artist" case.
fn genre_under_explored(model: &Model<'_>) -> Vec<Candidate> {
    let mut out = Vec::new();
    let mut keys: Vec<&String> = model.genres.keys().collect();
    keys.sort(); // deterministic: hashmap iteration order is not
    for key in keys {
        let roll = &model.genres[key];
        if roll.score < model.tuning.min_genre_affinity {
            continue;
        }
        let Some(artists) = model.genre_artists.get(key) else {
            continue;
        };
        if artists.len() < 2 {
            // one artist is not a genre you can under-explore.
            continue;
        }
        let top = artists
            .iter()
            .map(|&slot| model.artists[slot].score)
            .fold(0.0f32, f32::max);
        if top <= 0.0 {
            continue;
        }
        let least = artists
            .iter()
            .filter(|&&slot| model.catalog.artists[slot].item.is_some())
            .map(|&slot| (model.artists[slot].score, slot))
            .min_by(|a, b| {
                a.0.total_cmp(&b.0).then_with(|| {
                    model.catalog.artists[a.1]
                        .key
                        .cmp(&model.catalog.artists[b.1].key)
                })
            });
        let Some((least_score, slot)) = least else {
            continue;
        };
        if least_score >= top {
            continue;
        }
        let artist = &model.catalog.artists[slot];
        let item = artist.item.as_ref().expect("filtered on item above");
        let genre = model
            .genre_names
            .get(key)
            .map_or(key.as_str(), String::as_str);
        let text = format!(
            "{genre} is one of your most played ({} plays across {} days); {} is the one you play least ({} plays)",
            roll.plays, roll.days, artist.name, model.artists[slot].plays
        );
        out.push(
            Candidate::new(
                Reason::GenreUnderExplored,
                item,
                roll.score * (1.0 - least_score / top),
                text,
            )
            .attributes(artist.key.clone(), None, Some(key.clone())),
        );
    }
    out
}

/// Formerly high affinity, nothing recent. "You used to play this
/// constantly."
fn rediscovery(model: &Model<'_>) -> Vec<Candidate> {
    let mut out = Vec::new();
    for index in 0..model.catalog.tracks.len() {
        let affinity = &model.tracks[index];
        if affinity.plays == 0 || affinity.raw < model.tuning.rediscovery_min_frequency {
            continue;
        }
        let Some(age) = model.age_days(affinity.last_played_secs) else {
            continue;
        };
        if age < model.tuning.rediscovery_min_age_days {
            continue;
        }
        let staleness = 1.0
            - decay(
                affinity.last_played_secs,
                model.now_secs,
                model.tuning.half_life_days,
            );
        let text = format!(
            "played on {} days, nothing in the last {}",
            affinity.days,
            age_phrase(age)
        );
        out.push(track_candidate(
            model,
            Reason::Rediscovery,
            index,
            affinity.raw * staleness,
            text,
        ));
    }
    out
}

/// Explicit interest, zero follow-through.
fn starred_unplayed(model: &Model<'_>) -> Vec<Candidate> {
    let mut out = Vec::new();
    for index in 0..model.catalog.tracks.len() {
        let affinity = &model.tracks[index];
        if !affinity.starred || affinity.plays > 0 {
            continue;
        }
        let artist_slot = model.catalog.track_artist[index];
        let artist_roll = artist_slot.map(|s| &model.artists[s]);
        let other_plays = artist_roll.map_or(0, |r| r.plays);
        let text = if other_plays > 0 {
            format!(
                "starred and never played, while you played {other_plays} other {} tracks",
                model.catalog.tracks[index].artist
            )
        } else {
            "starred and never played, not once".to_string()
        };
        out.push(track_candidate(
            model,
            Reason::StarredUnplayed,
            index,
            1.0 + model.artist_score(artist_slot),
            text,
        ));
    }
    out
}

/// last.fm's own similar-artist graph (via `getArtistInfo2`), seeded
/// from top-affinity artists and intersected with what is actually in
/// the library. Artists already played are not a gap.
fn similar_artists(model: &Model<'_>, enrichment: &dyn Enrichment) -> Vec<Candidate> {
    let mut out = Vec::new();
    let mut seen: HashSet<usize> = HashSet::new();
    for seed_slot in top_artists(model, model.tuning.similar_seed_artists) {
        let seed = &model.catalog.artists[seed_slot];
        let Some(seed_id) = &seed.id else {
            continue;
        };
        let seed_roll = &model.artists[seed_slot];
        for (rank, name) in enrichment.similar_artists(seed_id).iter().enumerate() {
            let Some(&slot) = model.catalog.by_artist.get(&norm(name)) else {
                continue;
            };
            if model.artists[slot].score > 0.0 || !seen.insert(slot) {
                continue;
            }
            let artist = &model.catalog.artists[slot];
            let Some(item) = &artist.item else {
                continue;
            };
            let text = format!(
                "last.fm puts them next to {}, who you played on {} days; you have never played them",
                seed.name, seed_roll.days
            );
            out.push(
                Candidate::new(
                    Reason::SimilarArtist,
                    item,
                    seed_roll.score / (1.0 + rank as f32 * model.tuning.rank_falloff),
                    text,
                )
                .attributes(artist.key.clone(), None, None),
            );
        }
    }
    out
}

/// `getTopSongs`: an artist you love has a famous track you have never
/// played. Coverage alone cannot rank which hole in a catalogue matters.
fn popular_unplayed(model: &Model<'_>, enrichment: &dyn Enrichment) -> Vec<Candidate> {
    let mut out = Vec::new();
    for seed_slot in top_artists(model, model.tuning.similar_seed_artists) {
        let artist = &model.catalog.artists[seed_slot];
        let roll = &model.artists[seed_slot];
        for (rank, id) in enrichment.top_songs(&artist.name).iter().enumerate() {
            let Some(&index) = model.catalog.by_key.get(id) else {
                continue;
            };
            if model.tracks[index].plays > 0 {
                continue;
            }
            let text = format!(
                "one of {}'s most played tracks everywhere; you played {} of their others, never this",
                artist.name, roll.plays
            );
            out.push(track_candidate(
                model,
                Reason::PopularUnplayed,
                index,
                roll.score / (1.0 + rank as f32 * model.tuning.rank_falloff),
                text,
            ));
        }
    }
    out
}

/// Candidate for a track index, with the diversification attributes
/// filled in from the catalog.
fn track_candidate(
    model: &Model<'_>,
    reason: Reason,
    index: usize,
    raw: f32,
    reason_text: String,
) -> Candidate {
    let track = &model.catalog.tracks[index];
    Candidate::new(reason, &track.item, raw, reason_text).attributes(
        norm(&track.artist),
        model.catalog.track_album[index].map(|s| model.catalog.albums[s].key.clone()),
        track.genres.first().map(|g| norm(g)),
    )
}

/// "3 days" / "4 months" / "2 years", for reason text. Rounded down,
/// because overstating how long it has been reads as wrong.
fn age_phrase(age_days: f32) -> String {
    let days = age_days.max(0.0) as u64;
    match days {
        0..=59 => format!("{days} days"),
        60..=729 => {
            let months = days / 30;
            format!("{months} months")
        }
        _ => {
            let years = days / 365;
            format!("{years} years")
        }
    }
}

// ---------------------------------------------------------------------
// stage 2: scoring
// ---------------------------------------------------------------------

/// Per-generator normalisation to 0..=1 against that generator's own
/// best candidate, then the generator's weight. Without this, whichever
/// generator happens to produce the largest raw numbers wins the whole
/// menu for reasons that have nothing to do with how good its
/// suggestions are. Sorts best-first as a side effect.
fn normalise(pool: &mut [Candidate], tuning: &Tuning) {
    let Some(reason) = pool.first().map(|c| c.reason) else {
        return;
    };
    let weight = tuning.weights.for_reason(reason);
    let max = pool.iter().map(|c| c.raw).fold(0.0f32, f32::max);
    for candidate in pool.iter_mut() {
        // an all-zero pool (every candidate equally weak, e.g. starred
        // tracks by artists with no history) still deserves a place,
        // just below any generator with real signal behind it.
        candidate.score = if max > f32::EPSILON {
            weight * candidate.raw / max
        } else {
            weight * 0.5
        };
    }
    pool.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.identity.cmp(&b.identity))
    });
}

/// The same track can be both a track gap and an album neighbour. Keep
/// the highest-scoring reason for it and drop the rest, so no row
/// appears twice in the menu.
fn dedup(merged: &mut Vec<Candidate>) {
    merged.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.reason.cmp(&b.reason))
            .then_with(|| a.identity.cmp(&b.identity))
    });
    let mut seen: HashSet<(u8, String)> = HashSet::new();
    merged.retain(|c| seen.insert(c.identity.clone()));
}

// ---------------------------------------------------------------------
// stage 3: diversification
// ---------------------------------------------------------------------

/// Greedy MMR-ish selection: hard caps per artist and per album, plus a
/// multiplicative penalty for every already-selected row sharing an
/// artist, album, or genre. Ten rows from one artist is a failure even
/// when every one of them scores well.
fn diversify(pool: Vec<Candidate>, tuning: &Tuning) -> Vec<Candidate> {
    let mut remaining: Vec<Option<Candidate>> = pool.into_iter().map(Some).collect();
    let mut picked: Vec<Candidate> = Vec::new();
    let mut artists: HashMap<String, usize> = HashMap::new();
    let mut albums: HashMap<AlbumKey, usize> = HashMap::new();
    let mut genres: HashMap<String, usize> = HashMap::new();
    let mut sections: HashMap<Reason, usize> = HashMap::new();

    while picked.len() < tuning.max_items {
        let mut best: Option<(usize, f32)> = None;
        for (index, slot) in remaining.iter().enumerate() {
            let Some(candidate) = slot else {
                continue;
            };
            let artist_count = count(&artists, &candidate.artist_key);
            if !candidate.artist_key.is_empty() && artist_count >= tuning.max_per_artist {
                continue;
            }
            let album_count = candidate
                .album_key
                .as_ref()
                .map_or(0, |k| albums.get(k).copied().unwrap_or(0));
            if candidate.album_key.is_some() && album_count >= tuning.max_per_album {
                continue;
            }
            if sections.get(&candidate.reason).copied().unwrap_or(0) >= tuning.section_max_items {
                continue;
            }
            let genre_count = candidate
                .genre_key
                .as_ref()
                .map_or(0, |k| count(&genres, k))
                .min(4);
            let adjusted = candidate.score
                * tuning.repeat_artist_penalty.powi(artist_count as i32)
                * tuning.repeat_album_penalty.powi(album_count as i32)
                * tuning.repeat_genre_penalty.powi(genre_count as i32);
            // ties fall back to pool order, which `dedup` already made
            // total, so selection is deterministic.
            if best.is_none_or(|(_, b)| adjusted > b) {
                best = Some((index, adjusted));
            }
        }
        let Some((index, _)) = best else {
            break;
        };
        let candidate = remaining[index].take().expect("just selected");
        if !candidate.artist_key.is_empty() {
            *artists.entry(candidate.artist_key.clone()).or_default() += 1;
        }
        if let Some(key) = &candidate.album_key {
            *albums.entry(key.clone()).or_default() += 1;
        }
        if let Some(key) = &candidate.genre_key {
            *genres.entry(key.clone()).or_default() += 1;
        }
        *sections.entry(candidate.reason).or_default() += 1;
        picked.push(candidate);
    }
    picked
}

fn count(counts: &HashMap<String, usize>, key: &str) -> usize {
    counts.get(key).copied().unwrap_or(0)
}

/// Buckets the selected rows by reason, keeping both the row order and
/// the section order the selection pass produced (so the strongest
/// section leads).
fn group(picked: Vec<Candidate>, tuning: &Tuning) -> Vec<Section> {
    let mut sections: Vec<Section> = Vec::new();
    for candidate in picked {
        let slot = match sections.iter().position(|s| s.reason == candidate.reason) {
            Some(slot) => slot,
            None => {
                sections.push(Section {
                    title: candidate.reason.title().to_string(),
                    reason: candidate.reason,
                    items: Vec::new(),
                });
                sections.len() - 1
            }
        };
        if sections[slot].items.len() >= tuning.section_max_items {
            continue;
        }
        sections[slot].items.push(Suggestion {
            item: candidate.item,
            reason_text: candidate.reason_text,
            score: candidate.score,
        });
    }
    sections
}

// ---------------------------------------------------------------------
// cold start
// ---------------------------------------------------------------------

/// No history at all (or history that produced nothing worth showing).
/// Starred items first, since those are real stated interest, then a
/// stratified sample across artists so the screen is never empty and
/// never the same three rows twice.
fn cold_start(model: &Model<'_>, req: &Request<'_>) -> Suggestions {
    let catalog = model.catalog;
    let mut sections = Vec::new();

    let mut starred: Vec<usize> = (0..catalog.tracks.len())
        .filter(|&i| model.tracks[i].starred)
        .collect();
    starred.sort_by(|&a, &b| catalog.tracks[a].key.cmp(&catalog.tracks[b].key));
    shuffle(&mut starred, req.seed);
    if !starred.is_empty() {
        let items = starred
            .into_iter()
            .take(req.tuning.section_max_items)
            .map(|index| Suggestion {
                item: catalog.tracks[index].item.clone(),
                reason_text: format!("starred, by {}", catalog.tracks[index].artist),
                score: 1.0,
            })
            .collect();
        sections.push(Section {
            title: Reason::ColdStartStarred.title().to_string(),
            reason: Reason::ColdStartStarred,
            items,
        });
    }

    // one row per artist, round-robin over a seeded artist order:
    // picking the globally "first" N rows would show the same three
    // albums by the same artist on every open.
    let mut by_artist: Vec<(String, Vec<SearchItem>)> = Vec::new();
    let mut index_of: HashMap<String, usize> = HashMap::new();
    let mut push = |artist_key: &str, item: &SearchItem| {
        let slot = *index_of
            .entry(artist_key.to_string())
            .or_insert_with(|| by_artist.len());
        if slot == by_artist.len() {
            by_artist.push((artist_key.to_string(), Vec::new()));
        }
        by_artist[slot].1.push(item.clone());
    };
    for album in &catalog.albums {
        if let Some(item) = &album.item {
            push(&album.key.0, item);
        }
    }
    for track in &catalog.tracks {
        push(&norm(&track.artist), &track.item);
    }

    let artist_total = catalog.artists.len().max(by_artist.len());
    let mut order: Vec<usize> = (0..by_artist.len()).collect();
    order.sort_by(|&a, &b| by_artist[a].0.cmp(&by_artist[b].0));
    shuffle(&mut order, req.seed);
    for (_, items) in by_artist.iter_mut() {
        shuffle(items, req.seed);
    }

    let mut sampled: Vec<Suggestion> = Vec::new();
    let depth = by_artist.iter().map(|(_, i)| i.len()).max().unwrap_or(0);
    'outer: for round in 0..depth {
        for &slot in &order {
            let (artist, items) = &by_artist[slot];
            let Some(item) = items.get(round) else {
                continue;
            };
            sampled.push(Suggestion {
                item: item.clone(),
                reason_text: format!(
                    "nothing played yet; {} is 1 of {artist_total} artists you have",
                    if artist.is_empty() { "this" } else { artist }
                ),
                score: 0.5,
            });
            if sampled.len() >= req.tuning.section_max_items {
                break 'outer;
            }
        }
    }
    if !sampled.is_empty() {
        sections.push(Section {
            title: Reason::ColdStartSample.title().to_string(),
            reason: Reason::ColdStartSample,
            items: sampled,
        });
    }

    Suggestions { sections }
}

/// Seeded Fisher-Yates over splitmix64. Hand-rolled rather than
/// pulling `rand` in here so the sequence is pinned by the seed alone,
/// independent of any crate's generator changing between versions;
/// same seed, same menu.
fn shuffle<T>(items: &mut [T], seed: u64) {
    let mut state = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut next = move || {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    };
    for i in (1..items.len()).rev() {
        let j = (next() % (i as u64 + 1)) as usize;
        items.swap(i, j);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::day_of;
    use crate::config::NavidromeConfig;

    const DAY: u64 = 86_400;
    /// Fixed "now" for every test: 2026-01-01, so decay ages and day
    /// labels are both exact and nothing reads a clock.
    const NOW: u64 = 1_767_225_600;

    fn client() -> Arc<Client> {
        Arc::new(
            Client::new(&NavidromeConfig {
                url: "https://example.invalid".into(),
                username: "test".into(),
                password: Some("pw".into()),
                password_file: None,
                max_bitrate_kbps: None,
                transcode_format: None,
            })
            .unwrap(),
        )
    }

    fn song(id: &str, title: &str, artist: &str, album: &str, genres: &[&str]) -> Song {
        Song {
            id: id.into(),
            title: title.into(),
            artist: artist.into(),
            album: album.into(),
            duration: Some(180),
            genres: genres.iter().map(|g| g.to_string()).collect(),
        }
    }

    fn album_item(id: &str, name: &str, artist: &str) -> SearchItem {
        SearchItem::Album {
            id: id.into(),
            name: name.into(),
            artist: artist.into(),
            client: client(),
        }
    }

    fn artist_item(id: &str, name: &str) -> SearchItem {
        SearchItem::Artist {
            id: id.into(),
            name: name.into(),
            client: client(),
        }
    }

    /// Play history built as an `index.toml` would be loaded, which is
    /// the only way to get backdated timestamps: `record_play` stamps
    /// `last_played_secs` off the wall clock.
    struct History(String);

    impl History {
        fn new() -> History {
            History(String::new())
        }

        /// `days_ago` is when it was last played; `days` distinct days
        /// ending there, one per day.
        fn played(mut self, id: &str, plays: u32, days: u32, days_ago: u64) -> History {
            let last = NOW - days_ago * DAY;
            let labels: Vec<String> = (0..days)
                .rev()
                .map(|back| format!("\"{}\"", day_of(last - back as u64 * DAY)))
                .collect();
            self.0.push_str(&format!(
                "[entries.{id}]\nplay_count = {plays}\nplay_days = [{}]\nlast_played_secs = {last}\n",
                labels.join(", ")
            ));
            self
        }

        fn build(self) -> CacheIndex {
            toml::from_str(&self.0).expect("fixture index must parse")
        }
    }

    fn starred(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|i| i.to_string()).collect()
    }

    fn run(catalog: &Catalog, cache: &CacheIndex, starred: &HashSet<String>) -> Suggestions {
        suggest(&Request::new(catalog, cache, starred, NOW))
    }

    fn run_with(
        catalog: &Catalog,
        cache: &CacheIndex,
        starred: &HashSet<String>,
        enrichment: &dyn Enrichment,
    ) -> Suggestions {
        let tuning = Tuning::default();
        suggest(&Request {
            catalog,
            cache,
            starred,
            now_secs: NOW,
            seed: 7,
            enrichment,
            tuning: &tuning,
        })
    }

    fn section(s: &Suggestions, reason: Reason) -> Option<&Section> {
        s.sections.iter().find(|sec| sec.reason == reason)
    }

    fn titles(section: &Section) -> Vec<String> {
        section
            .items
            .iter()
            .map(|i| match &i.item {
                SearchItem::Song { title, .. } => title.clone(),
                SearchItem::Album { name, .. } => name.clone(),
                SearchItem::Artist { name, .. } => name.clone(),
                other => format!("{other:?}"),
            })
            .collect()
    }

    /// Two albums by one artist: one played to death, one never
    /// touched. Plus a second artist so diversification has something
    /// to work with.
    fn two_album_catalog() -> Catalog {
        let client = client();
        let mut catalog = Catalog::new();
        catalog.ingest_item(&artist_item("ar-1", "Bladee"));
        catalog.ingest_item(&album_item("al-1", "Eversince", "Bladee"));
        catalog.ingest_item(&album_item("al-2", "Crest", "Bladee"));
        catalog.ingest_album(
            "Eversince",
            &[
                song("s1", "Be Nice 2 Me", "Bladee", "Eversince", &["Cloud Rap"]),
                song("s2", "Obedient", "Bladee", "Eversince", &["Cloud Rap"]),
                song("s3", "Wondering", "Bladee", "Eversince", &["Cloud Rap"]),
                song("s4", "Backseat", "Bladee", "Eversince", &["Cloud Rap"]),
                song("s5", "Decay", "Bladee", "Eversince", &["Cloud Rap"]),
            ],
            &client,
        );
        catalog
    }

    // ---------------- affinity model ----------------

    #[test]
    fn day_number_is_the_exact_inverse_of_day_of() {
        for secs in [0u64, 86_399, 86_400, 1_709_164_800, NOW, NOW + 77 * DAY] {
            assert_eq!(
                day_number(&day_of(secs)),
                Some((secs / DAY) as i64),
                "round trip failed for {secs}"
            );
        }
        assert_eq!(day_number("garbage"), None);
        assert_eq!(day_number("2026-13-01"), None);
    }

    #[test]
    fn decay_puts_a_recent_play_above_an_identical_old_one() {
        let tuning = Tuning::default();
        let recent = decay(Some(NOW - 2 * DAY), NOW, tuning.half_life_days);
        let month_old = decay(Some(NOW - 30 * DAY), NOW, tuning.half_life_days);
        let year_old = decay(Some(NOW - 365 * DAY), NOW, tuning.half_life_days);
        assert!(recent > month_old && month_old > year_old);
        // the documented half-life: 30 days is exactly half.
        assert!((month_old - 0.5).abs() < 0.001, "half-life is 30 days");
        assert!(year_old < 0.001, "last year ranks far below this month");
    }

    /// The reason the engine counts distinct days, not plays: ten plays
    /// in one evening is one day of interest.
    #[test]
    fn distinct_days_beat_raw_play_count() {
        let tuning = Tuning::default();
        let binge = frequency(1, 10, false, &tuning);
        let habit = frequency(3, 3, false, &tuning);
        assert!(
            habit > binge,
            "3 days/3 plays ({habit}) must beat 1 day/10 plays ({binge})"
        );
    }

    #[test]
    fn affinity_rolls_up_from_tracks_to_album_artist_and_genre() {
        let catalog = two_album_catalog();
        let cache = History::new()
            .played("s1", 9, 6, 1)
            .played("s2", 4, 3, 2)
            .build();
        let starred = HashSet::new();
        let tuning = Tuning::default();
        let request = Request {
            catalog: &catalog,
            cache: &cache,
            starred: &starred,
            now_secs: NOW,
            seed: 0,
            enrichment: &NoEnrichment,
            tuning: &tuning,
        };
        let model = Model::build(&request);
        let artist = model.catalog.by_artist["bladee"];
        assert!(model.artists[artist].score > 0.0);
        assert_eq!(model.artists[artist].plays, 13);
        assert_eq!(model.artists[artist].played_tracks, 2);
        let eversince = model.catalog.by_album[&("bladee".into(), "eversince".into())];
        assert_eq!(model.albums[eversince].played_tracks, 2);
        assert_eq!(model.albums[eversince].known_tracks, 5);
        assert!((model.albums[eversince].coverage(5) - 0.4).abs() < 0.001);
        assert_eq!(model.genres["cloud rap"].played_tracks, 2);
    }

    // ---------------- generators fire ----------------

    #[test]
    fn album_gap_fires_for_an_unopened_album_by_a_loved_artist() {
        let catalog = two_album_catalog();
        let cache = History::new()
            .played("s1", 20, 9, 1)
            .played("s2", 8, 5, 2)
            .build();
        let out = run(&catalog, &cache, &HashSet::new());
        let gap = section(&out, Reason::AlbumGap).expect("album gap should fire");
        assert_eq!(titles(gap), vec!["Crest"]);
        assert!(
            gap.items[0].reason_text.contains("28"),
            "reason must carry the real play count: {}",
            gap.items[0].reason_text
        );
    }

    #[test]
    fn album_gap_does_not_fire_without_artist_affinity() {
        let catalog = two_album_catalog();
        // no history at all for this artist, so there is no affinity to
        // weight a gap by; whatever shows, it is not an album gap.
        let out = run(&catalog, &CacheIndex::default(), &HashSet::new());
        assert!(section(&out, Reason::AlbumGap).is_none());
    }

    #[test]
    fn track_gap_names_the_one_track_you_never_played() {
        let catalog = two_album_catalog();
        let cache = History::new()
            .played("s1", 6, 4, 1)
            .played("s2", 5, 4, 2)
            .played("s3", 4, 3, 3)
            .played("s4", 3, 3, 4)
            .build();
        let out = run(&catalog, &cache, &HashSet::new());
        let gap = section(&out, Reason::TrackGap).expect("track gap should fire");
        assert_eq!(titles(gap), vec!["Decay"]);
        assert!(
            gap.items[0].reason_text.contains("4 of 5 tracks played"),
            "explanation must be built from the real counts: {}",
            gap.items[0].reason_text
        );
    }

    #[test]
    fn track_gap_does_not_fire_on_a_fully_played_album() {
        let catalog = two_album_catalog();
        let cache = History::new()
            .played("s1", 3, 2, 1)
            .played("s2", 3, 2, 1)
            .played("s3", 3, 2, 1)
            .played("s4", 3, 2, 1)
            .played("s5", 3, 2, 1)
            .build();
        let out = run(&catalog, &cache, &HashSet::new());
        assert!(section(&out, Reason::TrackGap).is_none());
    }

    #[test]
    fn album_neighbour_fires_next_to_a_heavily_played_track() {
        let client = client();
        let mut catalog = Catalog::new();
        catalog.ingest_item(&artist_item("ar-1", "Burial"));
        catalog.ingest_album(
            "Untrue",
            &[
                song("b1", "Archangel", "Burial", "Untrue", &["Dubstep"]),
                song("b2", "Near Dark", "Burial", "Untrue", &["Dubstep"]),
                song("b3", "Ghost Hardware", "Burial", "Untrue", &["Dubstep"]),
            ],
            &client,
        );
        // only the middle track is played, so both neighbours qualify.
        let cache = History::new().played("b2", 12, 7, 1).build();
        let out = run(&catalog, &cache, &HashSet::new());
        let neighbours =
            section(&out, Reason::AlbumNeighbour).expect("album neighbours should fire");
        let found = titles(neighbours);
        assert!(
            found.contains(&"Archangel".to_string())
                || found.contains(&"Ghost Hardware".to_string())
        );
        assert!(
            neighbours.items[0].reason_text.contains("Near Dark")
                && neighbours.items[0].reason_text.contains("7 days"),
            "reason names the neighbour and its real day count: {}",
            neighbours.items[0].reason_text
        );
    }

    #[test]
    fn album_neighbour_does_not_fire_without_track_order() {
        // corpus-only ingest carries no track positions, so adjacency
        // is simply unknown and the generator must stay silent.
        let client = client();
        let mut catalog = Catalog::new();
        catalog.ingest_songs(
            &[
                song("b1", "Archangel", "Burial", "Untrue", &["Dubstep"]),
                song("b2", "Near Dark", "Burial", "Untrue", &["Dubstep"]),
            ],
            &client,
        );
        let cache = History::new().played("b2", 12, 7, 1).build();
        let out = run(&catalog, &cache, &HashSet::new());
        assert!(section(&out, Reason::AlbumNeighbour).is_none());
    }

    #[test]
    fn genre_under_exploration_surfaces_the_least_played_artist_in_a_loved_genre() {
        let client = client();
        let mut catalog = Catalog::new();
        catalog.ingest_item(&artist_item("ar-1", "Metallica"));
        catalog.ingest_item(&artist_item("ar-2", "Mastodon"));
        catalog.ingest_songs(
            &[
                song("m1", "One", "Metallica", "And Justice", &["Metal"]),
                song("m2", "Blackened", "Metallica", "And Justice", &["Metal"]),
                song(
                    "x1",
                    "Blood and Thunder",
                    "Mastodon",
                    "Leviathan",
                    &["Metal"],
                ),
            ],
            &client,
        );
        let cache = History::new()
            .played("m1", 30, 12, 1)
            .played("m2", 20, 9, 2)
            .played("x1", 1, 1, 40)
            .build();
        let out = run(&catalog, &cache, &HashSet::new());
        let genre =
            section(&out, Reason::GenreUnderExplored).expect("genre under-exploration should fire");
        assert_eq!(titles(genre), vec!["Mastodon"]);
        let text = &genre.items[0].reason_text;
        assert!(
            text.contains("Metal") && text.contains("51 plays") && text.contains("1 plays"),
            "reason must carry the real genre and artist counts: {text}"
        );
    }

    #[test]
    fn genre_under_exploration_does_not_fire_with_only_one_artist_in_the_genre() {
        let client = client();
        let mut catalog = Catalog::new();
        catalog.ingest_item(&artist_item("ar-1", "Metallica"));
        catalog.ingest_songs(
            &[
                song("m1", "One", "Metallica", "And Justice", &["Metal"]),
                song("m2", "Blackened", "Metallica", "And Justice", &["Metal"]),
            ],
            &client,
        );
        let cache = History::new().played("m1", 30, 12, 1).build();
        let out = run(&catalog, &cache, &HashSet::new());
        assert!(section(&out, Reason::GenreUnderExplored).is_none());
    }

    #[test]
    fn daily_rotation_fires_for_a_track_played_nearly_every_day() {
        let client = client();
        let mut catalog = Catalog::new();
        catalog.ingest_songs(
            &[
                song("d1", "Nightcall", "Kavinsky", "OutRun", &["Synthwave"]),
                song("d2", "Odd Look", "Kavinsky", "OutRun", &["Synthwave"]),
            ],
            &client,
        );
        let cache = History::new()
            .played("d1", 14, 10, 0)
            .played("d2", 2, 1, 3)
            .build();
        let out = run(&catalog, &cache, &HashSet::new());
        let daily = section(&out, Reason::DailyRotation).expect("daily rotation should fire");
        assert_eq!(titles(daily), vec!["Nightcall"]);
        assert!(
            daily.items[0]
                .reason_text
                .contains("10 of the last 10 days"),
            "reason must carry the real day counts: {}",
            daily.items[0].reason_text
        );
    }

    #[test]
    fn daily_rotation_does_not_fire_for_an_occasional_track() {
        let client = client();
        let mut catalog = Catalog::new();
        catalog.ingest_songs(
            &[song(
                "d1",
                "Nightcall",
                "Kavinsky",
                "OutRun",
                &["Synthwave"],
            )],
            &client,
        );
        // 5 distinct days spread over a year is not a daily habit.
        let raw = format!(
            "[entries.d1]\nplay_count = 5\nplay_days = [\"{}\", \"{}\", \"{}\", \"{}\", \"{}\"]\nlast_played_secs = {}\n",
            day_of(NOW - 300 * DAY),
            day_of(NOW - 200 * DAY),
            day_of(NOW - 100 * DAY),
            day_of(NOW - 50 * DAY),
            day_of(NOW - DAY),
            NOW - DAY
        );
        let cache: CacheIndex = toml::from_str(&raw).unwrap();
        let out = run(&catalog, &cache, &HashSet::new());
        assert!(section(&out, Reason::DailyRotation).is_none());
    }

    #[test]
    fn rediscovery_fires_for_a_former_obsession_and_not_for_a_current_one() {
        let client = client();
        let mut catalog = Catalog::new();
        catalog.ingest_songs(
            &[
                song(
                    "r1",
                    "Teardrop",
                    "Massive Attack",
                    "Mezzanine",
                    &["Trip-Hop"],
                ),
                song("r2", "Angel", "Massive Attack", "Mezzanine", &["Trip-Hop"]),
            ],
            &client,
        );
        let cache = History::new()
            .played("r1", 40, 20, 400) // loved it, a year ago
            .played("r2", 40, 20, 1) // loved it, still playing it
            .build();
        let out = run(&catalog, &cache, &HashSet::new());
        let rediscovery = section(&out, Reason::Rediscovery).expect("rediscovery should fire");
        assert_eq!(titles(rediscovery), vec!["Teardrop"]);
        assert!(
            rediscovery.items[0].reason_text.contains("20 days")
                && rediscovery.items[0].reason_text.contains("13 months"),
            "reason must carry the real day count and age: {}",
            rediscovery.items[0].reason_text
        );
    }

    #[test]
    fn rediscovery_does_not_fire_for_something_barely_played() {
        let client = client();
        let mut catalog = Catalog::new();
        catalog.ingest_songs(
            &[song(
                "r1",
                "Teardrop",
                "Massive Attack",
                "Mezzanine",
                &["Trip-Hop"],
            )],
            &client,
        );
        // one play, long ago: never an obsession, so nothing to rediscover.
        let cache = History::new().played("r1", 1, 1, 400).build();
        let out = run(&catalog, &cache, &HashSet::new());
        assert!(section(&out, Reason::Rediscovery).is_none());
    }

    #[test]
    fn starred_but_unplayed_fires_and_stops_once_it_is_played() {
        let client = client();
        let mut catalog = Catalog::new();
        catalog.ingest_songs(
            &[
                song("s1", "Flatline", "Bladee", "Icedancer", &["Cloud Rap"]),
                song("s2", "Rainbow", "Bladee", "Icedancer", &["Cloud Rap"]),
                song("s3", "Hennessy", "Bladee", "Icedancer", &["Cloud Rap"]),
            ],
            &client,
        );
        let cache = History::new().played("s2", 12, 6, 1).build();
        let out = run(&catalog, &cache, &starred(&["s1"]));
        let unplayed =
            section(&out, Reason::StarredUnplayed).expect("starred-unplayed should fire");
        assert_eq!(titles(unplayed), vec!["Flatline"]);
        assert!(
            unplayed.items[0].reason_text.contains("12 other Bladee"),
            "reason must carry the real sibling play count: {}",
            unplayed.items[0].reason_text
        );

        // starred *and* played is not a gap.
        let played = History::new()
            .played("s1", 3, 2, 1)
            .played("s2", 12, 6, 1)
            .build();
        let out = run(&catalog, &played, &starred(&["s1"]));
        assert!(section(&out, Reason::StarredUnplayed).is_none());
    }

    #[test]
    fn similar_artists_fires_only_for_artists_actually_in_the_corpus() {
        let client = client();
        let mut catalog = Catalog::new();
        catalog.ingest_item(&artist_item("ar-1", "Bladee"));
        catalog.ingest_item(&artist_item("ar-7", "Ecco2k"));
        catalog.ingest_songs(
            &[song(
                "s1",
                "Flatline",
                "Bladee",
                "Icedancer",
                &["Cloud Rap"],
            )],
            &client,
        );
        let cache = History::new().played("s1", 20, 10, 1).build();

        let mut enrichment = EnrichmentCache::new();
        enrichment.add_artist_info(
            "ar-1",
            &ArtistInfo {
                biography: None,
                similar_artists: vec![
                    crate::subsonic::Artist {
                        id: String::new(),
                        name: "Thaiboy Digital".into(), // not in this library
                    },
                    crate::subsonic::Artist {
                        id: "ar-7".into(),
                        name: "ecco2k".into(), // different casing on purpose
                    },
                ],
            },
        );
        let out = run_with(&catalog, &cache, &HashSet::new(), &enrichment);
        let similar = section(&out, Reason::SimilarArtist).expect("similar artists should fire");
        assert_eq!(titles(similar), vec!["Ecco2k"]);
        assert!(
            similar.items[0].reason_text.contains("Bladee")
                && similar.items[0].reason_text.contains("10 days"),
            "reason must name the seed and its real day count: {}",
            similar.items[0].reason_text
        );

        // with nothing fetched, the generator simply does not fire.
        let out = run(&catalog, &cache, &HashSet::new());
        assert!(section(&out, Reason::SimilarArtist).is_none());
    }

    #[test]
    fn popular_unplayed_fires_from_top_songs() {
        let client = client();
        let mut catalog = Catalog::new();
        catalog.ingest_item(&artist_item("ar-1", "Bladee"));
        catalog.ingest_songs(
            &[
                song("s1", "Flatline", "Bladee", "Icedancer", &["Cloud Rap"]),
                song("s2", "Be Nice 2 Me", "Bladee", "Eversince", &["Cloud Rap"]),
            ],
            &client,
        );
        let cache = History::new().played("s1", 20, 10, 1).build();
        let mut enrichment = EnrichmentCache::new();
        enrichment.add_top_songs(
            "Bladee",
            &[
                song("s2", "Be Nice 2 Me", "Bladee", "Eversince", &["Cloud Rap"]),
                song("s1", "Flatline", "Bladee", "Icedancer", &["Cloud Rap"]),
            ],
        );
        let out = run_with(&catalog, &cache, &HashSet::new(), &enrichment);
        let popular = section(&out, Reason::PopularUnplayed).expect("popular-unplayed should fire");
        assert_eq!(titles(popular), vec!["Be Nice 2 Me"]);
        assert!(popular.items[0].reason_text.contains("20"));
    }

    // ---------------- scoring, diversification, determinism ----------

    #[test]
    fn one_artist_cannot_fill_the_menu() {
        // 30 unplayed tracks by one artist across six albums, every one
        // of them a legitimate track gap, plus one played track per
        // album to make the gaps fire at all.
        let client = client();
        let mut catalog = Catalog::new();
        catalog.ingest_item(&artist_item("ar-1", "Prolific"));
        let mut cache = History::new();
        for album in 0..6 {
            let mut songs = Vec::new();
            for track in 0..5 {
                songs.push(song(
                    &format!("p-{album}-{track}"),
                    &format!("track {album}-{track}"),
                    "Prolific",
                    &format!("album {album}"),
                    &["Shoegaze"],
                ));
            }
            catalog.ingest_item(&album_item(
                &format!("al-{album}"),
                &format!("album {album}"),
                "Prolific",
            ));
            catalog.ingest_album(&format!("album {album}"), &songs, &client);
            cache = cache.played(&format!("p-{album}-0"), 20, 10, 1);
        }
        let out = run(&catalog, &cache.build(), &HashSet::new());
        assert!(!out.is_empty());
        let tuning = Tuning::default();
        assert!(
            out.len() <= tuning.max_per_artist,
            "one artist must not fill the menu: got {} rows",
            out.len()
        );
    }

    #[test]
    fn diversification_caps_artists_and_albums_across_the_whole_menu() {
        let tuning = Tuning::default();
        let item = |id: &str| SearchItem::Album {
            id: id.into(),
            name: id.into(),
            artist: "One Artist".into(),
            client: client(),
        };
        let pool: Vec<Candidate> = (0..20)
            .map(|i| {
                let id = format!("al-{i}");
                Candidate::new(Reason::AlbumGap, &item(&id), 1.0, String::new()).attributes(
                    "one artist".into(),
                    Some(("one artist".into(), id)),
                    Some("metal".into()),
                )
            })
            .map(|mut c| {
                c.score = 1.0;
                c
            })
            .collect();
        let picked = diversify(pool, &tuning);
        assert_eq!(
            picked.len(),
            tuning.max_per_artist,
            "the per-artist cap is what stops a one-artist menu"
        );

        // distinct artists are not capped the same way.
        let pool: Vec<Candidate> = (0..20)
            .map(|i| {
                let id = format!("al-{i}");
                let mut c = Candidate::new(Reason::AlbumGap, &item(&id), 1.0, String::new())
                    .attributes(
                        format!("artist {i}"),
                        Some((format!("artist {i}"), id)),
                        None,
                    );
                c.score = 1.0;
                c
            })
            .collect();
        let picked = diversify(pool, &tuning);
        assert_eq!(picked.len(), tuning.section_max_items);
    }

    #[test]
    fn per_generator_normalisation_keeps_a_big_number_generator_from_dominating() {
        let tuning = Tuning::default();
        let item = artist_item("ar-1", "Someone");
        let mut big = vec![
            Candidate::new(Reason::DailyRotation, &item, 10_000.0, String::new()),
            Candidate::new(Reason::DailyRotation, &item, 5_000.0, String::new()),
        ];
        let mut small = vec![Candidate::new(Reason::TrackGap, &item, 0.02, String::new())];
        normalise(&mut big, &tuning);
        normalise(&mut small, &tuning);
        assert!((big[0].score - tuning.weights.daily_rotation).abs() < 0.001);
        assert!((small[0].score - tuning.weights.track_gap).abs() < 0.001);
        assert!(
            small[0].score > big[1].score,
            "a generator's raw magnitude must not decide the merge"
        );
    }

    #[test]
    fn the_same_result_comes_back_for_the_same_inputs() {
        let catalog = two_album_catalog();
        let cache = History::new()
            .played("s1", 20, 9, 1)
            .played("s2", 8, 5, 2)
            .played("s3", 2, 2, 90)
            .build();
        let starred = starred(&["s5"]);
        let first = run(&catalog, &cache, &starred);
        let second = run(&catalog, &cache, &starred);
        let flatten = |s: &Suggestions| {
            s.flat()
                .map(|(r, i)| (r, i.reason_text.clone(), format!("{:.6}", i.score)))
                .collect::<Vec<_>>()
        };
        assert_eq!(flatten(&first), flatten(&second));
    }

    #[test]
    fn nothing_is_suggested_twice() {
        let catalog = two_album_catalog();
        let cache = History::new()
            .played("s1", 20, 9, 1)
            .played("s2", 8, 5, 2)
            .played("s3", 8, 5, 2)
            .played("s4", 8, 5, 2)
            .build();
        let out = run(&catalog, &cache, &starred(&["s5"]));
        let mut ids: Vec<(u8, String)> = out
            .flat()
            .map(|(_, s)| {
                let (kind, id) = s.item.identity();
                (kind, id.into_owned())
            })
            .collect();
        let before = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), before, "a row appeared in two sections");
    }

    // ---------------- cold start ----------------

    #[test]
    fn cold_start_is_never_empty_and_spreads_across_artists() {
        let mut catalog = Catalog::new();
        for i in 0..8 {
            let artist = format!("artist {i}");
            catalog.ingest_item(&artist_item(&format!("ar-{i}"), &artist));
            for album in 0..3 {
                catalog.ingest_item(&album_item(
                    &format!("al-{i}-{album}"),
                    &format!("album {i}-{album}"),
                    &artist,
                ));
            }
        }
        let out = run(&catalog, &CacheIndex::default(), &HashSet::new());
        let sample =
            section(&out, Reason::ColdStartSample).expect("cold start must show something");
        assert!(!sample.items.is_empty());
        let artists: HashSet<String> = sample
            .items
            .iter()
            .map(|i| match &i.item {
                SearchItem::Album { artist, .. } => artist.clone(),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(
            artists.len(),
            sample.items.len(),
            "a stratified sample shows one row per artist, not three by one"
        );
        assert!(
            sample.items[0].reason_text.contains("1 of 8 artists"),
            "even cold start explains itself with a real number: {}",
            sample.items[0].reason_text
        );
    }

    #[test]
    fn cold_start_leads_with_starred_items() {
        let client = client();
        let mut catalog = Catalog::new();
        catalog.ingest_songs(
            &[
                song("c1", "Starred Track", "Someone", "Some Album", &[]),
                song("c2", "Other Track", "Someone Else", "Other Album", &[]),
            ],
            &client,
        );
        let out = run(&catalog, &CacheIndex::default(), &starred(&["c1"]));
        assert_eq!(out.sections[0].reason, Reason::ColdStartStarred);
        assert_eq!(titles(&out.sections[0]), vec!["Starred Track"]);
    }

    #[test]
    fn cold_start_does_not_show_the_same_rows_for_every_seed() {
        let mut catalog = Catalog::new();
        for i in 0..40 {
            let artist = format!("artist {i:02}");
            catalog.ingest_item(&artist_item(&format!("ar-{i}"), &artist));
            catalog.ingest_item(&album_item(
                &format!("al-{i}"),
                &format!("album {i:02}"),
                &artist,
            ));
        }
        let cache = CacheIndex::default();
        let starred = HashSet::new();
        let tuning = Tuning::default();
        let rows = |seed: u64| {
            suggest(&Request {
                catalog: &catalog,
                cache: &cache,
                starred: &starred,
                now_secs: NOW,
                seed,
                enrichment: &NoEnrichment,
                tuning: &tuning,
            })
            .flat()
            .map(|(_, s)| s.item.identity().1.into_owned())
            .collect::<Vec<_>>()
        };
        assert_ne!(rows(1), rows(2), "the seed has to actually move the sample");
        assert_eq!(rows(1), rows(1), "but one seed is still deterministic");
    }

    #[test]
    fn an_empty_catalog_is_empty_not_a_panic() {
        let out = run(&Catalog::new(), &CacheIndex::default(), &HashSet::new());
        assert!(out.is_empty());
        assert_eq!(out.len(), 0);
    }

    // ---------------- host-facing api ----------------

    #[test]
    fn enrichment_targets_are_the_top_artists_with_a_server_id() {
        let client = client();
        let mut catalog = Catalog::new();
        catalog.ingest_item(&artist_item("ar-1", "Bladee"));
        catalog.ingest_item(&artist_item("ar-2", "Burial"));
        catalog.ingest_songs(
            &[
                song("s1", "Flatline", "Bladee", "Icedancer", &[]),
                song("b1", "Archangel", "Burial", "Untrue", &[]),
                // no SearchItem::Artist for this one, so no id to fetch
                song("z1", "Unknown", "Nobody", "Nowhere", &[]),
            ],
            &client,
        );
        let cache = History::new()
            .played("s1", 20, 10, 1)
            .played("b1", 5, 3, 2)
            .played("z1", 50, 30, 1)
            .build();
        let starred = HashSet::new();
        let targets = enrichment_targets(&Request::new(&catalog, &cache, &starred, NOW), 5);
        assert_eq!(
            targets,
            vec![
                ArtistTarget {
                    id: "ar-1".into(),
                    name: "Bladee".into()
                },
                ArtistTarget {
                    id: "ar-2".into(),
                    name: "Burial".into()
                },
            ]
        );
    }

    #[test]
    fn an_empty_enrichment_answer_still_counts_as_answered() {
        // a server with no last.fm metadata behind it answers with
        // nothing; asking it again on every menu open would be an
        // unbounded cost for a permanently empty result.
        let mut enrichment = EnrichmentCache::new();
        assert!(enrichment.needs_similar("ar-1"));
        assert!(enrichment.needs_top_songs("Bladee"));
        enrichment.add_artist_info("ar-1", &ArtistInfo::default());
        enrichment.add_top_songs("Bladee", &[]);
        assert!(!enrichment.needs_similar("ar-1"));
        assert!(!enrichment.needs_top_songs("Bladee"));
        // and top songs are keyed by normalised name, so a differently
        // cased spelling of the same artist is not a second question.
        assert!(!enrichment.needs_top_songs("  bladee "));
    }

    #[test]
    fn the_enrichment_cache_round_trips_through_toml() {
        let mut enrichment = EnrichmentCache::new();
        enrichment.add_artist_info(
            "ar-1",
            &ArtistInfo {
                biography: Some("not persisted; the bio pane fetches its own".into()),
                similar_artists: vec![
                    crate::subsonic::Artist {
                        id: "ar-2".into(),
                        name: "Ecco2k".into(),
                    },
                    crate::subsonic::Artist {
                        id: "-1".into(),
                        name: "Thaiboy Digital".into(),
                    },
                ],
            },
        );
        enrichment.add_artist_info("ar-9", &ArtistInfo::default());
        enrichment.add_top_songs(
            "Bladee",
            &[
                song("s1", "Be Nice 2 Me", "Bladee", "Eversince", &[]),
                song("s2", "Rosier", "Bladee", "Eversince", &[]),
            ],
        );

        let raw = toml::to_string_pretty(&enrichment).expect("serialisable");
        let back: EnrichmentCache = toml::from_str(&raw).expect("deserialisable");
        let similar: Vec<&str> = back
            .similar_artists("ar-1")
            .iter()
            .map(String::as_str)
            .collect();
        assert_eq!(
            similar,
            ["Ecco2k", "Thaiboy Digital"],
            "rank order is the signal"
        );
        let top: Vec<&str> = back
            .top_songs("Bladee")
            .iter()
            .map(String::as_str)
            .collect();
        assert_eq!(top, ["s1", "s2"]);
        assert!(
            !back.needs_similar("ar-9"),
            "the negative answer has to survive a restart too"
        );
    }

    #[test]
    fn album_targets_are_the_played_albums_with_no_tracklist_yet() {
        let client = client();
        let mut catalog = Catalog::new();
        catalog.ingest_item(&album_item("al-1", "Icedancer", "Bladee"));
        catalog.ingest_item(&album_item("al-2", "Eversince", "Bladee"));
        catalog.ingest_item(&album_item("al-3", "Untrue", "Burial"));
        catalog.ingest_item(&album_item("al-4", "Crest", "Bladee"));
        catalog.ingest_songs(
            &[
                song("i1", "Decay", "Bladee", "Icedancer", &[]),
                song("e1", "Rosier", "Bladee", "Eversince", &[]),
                song("u1", "Archangel", "Burial", "Untrue", &[]),
                song("c1", "Hahah", "Bladee", "Crest", &[]),
            ],
            &client,
        );
        // Eversince has already been fetched whole, so there is
        // nothing left to learn about it.
        catalog.ingest_album(
            "Eversince",
            &[song("e1", "Rosier", "Bladee", "Eversince", &[])],
            &client,
        );
        let cache = History::new()
            // Crest is deliberately absent: an album with no plays is
            // AlbumGap's job and needs no round trip.
            .played("i1", 20, 10, 1)
            .played("u1", 3, 2, 1)
            .played("e1", 40, 20, 1)
            .build();
        let starred = HashSet::new();
        let request = Request::new(&catalog, &cache, &starred, NOW);

        let targets = album_targets(&request, 10);
        let names: Vec<&str> = targets.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(
            names,
            ["Icedancer", "Untrue"],
            "highest album affinity first, and only albums we have played from"
        );
        assert_eq!(targets[0].id, "al-1", "the id getAlbum needs comes along");

        assert_eq!(
            album_targets(&request, 1)
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>(),
            ["Icedancer"],
            "the cap is what bounds a warm-up on a large library"
        );
    }

    #[test]
    fn an_album_with_no_row_of_its_own_is_never_a_fetch_target() {
        // an album known only as a track's album name has no id to
        // call getAlbum with.
        let client = client();
        let mut catalog = Catalog::new();
        catalog.ingest_songs(
            &[song("s1", "Obedient", "Bladee", "Working on Dying", &[])],
            &client,
        );
        let cache = History::new().played("s1", 9, 4, 1).build();
        let starred = HashSet::new();
        let targets = album_targets(&Request::new(&catalog, &cache, &starred, NOW), 10);
        assert!(targets.is_empty());
    }

    #[test]
    fn artist_client_resolves_the_server_an_artist_came_from() {
        let mut catalog = Catalog::new();
        catalog.ingest_item(&artist_item("ar-1", "Bladee"));
        assert!(catalog.artist_client("bladee").is_some(), "joined on name");
        assert!(
            catalog.artist_client("Nobody").is_none(),
            "an artist with no indexed row has no server to ask"
        );
    }

    #[test]
    fn the_corpus_ingest_and_the_album_ingest_agree_on_one_track() {
        // a track indexed from search first and from a full album fetch
        // later must be one row with the richer facts, not two.
        let client = client();
        let mut catalog = Catalog::new();
        catalog.ingest_item(&SearchItem::Song {
            id: "s1".into(),
            title: "Be Nice 2 Me".into(),
            artist: "Bladee".into(),
            album: "Eversince".into(),
            client: client.clone(),
        });
        assert_eq!(catalog.track_count(), 1);
        assert!(catalog.tracks[0].genres.is_empty());
        catalog.ingest_album(
            "Eversince",
            &[song(
                "s1",
                "Be Nice 2 Me",
                "Bladee",
                "Eversince",
                &["Cloud Rap"],
            )],
            &client,
        );
        assert_eq!(catalog.track_count(), 1, "the same id is one track");
        assert_eq!(catalog.tracks[0].genres, vec!["Cloud Rap"]);
        assert_eq!(catalog.tracks[0].position, Some(0));
    }
}
