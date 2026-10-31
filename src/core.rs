//! The player core: everything a shellsong `Op` can touch that is not
//! a pane. `wim` and `shellswim` both drive this; what they do not
//! share goes through `Surface`, whose default impls decline. Read
//! docs/library-seam.md before moving anything across that line.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::app::{self, Action, LoopMode};
use crate::cache::{self, CacheIndex};
use crate::download::{self, DownloadEvent};
use crate::notify::{Notifications, Severity};
use crate::playqueue::PlayQueue;
use crate::search::SearchItem;
use crate::shellsong::{self, Console, GoTarget, Op, Program, Target, ViewTarget};
use crate::shellsong_host::{Ramp, SearchSettle};
use crate::ui::controlsbar::Field;
use crate::ui::results_list::label;
use crate::{
    browse, config, eq, favorites, library, listenbrainz, lyrics, mpv, namehint, opensubtitles,
    persistence, queues, search, shellsong_host, subsonic, ui, undo, Focus,
};

/// Local items resolve straight to a file path mpv can load. A remote
/// `Song` resolves straight to a stream url (mpv loads http urls the
/// same as local paths, nothing special needed). Remote `Album`/
/// `Playlist`/`Artist` do NOT resolve here: they represent many
/// tracks, not one, and get expanded into individual `Song` items by
/// `resolve_tracks`/`artist_tracks` before anything ever calls this
/// function on them.
pub fn playable_path(item: &SearchItem) -> Option<String> {
    match item {
        SearchItem::LocalMusic { path, .. } | SearchItem::LocalVideo { path, .. } => {
            Some(path.to_string_lossy().into_owned())
        }
        SearchItem::Song { id, client, .. } => Some(client.stream_url(id)),
        SearchItem::Silence { secs } => Some(silence_url(*secs)),
        SearchItem::Artist { .. } | SearchItem::Album { .. } | SearchItem::Playlist { .. } => None,
    }
}

/// Play/pause, with the one case where it does not mean that.
///
/// During a between-track gap mpv is paused at the end of a track that
/// has already finished, so unpausing resumes nothing: mpv would sit
/// at the same end and immediately report it again. Pressing play
/// there plainly means "get on with it", so the gap is dropped and the
/// next track starts.
///
/// Shared so the keybind, the bar click and enter on the player cannot
/// disagree; `activate_transport` is the one place that calls it.
pub fn toggle_pause(player: &mpv::Mpv, between: &mut BetweenTracks) -> anyhow::Result<()> {
    if between.holding() {
        between.cancel();
        player.playlist_next()?;
        return player.set_pause(false);
    }
    player.toggle_pause()
}

/// What the host must do about the between-track gap this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GapAction {
    /// The gap starts now. mpv has already paused *itself* (that is
    /// what `keep-open` does), so there is nothing to send; this is
    /// only so the host can say so on screen.
    Begin,
    /// The gap is over: advance, and **unpause**, because mpv stays
    /// paused across `playlist-next` once it has held.
    Release,
}

/// The pause between tracks (`gap`), as a state machine over mpv's
/// `eof-reached`.
///
/// Deliberately not "insert a silence entry between every track",
/// which was considered and rejected: it doubles the queue's length,
/// makes every index the user sees wrong, and leaves orphaned gaps
/// behind every removal. This is a property of the session applied at
/// the join instead, so the queue is exactly what the user put in it.
///
/// Pure apart from the `Instant` it is handed, so the whole of it is
/// testable without mpv.
#[derive(Debug, Default)]
pub struct BetweenTracks {
    until: Option<Instant>,
    /// Whether an `eof` may still start a gap. Cleared when one
    /// starts and set again only once mpv reports it has left the end.
    ///
    /// This is not bookkeeping for its own sake: `Release` sends
    /// `playlist-next`, and `eof-reached` stays true locally until
    /// mpv's own property change arrives a frame or two later. Without
    /// the latch that stale `true` immediately starts a second gap,
    /// and every track gets two.
    rearmed: bool,
}

impl BetweenTracks {
    pub fn new() -> Self {
        BetweenTracks {
            until: None,
            rearmed: true,
        }
    }

    /// `eof` is mpv's `eof-reached`, `gap` the configured pause,
    /// `more_to_play` whether anything is queued after this.
    pub fn tick(
        &mut self,
        eof: bool,
        gap: Duration,
        more_to_play: bool,
        now: Instant,
    ) -> Option<GapAction> {
        if !eof {
            self.rearmed = true;
        }
        if let Some(until) = self.until {
            if now < until {
                return None;
            }
            self.until = None;
            return Some(GapAction::Release);
        }
        // `more_to_play` is the guard that keeps the end of the queue
        // the end of the queue: holding there would wait out a gap and
        // then skip into nothing.
        if eof && self.rearmed && more_to_play && !gap.is_zero() {
            self.until = Some(now + gap);
            self.rearmed = false;
            return Some(GapAction::Begin);
        }
        None
    }

    /// How much of the gap is left, for the countdown.
    pub fn remaining(&self, now: Instant) -> Option<Duration> {
        self.until.map(|until| until.saturating_duration_since(now))
    }

    /// Ends the gap immediately, without advancing. For "the user
    /// asked to get on with it": the caller advances itself, so this
    /// only drops the hold.
    pub fn cancel(&mut self) {
        self.until = None;
        self.rearmed = false;
    }

    pub fn holding(&self) -> bool {
        self.until.is_some()
    }
}

/// Most results `show_results` will print. Past this the list is the
/// flood it was meant to avoid, and a narrower query is the answer.
pub const MAX_ECHOED_RESULTS: usize = 30;

/// Search results as lines to print, for a front-end with no pane to
/// list them in.
///
/// Bare `primary_name`s, not the decorated `results_list::label`:
/// three columns of a 72-cell terminal leave ~22 cells each, which
/// truncates "title · artist (album)" to exactly the useless half, and
/// a bare name is also what you would type back into `play(...)`.
/// Deliberately unnumbered, since no `Target` is addressable by
/// result index from the language and a numbered list would advertise
/// a `play(2)` that does not exist.
///
/// **`shown` is what has already been printed for this query, and it
/// is why this takes a set rather than a slice offset.** A remote
/// server answering after the settle window enriches the corpus and
/// the results are then *re-ranked*, so the late arrivals are not a
/// tail that can be printed by index: a name can move anywhere in the
/// list. Diffing by name is correct whatever the reordering, and it
/// means the second batch prints only what is actually new instead of
/// repeating the first.
pub fn echo_lines(
    results: &[SearchItem],
    shown: &mut HashSet<String>,
    avail: usize,
) -> Vec<String> {
    let room = MAX_ECHOED_RESULTS.saturating_sub(shown.len());
    if room == 0 {
        return Vec::new();
    }
    let mut fresh: Vec<String> = Vec::new();
    let mut held = 0usize;
    for item in results {
        let name = item.primary_name();
        if shown.contains(name) || fresh.iter().any(|f| f == name) {
            continue;
        }
        if fresh.len() == room {
            held += 1;
            continue;
        }
        fresh.push(name.to_string());
    }
    shown.extend(fresh.iter().cloned());
    let mut lines = crate::ui::textfit::columns(&fresh, avail, 3);
    if held > 0 {
        lines.push(format!("+{held} more"));
    }
    lines
}

/// How long what is playing actually is: the entry's own answer where
/// it has one, mpv's reported `duration` otherwise.
///
/// mpv reports ~1s for a generated silence whatever length was asked
/// for (measured, see `silence_url`), so a 15m gap drew as a
/// one-second track with a scrubber that filled and sat full for the
/// rest of it. The entry is authoritative *only* where it genuinely
/// knows better; for a real file mpv is the one that has read the
/// container, so this must not become a general-purpose override.
///
/// Used for everything that means "the length of what is playing":
/// the readouts, the scrubber, a scrub's seek target, and the
/// crossfade ramp. The ramp is not cosmetic either: with a 1s
/// duration, `remaining` stayed negative for the whole silence, so it
/// wrote a volume to mpv on **every frame for fifteen minutes**
/// instead of over the last few seconds.
pub fn playing_duration(item: Option<&SearchItem>, reported: Option<f64>) -> Option<f64> {
    match item {
        Some(SearchItem::Silence { secs }) => Some(*secs as f64),
        _ => reported,
    }
}

/// The ffmpeg source mpv loads for a timed silence.
///
/// **Measured, because the obvious reading of this is wrong.** mpv
/// plays it for exactly `secs` (timed: a `d=3` source and a `d=6` one
/// differ by three seconds of wall clock), but it reports
/// `duration` as `00:00:01` whatever `d` says, at every spelling
/// tried (`d=`, `duration=`, with and without explicit rate and
/// channel layout). So playback and ordering are correct and
/// **anything drawing the length of a silence must take it from the
/// entry, not from mpv's `duration` property**, or a 15m gap draws as
/// a one-second track.
pub fn silence_url(secs: u64) -> String {
    format!("av://lavfi:anullsrc=d={secs}")
}
/// Same as `playable_path`, but checks the local cache (see `cache.rs`)
/// for a remote `Song` first: a valid cached file plays back instantly
/// with no network round-trip at all, which is the entire payoff of
/// caching in the first place. Falls through to `playable_path` (a
/// stream url) whenever nothing's cached, or for anything that was
/// never a candidate for caching to begin with.
pub fn resolve_playable(item: &SearchItem, cache: &Arc<Mutex<CacheIndex>>) -> Option<String> {
    if let SearchItem::Song { id, client, .. } = item {
        if let Some(path) = cache.lock().ok().and_then(|idx| idx.cached_path(id)) {
            return Some(path.to_string_lossy().into_owned());
        }
        return Some(client.stream_url(id));
    }
    playable_path(item)
}
/// Expands anything that represents more than one track into its
/// actual tracks, so playing/queuing an album, playlist, or artist
/// means playing/queuing every (actual, see `artist_tracks`) song,
/// not the entity itself. Local items and remote songs are already
/// exactly one track, so they pass through unchanged.
/// Where a resolved batch goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    /// Appended, which is what the queue key does.
    Back,
    /// Inserted directly behind what is playing
    /// (`add_to_queue(front, ...)`).
    Front,
    /// Cached to disk rather than queued (`KeyAction::DownloadSelected`,
    /// `Op::Download`). Not a queue destination, but it expands an
    /// album or artist through the very same `resolve_tracks`, so it
    /// shares the resolver rather than growing a fourth one.
    Download,
}

impl Intent {
    /// For "nothing to {verb} in {album}", so an empty album says
    /// which thing it failed to be.
    pub fn verb(self) -> &'static str {
        match self {
            Intent::Back | Intent::Front => "queue",
            Intent::Download => "download",
        }
    }

    /// Whether replacing the queue makes this batch meaningless.
    ///
    /// It does for the queue destinations, which is the whole point of
    /// `invalidate`. It does not for a download: the tracks go to disk,
    /// and clearing the queue says nothing about whether you still
    /// want them cached. Sharing one resolver across both would
    /// otherwise have made `Op::ClearQueue` quietly cancel a download
    /// someone asked for seconds earlier.
    pub fn queue_bound(self) -> bool {
        match self {
            Intent::Back | Intent::Front => true,
            Intent::Download => false,
        }
    }
}

/// What `TrackResolver::request` could answer with.
pub enum Resolution {
    /// No round trip was needed, so the caller acts now. A local file
    /// or a single song is already its own track list, and making the
    /// commonest action wait a frame for a thread would be a
    /// regression dressed as a fix.
    Ready(Vec<SearchItem>),
    /// A fetch is running; the label is what to tell the user it is
    /// waiting on. The batch arrives through `poll`.
    Fetching(String),
}

/// Resolves an album, artist or playlist into tracks off-thread, for
/// queueing.
///
/// `resolve_tracks` is a network call, and queueing used to make it on
/// the input path. For an album that is one `getAlbum`; **for an artist
/// it is a `getArtist` plus a `getAlbum` per album**, so pressing the
/// queue key on a prolific artist froze the whole ui for the length of
/// that fan-out.
///
/// Guarded like `browse::Loader` and `lyrics::Loader`, but **the guard
/// counts queue replacements, not requests**, and that difference is
/// the whole design. Those two loaders fill one pane, so a newer
/// request supersedes an older one and latest-wins is right. Queueing
/// *accumulates*: queueing two albums in a row has to queue both, so a
/// per-request counter dropped the first one silently (shipped that
/// way; see ROADMAP).
///
/// So only `invalidate` bumps. Every worker captures the epoch it was
/// spawned in and checks it before sending, and `poll` checks again, so
/// any number of requests can be in flight at once, and replacing the
/// queue disowns all of them. That last part matters more here than
/// elsewhere: a late arrival does not just draw something stale, it
/// *mutates the queue*.
pub struct TrackResolver {
    epoch: Arc<AtomicU64>,
    tx: mpsc::Sender<ResolvedBatch>,
    rx: mpsc::Receiver<ResolvedBatch>,
}

struct ResolvedBatch {
    epoch: u64,
    into: Intent,
    label: String,
    tracks: Result<Vec<SearchItem>, String>,
}

impl Default for TrackResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl TrackResolver {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        TrackResolver {
            epoch: Arc::new(AtomicU64::new(0)),
            tx,
            rx,
        }
    }

    /// Resolves `item` for `into`, instantly where no request is
    /// needed.
    pub fn request(&mut self, item: &SearchItem, into: Intent) -> Resolution {
        if matches!(
            item,
            SearchItem::LocalMusic { .. }
                | SearchItem::LocalVideo { .. }
                | SearchItem::Song { .. }
                | SearchItem::Silence { .. }
        ) {
            // `resolve_tracks` cannot fail for these, but it owns the
            // rule about what they resolve to, so it stays the one
            // place that says so.
            return Resolution::Ready(resolve_tracks(item).unwrap_or_else(|_| vec![item.clone()]));
        }
        let label = item.primary_name().to_string();
        // read, not bumped: a second request does not supersede the
        // first, it joins it.
        let epoch = self.epoch.load(Ordering::SeqCst);
        let (item, tx, shared) = (item.clone(), self.tx.clone(), Arc::clone(&self.epoch));
        let sent = label.clone();
        thread::spawn(move || {
            let tracks = resolve_tracks(&item);
            if into.queue_bound() && shared.load(Ordering::SeqCst) != epoch {
                return;
            }
            let _ = tx.send(ResolvedBatch {
                epoch,
                into,
                label: sent,
                tracks,
            });
        });
        Resolution::Fetching(label)
    }

    /// Disowns everything in flight, because the queue they were
    /// resolved against is gone. Called where the queue is replaced
    /// wholesale (`start_session`) or emptied (`Op::ClearQueue`), not
    /// per request.
    pub fn invalidate(&mut self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
    }

    /// The next batch still belonging to the current queue, if one
    /// landed.
    pub fn poll(&mut self) -> Option<(Intent, String, Result<Vec<SearchItem>, String>)> {
        let current = self.epoch.load(Ordering::SeqCst);
        loop {
            match self.rx.try_recv() {
                Ok(batch) if batch.into.queue_bound() && batch.epoch != current => continue,
                Ok(batch) => return Some((batch.into, batch.label, batch.tracks)),
                Err(_) => return None,
            }
        }
    }

    #[cfg(test)]
    fn deliver(&self, epoch: u64, into: Intent, tracks: Vec<SearchItem>) {
        let _ = self.tx.send(ResolvedBatch {
            epoch,
            into,
            label: "test".into(),
            tracks: Ok(tracks),
        });
    }

    #[cfg(test)]
    fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }
}

/// Appends a resolved batch and says how many. Shared by the queue
/// key's instant path and the drain that handles a fetched one, so the
/// two cannot report differently.
pub fn append_resolved(tracks: &[SearchItem], core: &mut CoreCtx) {
    for t in tracks {
        let s = enqueue_track(core.player, t, core.cache, core.queue, core.app.shuffle);
        if s.starts_with("error") {
            core.notifications.push(Severity::Error, s);
        }
    }
    let n = tracks.len();
    core.notifications.push(
        Severity::Info,
        if n == 1 {
            "queued 1 track".to_string()
        } else {
            format!("queued {n} tracks")
        },
    );
}

/// Inserts a resolved batch directly behind what is playing.
///
/// Reversed so the batch keeps its own order: each track goes in right
/// after the current one, so inserting them front-to-back would play
/// them backwards.
pub fn front_resolved(tracks: &[SearchItem], core: &mut CoreCtx) -> Result<usize, String> {
    for track in tracks.iter().rev() {
        enqueue_next(track, core)?;
    }
    Ok(tracks.len())
}

pub fn resolve_tracks(item: &SearchItem) -> Result<Vec<SearchItem>, String> {
    match item {
        SearchItem::LocalMusic { .. }
        | SearchItem::LocalVideo { .. }
        | SearchItem::Song { .. }
        | SearchItem::Silence { .. } => Ok(vec![item.clone()]),
        SearchItem::Album { id, client, .. } => client
            .get_album(id)
            .map(|details| browse::songs_to_items(details.songs, client))
            .map_err(|e| format!("couldn't load album tracks: {e}")),
        SearchItem::Playlist { id, client, .. } => client
            .get_playlist_songs(id)
            .map(|songs| browse::songs_to_items(songs, client))
            .map_err(|e| format!("couldn't load playlist tracks: {e}")),
        SearchItem::Artist { id, name, client } => {
            artist_tracks(client, id, name).map(|(tracks, _albums)| tracks)
        }
    }
}
/// Every song actually credited to this artist, across every album
/// including compilations. The Global/Various-Artists fix: `getAlbum`
/// returns each song with its own track-level `artist` field, distinct
/// from the album's credited artist (a compilation like navidrome's
/// "Global"/"Various Artists" convention would otherwise hide every
/// song on it from here); filtering by the song's own field instead of
/// the album's works for any compilation, not a hardcoded special case.
/// Every song actually credited to this artist (see the module doc
/// above for the Global/Various-Artists filtering), plus the
/// album list itself for the artist profile's own ALBUMS column.
pub fn artist_tracks(
    client: &Arc<subsonic::Client>,
    artist_id: &str,
    artist_name: &str,
) -> Result<(Vec<SearchItem>, Vec<SearchItem>), String> {
    let mut artist_albums = client
        .get_artist(artist_id)
        .map_err(|e| format!("couldn't load artist: {e}"))?;
    // chronological, not whatever order getArtist happened to return
    // (alphabetical); unknown-year albums sort last rather than first.
    artist_albums.sort_by_key(|a| a.year.unwrap_or(u32::MAX));

    // one getAlbum round-trip per album, done one at a time, was the
    // whole reason opening a prolific artist's profile felt so slow
    // (dozens of sequential network calls, each paying its own
    // latency); they're independent requests, so they go through
    // `album_details`'s bounded pool instead (a thread per album was
    // a socket per album against a real navidrome instance).
    let ids: Vec<&str> = artist_albums.iter().map(|a| a.id.as_str()).collect();
    let details = client.album_details(&ids);

    let mut tracks = Vec::new();
    let mut albums = Vec::new();
    for (artist_album, details) in artist_albums.into_iter().zip(details) {
        let Some(details) = details else { continue };
        // AlbumDetails itself carries no name (see subsonic.rs); each
        // of its own songs does, and they all share one.
        let album_name = details
            .songs
            .first()
            .map(|s| s.album.clone())
            .unwrap_or_default();
        albums.push(SearchItem::Album {
            id: artist_album.id,
            name: album_name,
            artist: artist_name.to_string(),
            client: client.clone(),
        });
        tracks.extend(
            details
                .songs
                .into_iter()
                .filter(|s| s.artist == artist_name)
                .map(|s| SearchItem::Song {
                    id: s.id,
                    title: s.title,
                    artist: s.artist,
                    album: s.album,
                    client: client.clone(),
                }),
        );
    }
    Ok((tracks, albums))
}
/// Local files flip the persisted favorites set; remote songs call
/// navidrome's own star/unstar so it stays in sync with any other
/// subsonic client hitting the same server. Returns the new state.
///
/// The remote half is **optimistic**: the local set flips and the next
/// frame shows it, while the server hears about it on a thread. A star
/// should feel instant, and this used to be a round trip on the
/// keypress. A refusal arrives on `star_tx` and the caller puts the
/// flip back; see `StarFailed`.
pub fn toggle_star(
    item: &SearchItem,
    favorites: &mut favorites::Favorites,
    remote_starred: &mut HashSet<String>,
    star_tx: &mpsc::Sender<StarFailed>,
) -> Result<bool, String> {
    match item {
        SearchItem::Silence { .. } => Err("a silence cannot be starred".to_string()),
        SearchItem::LocalMusic { path, .. } | SearchItem::LocalVideo { path, .. } => {
            let now_starred = favorites.toggle(&path.to_string_lossy());
            let _ = favorites::save(favorites);
            Ok(now_starred)
        }
        SearchItem::Song { id, client, .. } => {
            let wanted = !remote_starred.contains(id);
            if wanted {
                remote_starred.insert(id.clone());
            } else {
                remote_starred.remove(id);
            }
            let (client, id, tx) = (client.clone(), id.clone(), star_tx.clone());
            thread::spawn(move || {
                let sent = if wanted {
                    client.star(&id)
                } else {
                    client.unstar(&id)
                };
                if let Err(e) = sent {
                    // a dead receiver only means the app is shutting
                    // down, in which case the flip does not matter.
                    let _ = tx.send(StarFailed {
                        id,
                        wanted,
                        error: e.to_string(),
                    });
                }
            });
            Ok(wanted)
        }
        SearchItem::Artist { .. } | SearchItem::Album { .. } | SearchItem::Playlist { .. } => {
            Err("only tracks can be starred".to_string())
        }
    }
}
/// Starred songs browsed the same way an album is: merges every
/// configured server's own starred list with local favorites into one
/// `BrowseView`. Fetched fresh on open rather than cached, same as
/// opening an album, so it can't go stale.
/// A live client for `nav` if the corpus already holds one, else a
/// fresh one. `Client::new` re-reads `password_file` off disk and
/// starts with a cold connection pool, so building one per call is
/// paid for in both disk io and the first round trip.
///
/// Reuse rather than a cached singleton on purpose: these clients are
/// the corpus's own, so the password lives exactly as long as the
/// corpus does and is still zeroized when it drops. Parking one in a
/// `static` would mean it never drops and never zeroizes.
/// Drops any kept client no current profile still describes.
///
/// The sync thread holds clients across passes, so this is what stops
/// a removed or edited profile from being synced through a client
/// carrying the old credentials; dropping it is also what zeroizes
/// that password. `Client::matches` counts the stream options, so a
/// bitrate change is an edit too.
pub fn retain_live_clients(
    clients: &mut Vec<Arc<subsonic::Client>>,
    servers: &[config::NavidromeConfig],
) {
    clients.retain(|c| servers.iter().any(|nav| c.matches(nav)));
}
/// `Result` rather than `Option` so the reason survives: `Client::new`
/// fails on an unreadable `password_file` or a malformed url, and
/// "connection failed" with no cause is a worse error than the one
/// this replaced.
pub fn client_for(
    nav: &config::NavidromeConfig,
    live: &[Arc<subsonic::Client>],
) -> anyhow::Result<Arc<subsonic::Client>> {
    if let Some(live) = live.iter().find(|c| c.matches(nav)) {
        return Ok(live.clone());
    }
    Ok(Arc::new(subsonic::Client::new(nav)?))
}
/// Opens the starred listing through the browse loader, so the
/// keypress returns at once.
///
/// Local favorites are a file read and go into the view immediately;
/// the per-server fetches run on the loader's thread and fold in when
/// they answer. This used to fan out and join before returning, so
/// pressing the key paid for the slowest configured server.
pub fn open_starred(
    loader: &mut browse::Loader,
    servers: &[config::NavidromeConfig],
    favorites: &favorites::Favorites,
    live: &[Arc<subsonic::Client>],
) -> browse::BrowseView {
    let local: Vec<SearchItem> = favorites
        .paths()
        .filter_map(|path| persistence::item_from_path(path))
        .collect();
    // resolved here rather than in the loader: `client_for` lives in
    // this module and reuses a live client instead of re-reading
    // `password_file` and starting with a cold pool.
    let clients: Vec<Arc<subsonic::Client>> = servers
        .iter()
        .filter_map(|nav| client_for(nav, live).ok())
        .collect();
    loader.open_starred(local, clients)
}
/// A subtitle file the background fetch wrote to disk. `sub_add` isn't
/// called there: the `Mpv` handle isn't `Send`, so the main thread
/// loads it when this lands, same division of labor as every other
/// background worker in this file.
/// A star the server refused, so the optimistic local flip has to go
/// back. Carries what the local set was set *to*, which is what
/// reverting needs; the id alone would not say which way.
#[derive(Debug, Clone)]
pub struct StarFailed {
    pub id: String,
    pub wanted: bool,
    pub error: String,
}

pub struct SubtitleReady {
    pub path: String,
    pub release: String,
    pub language: String,
}
/// Starts an opensubtitles search + download on its own thread, same
/// fire-and-forget contract as `fire_playing_now`: a no-op whenever
/// `item` isn't a local video, no api key is configured, or the
/// search/download fails for any reason (no matches, network error,
/// quota exhausted). Two network round-trips, so it never runs inline;
/// it used to, which on startup meant holding the first frame behind
/// opensubtitles' response time.
pub fn fire_subtitle_fetch(
    cfg: &config::Config,
    subtitle_tx: &mpsc::Sender<SubtitleReady>,
    item: &SearchItem,
) {
    let SearchItem::LocalVideo { path, title } = item else {
        return;
    };
    let Some(api_key) = cfg.resolve_opensubtitles_api_key() else {
        return;
    };
    let language = cfg.subtitle_language.clone();
    let title = title.clone();
    let sub_path = path.with_extension("wim-auto.srt");
    let subtitle_tx = subtitle_tx.clone();
    thread::spawn(move || {
        let client = opensubtitles::Client::new(api_key);
        let Ok(results) = client.search(&title, &language) else {
            return;
        };
        let Some(best) = results.first() else {
            return;
        };
        let Ok(bytes) = client.download(best.file_id) else {
            return;
        };
        if std::fs::write(&sub_path, bytes).is_err() {
            return;
        }
        let _ = subtitle_tx.send(SubtitleReady {
            path: sub_path.to_string_lossy().into_owned(),
            release: best.release.clone(),
            language: best.language.clone(),
        });
    });
}
/// What a reply's text is, by the api's own `instrumental` flag rather
/// than "both fields happened to be empty": a real track with missing
/// lyrics data must not be mislabelled instrumental. Shared so wim's
/// pane and shellswim's console cannot disagree about one reply.
pub fn lyrics_text(l: &lyrics::Lyrics) -> String {
    if l.instrumental {
        return "(instrumental, no lyrics)".to_string();
    }
    l.plain
        .clone()
        .or_else(|| l.synced.clone())
        .unwrap_or_else(|| "(no lyrics text available)".to_string())
}

/// (artist, title, album) for a lyrics lookup; only meaningful for
/// local music; `SearchItem` doesn't carry duration, so lookups always
/// fall through to lrclib's fuzzy `/search` rather than its exact
/// `/get`. See ROADMAP.md.
pub fn lyrics_query(item: &SearchItem) -> Option<(String, String, String)> {
    match item {
        SearchItem::LocalMusic {
            title,
            artist,
            album,
            ..
        } => Some((
            artist.clone().unwrap_or_default(),
            title.clone(),
            album.clone().unwrap_or_default(),
        )),
        _ => None,
    }
}
/// `None` for anything that isn't a single track with a known artist:
/// local video has no artist field at all, and remote browse entries
/// (artist/album/playlist) never reach `now_playing` directly, they're
/// always expanded into `Song`s first via `resolve_tracks`.
pub fn scrobble_metadata(item: &SearchItem) -> Option<listenbrainz::TrackMetadata> {
    match item {
        SearchItem::LocalMusic {
            title,
            artist,
            album,
            ..
        } => Some(listenbrainz::TrackMetadata {
            artist_name: artist.clone()?,
            track_name: title.clone(),
            release_name: album.clone(),
        }),
        SearchItem::Song {
            title,
            artist,
            album,
            ..
        } => Some(listenbrainz::TrackMetadata {
            artist_name: artist.clone(),
            track_name: title.clone(),
            release_name: if album.is_empty() {
                None
            } else {
                Some(album.clone())
            },
        }),
        // the site that earned `Silence` its own variant rather than a
        // `LocalMusic` with an `av://` path: that spelling costs no
        // match arms and would have scrobbled a silence to
        // listenbrainz as a track called "silence".
        SearchItem::Silence { .. }
        | SearchItem::LocalVideo { .. }
        | SearchItem::Artist { .. }
        | SearchItem::Album { .. }
        | SearchItem::Playlist { .. } => None,
    }
}
/// Fires a "playing_now" listen on a background thread; fire-and-
/// forget, a slow/failed ListenBrainz request must never block
/// playback or the ui. A no-op if listenbrainz isn't configured (no
/// token) or `item` has no scrobbleable metadata.
pub fn fire_playing_now(cfg: &config::Config, item: &SearchItem) {
    let Some(meta) = scrobble_metadata(item) else {
        return;
    };
    let Some(client) = listenbrainz::Client::from_config(cfg) else {
        return;
    };
    thread::spawn(move || {
        let _ = client.submit_playing_now(&meta);
    });
}
/// Fires a "single" (scrobble) listen on a background thread; same
/// fire-and-forget contract as `fire_playing_now`.
pub fn fire_scrobble(cfg: &config::Config, item: &SearchItem, listened_at: i64) {
    let Some(meta) = scrobble_metadata(item) else {
        return;
    };
    let Some(client) = listenbrainz::Client::from_config(cfg) else {
        return;
    };
    thread::spawn(move || {
        let _ = client.submit_single(&meta, listened_at);
    });
}
/// Per-track scrobble bookkeeping, bundled into one struct (rather than
/// two loose locals threaded through every playback function) so
/// `go_to_previous` doesn't trip clippy's argument-count limit.
/// `start_tracking_playback` resets this every time `now_playing`
/// changes to a new track.
#[derive(Default)]
pub struct ScrobbleState {
    /// Set once the "single" listen has fired for the current track,
    /// so the per-tick threshold check (main loop) only ever fires it
    /// once, not every tick after the threshold is crossed.
    pub scrobbled: bool,
    /// The current track's own start time, not whenever a scrobble
    /// happens to fire; used as the `single` submission's
    /// `listened_at`.
    pub started_unix: Option<i64>,
}
/// Called from every site that sets `now_playing` to a new `Some(..)`:
/// resets the per-track scrobble bookkeeping and fires "playing_now".
/// Centralized here so each call site is one line instead of
/// repeating all three steps (easy to silently miss one otherwise).
pub fn start_tracking_playback(
    cfg: &config::Config,
    item: &SearchItem,
    scrobble: &mut ScrobbleState,
) {
    scrobble.scrobbled = false;
    scrobble.started_unix = Some(listenbrainz::now_unix());
    fire_playing_now(cfg, item);
}
/// Replaces mpv's whole playlist and plays `item` immediately. Returns
/// the status line and whether it actually started playing, so the
/// caller can update `now_playing` only on success. Callers are
/// expected to have already run `item` through `resolve_tracks`, so in
/// practice `playable_path` never actually returns `None` here; that
/// arm only exists because `playable_path` is total over `SearchItem`.
pub fn try_play(
    player: &mpv::Mpv,
    item: &SearchItem,
    cache: &Arc<Mutex<CacheIndex>>,
) -> (String, bool) {
    match resolve_playable(item, cache) {
        Some(path) => match player.load_file(&path) {
            Ok(()) => {
                set_vid_for_item(player, item);
                (format!("playing: {}", label(item)), true)
            }
            Err(e) => (format!("error loading file: {e}"), false),
        },
        None => (
            "not directly playable (expand via resolve_tracks first)".to_string(),
            false,
        ),
    }
}
/// mpv defaults to opening its own native window for any file with a
/// video track, including showing embedded cover art as a static
/// "video" for plain audio files; explicitly disabling it for
/// anything that isn't actually `LocalVideo` stops a window from
/// popping up for ordinary music playback. Properly embedding actual
/// video in the workspace is separate, larger work; this is the
/// narrow fix for audio never deserving a window at all.
pub fn set_vid_for_item(player: &mpv::Mpv, item: &SearchItem) {
    let vid = if matches!(item, SearchItem::LocalVideo { .. }) {
        serde_json::json!("auto")
    } else {
        serde_json::json!("no")
    };
    let _ = player.set_property("vid", vid);
}
/// Whether a playlist rewrite starts a fresh session (so "loop all"
/// restarts from it) or just re-lays-out the session already running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStart {
    New,
    Keep,
}
/// The only way mpv's playlist is ever replaced wholesale: `items[0]`
/// takes over from whatever was loaded, the rest are appended for
/// gapless auto-advance, and `queue` is re-pointed at that exact list
/// so index `i` resolves to `items[i]`. Writing both sides here (and in
/// `enqueue_track`/`resync_upcoming`) is what keeps the index map
/// honest; the caller still fires its own track-change hooks for
/// `items[0]`, since `queue` already knows it is at position 0 and will
/// report mpv's confirming `playlist-pos` as `Unchanged`.
pub fn load_playlist(
    player: &mpv::Mpv,
    cache: &Arc<Mutex<CacheIndex>>,
    queue: &mut PlayQueue,
    items: Vec<SearchItem>,
    session: SessionStart,
) -> (String, bool) {
    let total = items.len();
    let (playable, dropped) = resolve_all(items, cache);
    let Some((first, _)) = playable.first() else {
        return (
            if total == 0 {
                "nothing to play".to_string()
            } else {
                format!("nothing playable here ({total} entries could not be resolved)")
            },
            false,
        );
    };
    let (status, played) = try_play(player, first, cache);
    if !played {
        return (status, false);
    }
    // only what mpv actually took is stored, which is the whole point:
    // storing an item mpv refused puts every later index off by one.
    let mut kept = Vec::with_capacity(playable.len());
    let mut refused = 0;
    for (i, (item, path)) in playable.into_iter().enumerate() {
        // index 0 is already loaded, by `try_play` above.
        if i == 0 || player.playlist_append(&path).is_ok() {
            kept.push(item);
        } else {
            refused += 1;
        }
    }
    match session {
        SessionStart::New => queue.reset(kept),
        SessionStart::Keep => queue.rebuild(kept),
    }
    (
        format!("{status}{}", lost_note(dropped.len() + refused)),
        true,
    )
}

/// Splits `items` into the ones mpv can be handed a path for, paired
/// with that path, and the ones it cannot.
///
/// **The pairing is the fix.** `load_playlist` and `resync_upcoming`
/// each used to skip an unresolvable item while appending to mpv and
/// then store the *whole* vec in `PlayQueue`, breaking the one
/// invariant the index map has (`entries` is mpv's playlist: same
/// items, same order, same length). Every index after the skipped one
/// is then off by one, so an mpv `playlist-pos` resolves to the wrong
/// track: wrong title on the bar, wrong scrobble, wrong art.
///
/// It was unreachable only by luck, because every caller pre-ran
/// `resolve_tracks`; a queue entry kind that does not resolve reaches
/// it immediately, which the timeline's `silence(15m)` would have
/// been.
fn resolve_all(
    items: Vec<SearchItem>,
    cache: &Arc<Mutex<CacheIndex>>,
) -> (Vec<(SearchItem, String)>, Vec<SearchItem>) {
    let mut playable = Vec::with_capacity(items.len());
    let mut dropped = Vec::new();
    for item in items {
        match resolve_playable(&item, cache) {
            Some(path) => playable.push((item, path)),
            None => dropped.push(item),
        }
    }
    (playable, dropped)
}

/// " (n skipped)", or nothing at all when none were.
///
/// Same reasoning as `queues::summarise`: a silently shortened queue is
/// the bug, so the count is always said, but it is a suffix on a status
/// line rather than its own toast, because in the normal case there is
/// nothing to say and the useful half of the message is what played.
fn lost_note(lost: usize) -> String {
    if lost == 0 {
        String::new()
    } else {
        format!(" ({lost} skipped)")
    }
}
/// Appends `item` to mpv's own playlist and to the index map together
/// (plays immediately if nothing was loaded, otherwise queues
/// gaplessly; mpv auto-advances its own playlist with none of the
/// round-trip gap reloading on our end would introduce). While shuffle
/// is on the new entry is then moved into a random upcoming slot, which
/// `App::enqueue` used to do to its own queue copy alone, desyncing it
/// from mpv's strictly-appended playlist on the spot.
pub fn enqueue_track(
    player: &mpv::Mpv,
    item: &SearchItem,
    cache: &Arc<Mutex<CacheIndex>>,
    queue: &mut PlayQueue,
    shuffle: bool,
) -> String {
    let Some(path) = resolve_playable(item, cache) else {
        return "not directly playable (expand via resolve_tracks first)".to_string();
    };
    if let Err(e) = player.playlist_append(&path) {
        return format!("error queuing file: {e}");
    }
    let appended = queue.append(item.clone(), shuffle);
    if let Some(to) = appended.moved_to {
        let _ = player.playlist_move(appended.appended_at, to);
    }
    format!("queued: {}", label(item))
}
/// Resyncs mpv's upcoming playlist to `items` (after a shuffle)
/// without disturbing whatever's currently playing; mpv's own
/// `playlist-clear` keeps the current file and drops the rest, so both
/// sides end up as `[current] + items` at position 0.
pub fn resync_upcoming(
    player: &mpv::Mpv,
    cache: &Arc<Mutex<CacheIndex>>,
    queue: &mut PlayQueue,
    items: Vec<SearchItem>,
) -> String {
    if let Err(e) = player.playlist_clear() {
        return format!("error resyncing queue: {e}");
    }
    let (playable, dropped) = resolve_all(items, cache);
    let mut kept = Vec::with_capacity(playable.len());
    let mut failure = None;
    for (item, path) in playable {
        match player.playlist_append(&path) {
            Ok(()) => kept.push(item),
            Err(e) => {
                failure = Some(e.to_string());
                break;
            }
        }
    }
    // updated even when an append failed partway, because mpv has
    // *already been cleared*: returning early and leaving `entries` as
    // it was would be exactly the desync this function exists to
    // prevent, and a worse one than the skip bug (every index wrong,
    // not just the ones after a gap).
    queue.replace_upcoming(kept);
    match failure {
        Some(e) => format!("error resyncing queue: {e}"),
        None => format!("shuffled{}", lost_note(dropped.len())),
    }
}
/// Snapshots now-playing/queue/position/volume for next launch to
/// resume. Best-effort: a failed save just means nothing to resume
/// next time, not worth surfacing since we're already quitting.
pub fn save_playback_state(player: &mpv::Mpv, queue: &PlayQueue) {
    let state = persistence::PlaybackState {
        now_playing_path: queue.current().and_then(playable_path),
        queue_paths: queue.upcoming().iter().filter_map(playable_path).collect(),
        position_secs: player
            .get_property("time-pos")
            .ok()
            .and_then(|v| v.as_f64()),
        volume: player.get_property("volume").ok().and_then(|v| v.as_f64()),
    };
    let _ = persistence::save(&state);
}
/// Bundles everything the auto/manual download paths need to touch:
/// the shared cache index, the channel to report progress back on,
/// and the set of track ids currently mid-transfer. One struct instead
/// of four separate parameters, mostly so `go_to_previous` (which
/// already takes half a dozen other params) doesn't trip clippy's
/// too-many-arguments lint; see `note_playback_started`/
/// `trigger_downloads`, which take the same bundle.
pub struct DownloadCtx<'a> {
    pub cache: &'a Arc<Mutex<CacheIndex>>,
    pub download_tx: &'a mpsc::Sender<DownloadEvent>,
    pub downloading: &'a mut HashSet<String>,
    /// Distinct play days a track needs before it auto-downloads; see
    /// `config::Config::cache_days_threshold`.
    pub day_threshold: u32,
}
/// Bundles the two independent "a track just started" trackers
/// (download-on-repeat-listen, scrobble-on-threshold) into one
/// argument, purely to keep `go_to_previous` under clippy's
/// too-many-arguments limit; each one is otherwise unrelated to the
/// other and gets built/used on its own everywhere else.
pub struct TrackChangeCtx<'a, 'b> {
    pub dctx: &'a mut DownloadCtx<'b>,
    pub scrobble: &'a mut ScrobbleState,
    /// where `fire_subtitle_fetch` reports back; bundled here for the
    /// same argument-count reason as everything else in this struct.
    pub subtitle_tx: &'a mpsc::Sender<SubtitleReady>,
}
/// "Back" re-plays whatever was playing before the current track,
/// putting the interrupted track right behind it so playback picks up
/// where it would have anyway. Deliberately our own bookkeeping rather
/// than mpv's `playlist-prev`: the already-played prefix of mpv's
/// playlist is dropped by every `playlist-clear`, so prev isn't
/// reliably still there to step back onto. Rebuilding the playlist as
/// `[prev, interrupted, ...upcoming]` through `load_playlist` keeps
/// mpv and the index map written together, which inserting into a
/// separate queue copy (while mpv's own playlist got replaced out from
/// under it) did not. Only ever one step back, not a full history.
pub fn go_to_previous(
    cfg: &config::Config,
    player: &mpv::Mpv,
    notifications: &mut Notifications,
    previous_track: &mut Option<SearchItem>,
    queue: &mut PlayQueue,
    hooks: &mut TrackChangeCtx,
) -> String {
    let Some(prev) = previous_track.take() else {
        return "no previous track".to_string();
    };
    let mut items = vec![prev.clone()];
    items.extend(queue.current().cloned());
    items.extend(queue.upcoming().to_vec());
    let (status, played) =
        load_playlist(player, hooks.dctx.cache, queue, items, SessionStart::Keep);
    if played {
        fire_subtitle_fetch(cfg, hooks.subtitle_tx, &prev);
        note_playback_started(&prev, hooks.dctx, notifications);
        start_tracking_playback(cfg, &prev, hooks.scrobble);
    } else {
        *previous_track = Some(prev);
    }
    status
}
/// What the main loop should do once an action has been handled.
/// `Action::Quit` has to break that loop, which a called function
/// can't do for its caller, so it reports back instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dispatch {
    Continue,
    Quit,
}
/// The tracks enter on a browse row plays: the selected one through
/// the end of what the filter actually shows, not of the whole list (a
/// filtered artist page used to play the unfiltered tail). Shared by
/// the browse view's own enter and shellsong's `go()`, so the two
/// cannot pick different tracks.
pub fn browse_session(bv: &browse::BrowseView) -> Vec<SearchItem> {
    let visible = bv.visible_track_indices();
    let from = visible.iter().position(|&i| i == bv.selected).unwrap_or(0);
    visible[from..]
        .iter()
        .filter_map(|&i| bv.tracks.get(i).cloned())
        .collect()
}
/// `main::Focus` is the host's, `shellsong::Focus` the language's
/// mirror of it (kept separate so the language depends on nothing in
/// the ui). One to one.
pub fn shellsong_focus(focus: Focus) -> shellsong::Focus {
    match focus {
        Focus::Sidebar => shellsong::Focus::Sidebar,
        Focus::Workspace => shellsong::Focus::Workspace,
        Focus::BottomBar => shellsong::Focus::BottomBar,
    }
}
/// mpv's current volume, which `volume(+10%)` nudges from and a fade
/// ramps away from. `None` only if the ipc read fails.
pub fn current_volume(player: &mpv::Mpv) -> Option<f64> {
    player.get_property("volume").ok().and_then(|v| v.as_f64())
}
/// Whether `item` is starred right now, so `star()`/`unstar()` can set
/// a state rather than flip one (`toggle_star` only flips).
pub fn is_starred(
    item: &SearchItem,
    favorites: &favorites::Favorites,
    remote_starred: &HashSet<String>,
) -> bool {
    match item {
        SearchItem::LocalMusic { path, .. } | SearchItem::LocalVideo { path, .. } => {
            favorites.is_starred(&path.to_string_lossy())
        }
        SearchItem::Song { id, .. } => remote_starred.contains(id),
        SearchItem::Silence { .. }
        | SearchItem::Artist { .. }
        | SearchItem::Album { .. }
        | SearchItem::Playlist { .. } => false,
    }
}
/// The album page a song belongs to. A `Song` carries its album's
/// *name*, not its id, so this can only find an album the corpus has
/// already indexed; `view(q, ...)` landing on the album itself is the
/// direct route.
pub fn album_page(
    corpus: &search::Corpus,
    album: &str,
    client: &Arc<subsonic::Client>,
) -> Option<SearchItem> {
    corpus
        .items()
        .find(|item| match item {
            SearchItem::Album {
                name, client: from, ..
            } => name == album && from.base_url() == client.base_url(),
            _ => false,
        })
        .cloned()
}
/// What one op reported into the scrollback. `Described` takes
/// `Op::describe()`, which is usually the right line; the host
/// overrides with `Note` when it knows a better one (a track title, a
/// count) and with `Failed` when the op could not be applied. Every op
/// produces exactly one of these: nothing in shellsong is silent
/// (docs/shellsong.md R9).
pub enum Report {
    Described,
    Note(String),
    Failed(String),
}
pub fn op_done() -> (Dispatch, Report) {
    (Dispatch::Continue, Report::Described)
}
pub fn op_note(text: impl Into<String>) -> (Dispatch, Report) {
    (Dispatch::Continue, Report::Note(text.into()))
}
pub fn op_fail(text: impl Into<String>) -> (Dispatch, Report) {
    (Dispatch::Continue, Report::Failed(text.into()))
}
/// One ipc write, reported either way; mpv failing is the one thing
/// most of the transport ops can do wrong.
pub fn op_ipc(result: anyhow::Result<()>) -> (Dispatch, Report) {
    match result {
        Ok(()) => op_done(),
        Err(e) => op_fail(e.to_string()),
    }
}
/// Which way a selection op moves. `up()`/`down()` carry a delta,
/// `top()`/`bottom()` are ends.
pub enum SelectStep {
    By(i32),
    Top,
    Bottom,
}
/// Whether a play that just happened should kick off a background
/// download. Pure, and kept apart from the clock and the cache file so
/// the policy itself is testable: `days_played` is the number of
/// *distinct calendar days* the track has been played on, never the
/// raw play count, which is what made pressing enter twice in one
/// sitting download a track nobody asked to cache.
pub fn should_auto_download(
    days_played: u32,
    day_threshold: u32,
    already_cached: bool,
    in_flight: bool,
) -> bool {
    days_played >= day_threshold.max(1) && !already_cached && !in_flight
}
/// Called every time a remote song actually starts playing (every
/// track-change call site threads this through): records the play
/// against today's date and, once the track has been played on
/// `dctx.day_threshold` distinct days, kicks off a background download
/// so it's cached for next time (see `cache.rs`/`download.rs`). A
/// no-op for local items (never cached at all) and for anything
/// already cached or already mid-download.
///
/// `today` is read off the clock here and nowhere deeper; everything
/// that actually decides anything takes the day as an argument.
///
/// This is also the one call site a future scrobble submission
/// (ListenBrainz, separate work happening elsewhere) would hook into:
/// by the time this runs, the track has definitely started playing.
pub fn note_playback_started(
    item: &SearchItem,
    dctx: &mut DownloadCtx,
    notifications: &mut Notifications,
) {
    let SearchItem::Song {
        id,
        title,
        artist,
        client,
        ..
    } = item
    else {
        return;
    };
    let today = cache::today();
    let (days, already_cached) = match dctx.cache.lock() {
        Ok(mut idx) => {
            let days = idx.record_play(id, &today);
            let _ = idx.save();
            (days, idx.cached_path(id).is_some())
        }
        Err(_) => return,
    };
    if !should_auto_download(
        days,
        dctx.day_threshold,
        already_cached,
        dctx.downloading.contains(id),
    ) {
        return;
    }
    dctx.downloading.insert(id.clone());
    let track_label = format!("{title} · {artist}");
    notifications.push_progress(id.clone(), track_label.clone());
    download::spawn_tracks(
        vec![(id.clone(), track_label, client.stream_url(id))],
        dctx.cache.clone(),
        dctx.download_tx.clone(),
    );
}
/// Backs the manual download keybind (`KeyAction::DownloadSelected`)
/// once its tracks are in hand: downloads every remote song in
/// `tracks` concurrently (one thread each, see
/// `download::spawn_tracks`), skipping anything already cached,
/// already downloading, or not actually a remote track (local files
/// are already local, nothing to cache). Deliberately ignores
/// `day_threshold`: asking for a download outright is explicit, and
/// has no business waiting on a listening-habit heuristic.
///
/// Expanding an album or artist into those tracks is `resolve_tracks`,
/// which is a network fan-out and so happens in `TrackResolver` rather
/// than here; this used to begin with that call and was the last
/// blocking request left on the input path.
pub fn download_tracks(
    tracks: &[SearchItem],
    dctx: &mut DownloadCtx,
    notifications: &mut Notifications,
) -> Downloaded {
    let mut batch = Vec::new();
    let mut already_cached = 0;
    let mut in_flight = 0;
    for t in tracks {
        let SearchItem::Song {
            id,
            title,
            artist,
            client,
            ..
        } = t
        else {
            continue;
        };
        let cached = dctx
            .cache
            .lock()
            .ok()
            .is_some_and(|idx| idx.cached_path(id).is_some());
        if cached {
            already_cached += 1;
            continue;
        }
        if dctx.downloading.contains(id) {
            in_flight += 1;
            continue;
        }
        dctx.downloading.insert(id.clone());
        let track_label = format!("{title} · {artist}");
        notifications.push_progress(id.clone(), track_label.clone());
        batch.push((id.clone(), track_label, client.stream_url(id)));
    }
    let started = batch.len();
    if started > 0 {
        download::spawn_tracks(batch, dctx.cache.clone(), dctx.download_tx.clone());
    }
    let status = if started > 0 {
        format!("downloading {started} track(s)")
    } else if already_cached > 0 {
        "already cached".to_string()
    } else if in_flight > 0 {
        "already downloading".to_string()
    } else {
        "nothing to download (local files are already local)".to_string()
    };
    Downloaded { started, status }
}

/// What a resolved download batch did.
///
/// The count is separate from the message because the two keypress
/// paths want different things from it: every started track already
/// pushes its own progress toast, so repeating "downloading 3
/// track(s)" over the top of three progress bars is noise, while
/// "already cached" has nothing else to show it and used to be
/// dropped on the floor (both key sites did `let _ =`, so asking to
/// download something already on disk looked like the key did
/// nothing). `Op::Download` reports the message either way, since a
/// console line is the whole answer there.
pub struct Downloaded {
    pub started: usize,
    pub status: String,
}

/// `download_tracks` with the bundle built from `core`, so the three
/// call sites that have a `CoreCtx` do not each write the same
/// `DownloadCtx` literal.
pub fn download_resolved(tracks: &[SearchItem], core: &mut CoreCtx) -> Downloaded {
    let mut dctx = DownloadCtx {
        cache: core.cache,
        download_tx: core.download_tx,
        downloading: core.downloading,
        day_threshold: core.cfg.cache_days_threshold,
    };
    download_tracks(tracks, &mut dctx, core.notifications)
}
/// Deletes every cached file past `cfg.cache_eviction_days` unplayed;
/// best-effort, same as every other cache operation (a failed save
/// just means the eviction is re-attempted next sweep).
pub fn sweep_cache(cache: &Arc<Mutex<CacheIndex>>, eviction_days: u64) {
    if let Ok(mut idx) = cache.lock() {
        if idx.sweep_stale(eviction_days.saturating_mul(24 * 3600)) > 0 {
            let _ = idx.save();
        }
    }
}
/// The one starred set `suggest::Request` wants: remote song ids and
/// local favorite paths together, matching `suggest::Track::key`.
pub fn suggestion_starred_keys(
    remote_starred: &HashSet<String>,
    favorites: &favorites::Favorites,
) -> HashSet<String> {
    remote_starred
        .iter()
        .cloned()
        .chain(favorites.paths().cloned())
        .collect()
}
/// Enter on a preset row of the eq menu applies it, built-in or
/// saved; a no-op when `row` is a band row instead, since adjusting
/// those is `adjust_eq_band`'s job. Deliberately not wired to j/k:
/// this overwrites all ten gains, which is not something plain
/// navigation should ever do.
/// Saved eq curve names, for shellsong's `eq()` completion. Built
/// fresh per keystroke like the corpus candidates are; the list is a
/// handful of short strings, so cloning beats threading a borrow
/// through `CorpusCandidates`'s lifetime.
pub fn eq_preset_names(saved: &eq::CustomPresets) -> Vec<String> {
    saved.iter().map(|p| p.name.clone()).collect()
}
pub fn apply_eq_row(
    app: &mut app::App,
    player: &mpv::Mpv,
    notifications: &mut Notifications,
    saved: &eq::CustomPresets,
    row: usize,
) {
    let (source, gains) = match ui::eq_menu::cursor(row, saved.len()) {
        ui::eq_menu::Cursor::Builtin(i) => {
            // keep the `CycleEq` key's cycle where the menu left it,
            // so 'e' carries on from the preset you just picked.
            app.eq_preset = i;
            (eq::Source::Builtin(i), eq::PRESETS[i].gains)
        }
        ui::eq_menu::Cursor::Saved(i) => match saved.get(i) {
            Some(preset) => (eq::Source::Saved(i), eq::clamp_gains(preset.gains)),
            None => return,
        },
        ui::eq_menu::Cursor::Band(_) => return,
    };
    app.eq_source = Some(source);
    app.eq_custom = false;
    app.eq_gains = gains;
    apply_eq_gains(app, player, notifications);
}
/// Pushes `app.eq_gains` to mpv, reporting a failure: the menu paths
/// used to discard the error while the global cycle-eq key reported
/// it, so the same failure was visible from one route and silent from
/// the other.
pub fn apply_eq_gains(app: &app::App, player: &mpv::Mpv, notifications: &mut Notifications) {
    if let Err(e) = player.set_af(&eq::build_af_filter(&app.eq_gains)) {
        notifications.push(Severity::Error, format!("error setting eq: {e}"));
    }
}
/// How long to let rapid keystrokes settle before actually firing a
/// `search3` request, so a burst of typing costs one network round-
/// trip instead of one per character; see `spawn_background_search`.
pub const REMOTE_SEARCH_DEBOUNCE: Duration = Duration::from_millis(300);
/// One query-changed event, carrying the servers to search against at
/// the time it was sent; `Corpus::remote_clients` (already-indexed
/// servers only) is cheap to call per-keystroke on the main thread, so
/// the snapshot travels with the query rather than the background
/// thread needing its own separate copy of the server list to keep in
/// sync.
pub struct RemoteSearchQuery {
    pub query: String,
    pub clients: Vec<Arc<subsonic::Client>>,
}
/// One server's `search3` result for whatever query it was fired for;
/// not tagged with the query itself; see `spawn_background_search`'s
/// doc for why that's fine.
pub struct RemoteSearchOutcome {
    pub client: Arc<subsonic::Client>,
    pub result: subsonic::SearchResult,
}
/// Runs forever on its own thread, debouncing query-changed events
/// from the main thread and firing Subsonic's `search3` against every
/// already-indexed navidrome server once typing settles. This is the
/// live counterpart to `fetch_navidrome_catalog`'s eager indexing:
/// song titles are deliberately never indexed up front (a library's
/// songs could be enormous; see that function's own comment), so
/// without this, searching a song by title only ever worked by
/// coincidence. Same shape as `spawn_background_sync`: pure network
/// I/O on its own thread, result handed back over a channel, main
/// thread owns all the `!Send` state; just triggered by query changes
/// instead of a fixed timer.
///
/// Results aren't tagged with the query they answer and are merged
/// into the corpus unconditionally rather than discarded if the query
/// has since changed: `Corpus::add_subsonic_search_result` already
/// dedups against what's indexed, so a stale answer is harmless to
/// merge in (it simply enriches the corpus for whatever's searched
/// next) and the subsequent `resync_results()` re-filters against
/// whatever the *current* query actually is, so a stale result can
/// never flash on screen for the wrong query.
pub fn spawn_background_search(
    query_rx: mpsc::Receiver<RemoteSearchQuery>,
    result_tx: mpsc::Sender<RemoteSearchOutcome>,
) {
    thread::spawn(move || loop {
        let mut pending = match query_rx.recv() {
            Ok(q) => q,
            Err(_) => return, // main thread gone (shutting down)
        };
        // debounce: keep absorbing newer queries until typing settles
        // for a full window, so a burst of keystrokes fires exactly
        // one request rather than one per character.
        loop {
            match query_rx.recv_timeout(REMOTE_SEARCH_DEBOUNCE) {
                Ok(newer) => pending = newer,
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
        if pending.query.trim().is_empty() || pending.clients.is_empty() {
            continue;
        }
        // independent per-server requests, same reasoning as every
        // other multi-server fetch in this file: run them
        // concurrently rather than making every server wait its turn.
        let outcomes: Vec<RemoteSearchOutcome> = thread::scope(|scope| {
            pending
                .clients
                .iter()
                .map(|client| {
                    let query = pending.query.clone();
                    let client = client.clone();
                    scope.spawn(move || (client.clone(), client.search3(&query)))
                })
                .collect::<Vec<_>>()
                .into_iter()
                .filter_map(|handle| {
                    let (client, result) = handle.join().ok()?;
                    Some(RemoteSearchOutcome {
                        client,
                        result: result.ok()?,
                    })
                })
                .collect()
        });
        for outcome in outcomes {
            if result_tx.send(outcome).is_err() {
                return; // main thread is gone (shutting down)
            }
        }
    });
}
/// Everything an op can touch that is not a pane: no `Focus`, no
/// browse view, no menu flags, no selection cursors. Both front-ends
/// build one of these per use, the way `DownloadCtx` already is; what
/// wim has beyond it stays on wim's side of `Surface`.
///
/// This field list is the contract, not `app::App`'s; see
/// docs/library-seam.md for what is still joined inside `App` and why
/// that was left alone.
pub struct CoreCtx<'a> {
    pub cfg: &'a config::Config,
    pub player: &'a mpv::Mpv,
    /// The session: corpus, query and result list, plus the playback
    /// modes. The interpreter resolves names against the results in
    /// both binaries, so they are core; `selected` and the key
    /// handling on the same struct belong to wim and are never read
    /// here.
    pub app: &'a mut app::App,
    pub notifications: &'a mut Notifications,
    /// mpv's playlist as an index -> item map; the only queue there
    /// is, and the only thing "what's playing" is read out of.
    pub queue: &'a mut PlayQueue,
    pub previous_track: &'a mut Option<SearchItem>,
    pub cache: &'a Arc<Mutex<CacheIndex>>,
    pub download_tx: &'a mpsc::Sender<DownloadEvent>,
    pub downloading: &'a mut HashSet<String>,
    pub scrobble: &'a mut ScrobbleState,
    pub subtitle_tx: &'a mpsc::Sender<SubtitleReady>,
    /// Where every op's one line lands (docs/shellsong.md R9).
    pub console: &'a mut Console,
    /// The same monotonic reading `Env::now` carries this frame, so a
    /// ramp started by an op and ticked next frame share one clock.
    pub now: Duration,
    pub now_playing: &'a Option<SearchItem>,
    pub favorites: &'a mut favorites::Favorites,
    pub remote_starred: &'a mut HashSet<String>,
    pub lyrics_client: &'a lyrics::Client,
    /// Fetches off-thread. `lookup` is a round trip to lrclib, and
    /// calling it from a key handler froze the whole ui until lrclib
    /// answered; "nothing blocks" is the rule in both binaries.
    pub lyrics_loader: &'a mut lyrics::Loader,
    /// Where a refused star reports back, so the optimistic flip can
    /// be undone; drained by each binary's own per-frame drain.
    pub star_tx: &'a mpsc::Sender<StarFailed>,
    /// Resolves an album or artist into tracks off the input path; see
    /// `TrackResolver`.
    pub track_resolver: &'a mut TrackResolver,
    // volume, and everything that ramps it.
    pub base_volume: &'a mut f64,
    pub crossfade_enabled: &'a mut bool,
    /// The undo tree. Written in `apply_op` and read by `Op::Undo`;
    /// see `src/undo.rs`.
    pub undo: &'a mut undo::Recorder,
    /// Seconds between tracks, live. `config::gap_seconds` is only the
    /// value this starts at; `gap()` changes it for the session, the
    /// same split `crossfade_enabled` already has.
    pub gap_secs: &'a mut f64,
    /// The gap currently being waited out, so `gap(off)` can drop a
    /// hold instead of leaving it to expire, and play/pause can skip
    /// one.
    pub between_tracks: &'a mut BetweenTracks,
    pub fade_in_started_at: &'a mut Option<Instant>,
    /// `fade(in|out)`'s ramp; `sleep_due` is its deadline-driven
    /// cousin. Both reuse the crossfade ramp's shape rather than
    /// cutting the audio dead.
    pub fade: &'a mut Option<Ramp>,
    pub sleep_due: &'a mut Option<Duration>,
    /// When a `pause(30s)` should let go again. The mirror of
    /// `sleep_due`: that one stops playback at a time, this one starts
    /// it.
    pub resume_due: &'a mut Option<Duration>,
    /// Replaced wholesale by `rescan()`, which needs a fresh channel:
    /// the scan thread signs off by dropping its sender.
    pub scan_rx: &'a mut mpsc::Receiver<library::ScanBatch>,
    pub remote_search_tx: &'a mpsc::Sender<RemoteSearchQuery>,
    pub settle: &'a mut SearchSettle,
    /// Snapshot behind `load_queue`'s completion; refreshed on open
    /// and after a save rather than read off disk per keystroke.
    pub saved_queues: &'a mut Vec<String>,
    /// The live `let` bindings. Held here rather than re-read per
    /// keystroke, and written back to disk by `define`/`unset`.
    pub vars: &'a mut shellsong::Scope,
    /// The per-identity settings, written back by `remember`/`forget`.
    pub bindings: &'a mut shellsong_host::Bindings,
}

/// The one place an `Action` from `app.handle_key` is acted on. Both
/// key paths route through here (ordinary keys, and a query being
/// typed); the typing path used to discard its action, which is why
/// the first enter after a search only closed the text box.
pub fn dispatch_action(action: Action, core: &mut CoreCtx, surf: &mut dyn Surface) -> Dispatch {
    // the keyboard's half of the undo tree. computed before the match,
    // which moves the action.
    let how = undo::classify_action(&action);
    let kind = undo::coalesce_action(&action);
    let label = action.describe();
    let record = how != undo::Reversibility::Ignored && !core.undo.is_suspended();
    // **no root is planted here, deliberately**, unlike `apply_op`.
    // several actions do not carry their own mutation: `app.handle_key`
    // has already flipped `loop_mode`/`shuffle`/`speed` by the time one
    // arrives, so a "before" captured here is really an "after" and
    // undoing the very first keypress of a session would restore the
    // state it had just left. the host plants the root once, up front,
    // before any key is read; see main.rs.
    let record = record && !core.undo.is_unrooted();
    let dispatch = dispatch_action_inner(action, core, surf);
    if record {
        let after = undo::Snapshot::capture(core);
        core.undo.record(after, label, kind, None, core.now);
    }
    dispatch
}

/// The actions themselves, split out so `dispatch_action` can record
/// around them without threading the hook through every `return` in
/// here.
fn dispatch_action_inner(action: Action, core: &mut CoreCtx, surf: &mut dyn Surface) -> Dispatch {
    match action {
        Action::Quit => {
            save_playback_state(core.player, core.queue);
            return Dispatch::Quit;
        }
        // albums/playlists/artists open a track-listing view instead
        // of playing immediately ("let me scroll through the songs"
        // first); songs/local files still play straight away.
        Action::Play(item @ SearchItem::Album { .. })
        | Action::Play(item @ SearchItem::Playlist { .. })
        | Action::Play(item @ SearchItem::Artist { .. }) => {
            // the view itself (title, and "loading..." until the
            // tracks land) is the confirmation; no status line echo.
            // nothing here waits on the network: the loader answers at
            // once and fills the view in from `Loader::poll`.
            // declined rather than opened by a front-end with no
            // pane to open it in; the report is dropped here because
            // this is the keypress path. `Op::Play` routes a page
            // through `Surface::open_browse` itself so the shell can
            // say why it will not.
            surf.open_browse(&item, core);
        }
        // a song or a local file, always: the arm above takes every
        // page, so nothing here needs resolving and no request is
        // made. this used to call `resolve_tracks` for a track list it
        // already had, which read like a network call on the keypress
        // and was recorded in ROADMAP as one.
        //
        // `minimize: false` because a standalone play (not from an
        // open album or playlist) takes over the full player.
        Action::Play(item) => {
            if let Err(why) = start_session(vec![item], false, core, surf) {
                core.notifications.push(Severity::Error, why);
            }
        }
        Action::Queued(item) => {
            // queueing has no on-screen consequence of its own (the
            // queue count on the bottom bar moves, but that's easy to
            // miss), so this is one of the few things worth an actual
            // toast.
            // an album is one round trip and an artist is a fan-out
            // over its albums, so this no longer happens on the input
            // path; a single song still queues instantly, since it
            // needs no request at all.
            match core.track_resolver.request(&item, Intent::Back) {
                Resolution::Ready(tracks) => append_resolved(&tracks, core),
                Resolution::Fetching(label) => core
                    .notifications
                    .push(Severity::Info, format!("queueing {label}...")),
            }
        }
        Action::ShuffleToggled(on) => {
            // turning it off leaves the order alone (nothing to put
            // back); turning it on reshuffles what's still upcoming so
            // it takes effect now, not only on later additions.
            if on {
                let shuffled = core.queue.shuffled_upcoming();
                let msg = resync_upcoming(core.player, core.cache, core.queue, shuffled);
                if msg.starts_with("error") {
                    core.notifications.push(Severity::Error, msg);
                    return Dispatch::Continue;
                }
            }
            surf.flash(Field::Shuffle);
        }
        Action::SetLoop(mode) => {
            // "loop all" isn't an mpv concept; it's tracked ourselves
            // (see `PlayQueue::session`) and acted on once mpv's
            // playlist drains, not here. mpv only ever needs to know
            // about single-file loop.
            let mpv_loop = if mode == LoopMode::One { "inf" } else { "no" };
            match core.player.set_loop_file(mpv_loop) {
                Ok(()) => surf.flash(Field::Loop),
                Err(e) => core
                    .notifications
                    .push(Severity::Error, format!("error setting loop: {e}")),
            }
        }
        Action::SetSpeed(speed) => match core.player.set_speed(speed) {
            Ok(()) => surf.flash(Field::Speed),
            Err(e) => core
                .notifications
                .push(Severity::Error, format!("error setting speed: {e}")),
        },
        Action::SetEq(_name, gains) => {
            let filter = eq::build_af_filter(&gains);
            match core.player.set_af(&filter) {
                Ok(()) => surf.flash(Field::Eq),
                Err(e) => core
                    .notifications
                    .push(Severity::Error, format!("error setting eq: {e}")),
            }
        }
        Action::None => {}
    }
    Dispatch::Continue
}
/// Everything a running program knows about the world, rebuilt from
/// live state before every `step`. `queue_len` counts the whole
/// playlist, played prefix included, because that is what the queue
/// view numbers and therefore what `goto(n)`/`remove(n)` mean.
/// Takes its pieces rather than a whole `CoreCtx` for one reason: a
/// `CoreCtx` holds a live mpv handle, and the env has nothing to do
/// with mpv. One builder either way, so the two front-ends cannot
/// disagree about what a program can see.
pub fn shellsong_env(
    app: &app::App,
    queue: &PlayQueue,
    surf: &dyn Surface,
    settled: bool,
    now: Duration,
) -> shellsong::Env {
    shellsong::Env {
        now,
        query: app.query.clone(),
        results: app.results.iter().map(shellsong::Kind::of).collect(),
        search_settled: settled,
        queue_len: queue.entries().len(),
        browsing: surf.browsing(),
        focus: surf.focus(),
    }
}
/// What a term-taking op acts on. `Index` is a row of the results
/// list; `Selection` is whatever is highlighted, resolved exactly the
/// way the keyboard's star/download keys resolve it (a focused browse
/// view owns the highlight, otherwise the sidebar does).
fn shellsong_item(target: Target, core: &CoreCtx, surf: &dyn Surface) -> Option<SearchItem> {
    match target {
        Target::Index(i) => core.app.results.get(i).cloned(),
        Target::Selection => surf.selection(core),
    }
}
/// Applies one op and reports exactly one line into the scrollback.
/// The report is `Op::describe()` unless the host knew something
/// better; `Op::closes_console` is consulted here, in one place,
/// rather than by each view-opener remembering to.
pub fn apply_op(op: Op, core: &mut CoreCtx, surf: &mut dyn Surface) -> Dispatch {
    let described = op.describe();
    let closes = op.closes_console();
    // the undo tree's one hook. `apply_op` is the only public way in;
    // `run_op` recurses into itself three times (a bar field, a
    // browse-row `go`, a settings call) and never back through here,
    // so one record per intent needs no re-entrance guard.
    let how = undo::classify(&op);
    let kind = undo::coalesce_kind(&op);
    let recordable = how != undo::Reversibility::Ignored;
    if recordable && core.undo.is_unrooted() {
        let before = undo::Snapshot::capture(core);
        core.undo.plant(before, core.now);
    }
    // twelve of `run_op`'s arms call `dispatch_action`, which records
    // on its own for a keypress; suspended here so a delegated op is
    // one row rather than two.
    let was = core.undo.suspend(true);
    let (dispatch, report) = run_op(op, core, surf);
    core.undo.suspend(was);
    // a failed op changed nothing, so recording it would put a row in
    // the tree that undoes to the state it is already in.
    if recordable && !matches!(report, Report::Failed(_)) {
        let after = undo::Snapshot::capture(core);
        let barrier = match how {
            undo::Reversibility::Barrier(why) => Some(why),
            _ => None,
        };
        core.undo
            .record(after, described.clone(), kind, barrier, core.now);
    }
    // only on success: closing the console to make room for a screen
    // that then failed to open would take the error message with it.
    if closes && !matches!(report, Report::Failed(_)) {
        surf.close_console();
    }
    match report {
        Report::Described => core.console.note(described),
        Report::Note(text) => core.console.note(text),
        Report::Failed(text) => core.console.fail(text),
    }
    dispatch
}
/// The op a bottom-bar control presses, or `None` for the scrubber,
/// which is a drag rather than a one-shot.
///
/// **One table, two callers**, which is the whole point: `Op::Go(
/// BottomBarField)` reads it for enter-on-the-bar, and main.rs's
/// `activate_transport` reads it for a keypress or a mouse click.
/// This arm's comment already claimed to reuse the commands "rather
/// than keeping a second copy of the bar's own match" -- and
/// `activate_transport` *was* that second copy, reimplementing six of
/// these against mpv directly, with its shuffle arm's own comment
/// admitting it duplicated the `s` key. Routing both through here also
/// means every bar control lands in the undo tree, since `apply_op`
/// is where recording happens.
pub fn op_for_field(field: Field) -> Option<Op> {
    Some(match field {
        Field::Pause => Op::TogglePause,
        Field::Loop => Op::CycleLoop,
        Field::Shuffle => Op::ToggleShuffle,
        Field::Speed => Op::SpeedUp,
        Field::Eq => Op::View(ViewTarget::Eq),
        Field::Crossfade => Op::ToggleCrossfade,
        Field::Progress => return None,
    })
}

/// The `Op` -> host table. Every arm goes through the call the matching
/// keypress already makes (`dispatch_action` wherever an `Action`
/// exists), so a shellsong command and a keybind can never diverge in
/// behaviour; see docs/shellsong.md.
fn run_op(op: Op, core: &mut CoreCtx, surf: &mut dyn Surface) -> (Dispatch, Report) {
    match op {
        Op::TogglePause => {
            if core.now_playing.is_none() {
                return op_fail("nothing is playing");
            }
            // skips the rest of a gap rather than unpausing into a
            // track that has already ended; see `core::toggle_pause`.
            op_ipc(toggle_pause(core.player, core.between_tracks))
        }
        Op::SkipNext(n) => {
            if core.now_playing.is_none() {
                return op_fail("nothing is playing");
            }
            // asked here rather than left to mpv: `playlist-next
            // weak` on the last entry replies with an error (measured;
            // see `Mpv::playlist_next`), so without this, skipping the
            // only track in the queue reported "error running
            // command" at the user for doing something reasonable.
            //
            // the queue is the right authority: `entries` *is* mpv's
            // playlist by invariant, so if nothing is upcoming here,
            // there is nothing for mpv to skip to either.
            let upcoming = core.queue.upcoming_len();
            if upcoming == 0 {
                return op_fail("nothing to skip to; this is the last track");
            }
            // a skip past the end lands on the last track rather than
            // refusing the whole thing: asking to go forward four with
            // two left plainly means "the end", and mpv would error on
            // the overshoot.
            let steps = n.min(upcoming);
            for _ in 0..steps {
                if let Err(e) = core.player.playlist_next() {
                    return op_fail(format!("error skipping: {e}"));
                }
            }
            if steps < n {
                return op_note(format!("skipped {steps}, which is all there was"));
            }
            op_done()
        }
        Op::PauseFor(secs) => {
            if core.now_playing.is_none() {
                return op_fail("nothing is playing");
            }
            // one timer, reused: `sleep_due` already means "do
            // something to playback at this time", and the step loop
            // already polls it. this is the other direction, so it is
            // its own field rather than overloading that one.
            if let Err(e) = core.player.set_pause(true) {
                return op_fail(format!("error pausing: {e}"));
            }
            *core.resume_due = Some(core.now + Duration::from_secs_f64(secs.max(0.0)));
            op_done()
        }
        Op::SkipBack => {
            if core.previous_track.is_none() {
                return op_fail("no previous track");
            }
            let mut dctx = DownloadCtx {
                cache: core.cache,
                download_tx: core.download_tx,
                downloading: core.downloading,
                day_threshold: core.cfg.cache_days_threshold,
            };
            let mut hooks = TrackChangeCtx {
                dctx: &mut dctx,
                scrobble: core.scrobble,
                subtitle_tx: core.subtitle_tx,
            };
            let status = go_to_previous(
                core.cfg,
                core.player,
                core.notifications,
                core.previous_track,
                core.queue,
                &mut hooks,
            );
            op_note(status)
        }
        Op::SeekAbsolute(pos) => {
            // asked for directly rather than read from the observed
            // cache: this runs once per command, not per frame, and a
            // percent target must resolve against the duration as of
            // now rather than whatever the last pushed change said.
            let duration = core
                .player
                .get_property(mpv::PROP_DURATION)
                .ok()
                .and_then(|v| v.as_f64());
            match shellsong_host::seek_target(pos, duration) {
                Ok(secs) => match core.player.seek(secs, false) {
                    Ok(()) => op_note(format!("moved to {secs:.0}s")),
                    Err(e) => op_fail(e.to_string()),
                },
                Err(why) => op_fail(why),
            }
        }
        Op::SeekRelative(delta) => op_ipc(core.player.seek(delta, true)),
        Op::SetVolume(level) => {
            let current = current_volume(core.player).unwrap_or(*core.base_volume);
            let target = shellsong_host::volume_target(level, current);
            match core.player.set_volume(target) {
                // the new level becomes the ceiling the crossfade and
                // fade ramps scale against, or the next fade-in would
                // snap the volume back to whatever it was at startup.
                Ok(()) => {
                    *core.base_volume = target;
                    op_note(format!("volume: {target:.0}%"))
                }
                Err(e) => op_fail(format!("error setting volume: {e}")),
            }
        }
        // mpv's own `mute`, not volume 0: it is reversible without
        // having to remember the level, and `volume(...)` stays the one
        // thing that moves the level.
        Op::Mute(on) => op_ipc(core.player.set_property("mute", serde_json::json!(on))),
        Op::CycleLoop => {
            let mode = core.app.cycle_loop();
            let dispatch = dispatch_action(Action::SetLoop(mode), core, surf);
            (dispatch, Report::Note(format!("loop: {}", mode.label())))
        }
        Op::SetLoop(mode) => {
            core.app.loop_mode = mode;
            let dispatch = dispatch_action(Action::SetLoop(mode), core, surf);
            (dispatch, Report::Described)
        }
        Op::ToggleShuffle => {
            let on = core.app.toggle_shuffle();
            let dispatch = dispatch_action(Action::ShuffleToggled(on), core, surf);
            (
                dispatch,
                Report::Note(format!("shuffle: {}", if on { "on" } else { "off" })),
            )
        }
        Op::SetShuffle(want) => {
            if core.app.shuffle == want {
                return op_note(format!(
                    "shuffle is already {}",
                    if want { "on" } else { "off" }
                ));
            }
            let on = core.app.toggle_shuffle();
            let dispatch = dispatch_action(Action::ShuffleToggled(on), core, surf);
            (dispatch, Report::Described)
        }
        Op::SetSpeed(speed) => {
            core.app.speed = speed;
            let dispatch = dispatch_action(Action::SetSpeed(speed), core, surf);
            (dispatch, Report::Described)
        }
        Op::SpeedUp | Op::SpeedDown => {
            let dir = if matches!(op, Op::SpeedUp) { 1.0 } else { -1.0 };
            let speed = core.app.bump_speed(dir);
            let dispatch = dispatch_action(Action::SetSpeed(speed), core, surf);
            (dispatch, Report::Note(format!("speed: {speed}x")))
        }
        Op::SetEqPreset(name) => {
            // resolved here rather than at lowering time, because the
            // saved curves only exist at runtime; `eq::resolve` reads
            // built-ins first, then the saved list, same precedence the
            // eq menu shows them in.
            let saved = &core.cfg.eq_custom_presets;
            let Some((source, _)) = eq::resolve(&name, saved) else {
                return op_fail(eq::resolve_failure(&name, saved));
            };
            // `apply_eq_row` speaks eq-menu rows, which is exactly
            // built-ins then saved; `eq_menu::cursor` is the inverse of
            // this and a test pins the two agreeing.
            let row = match source {
                eq::Source::Builtin(i) => i,
                eq::Source::Saved(i) => ui::eq_menu::PRESET_COUNT + i,
            };
            apply_eq_row(core.app, core.player, core.notifications, saved, row);
            op_done()
        }
        Op::SetGap(secs) => {
            *core.gap_secs = secs.unwrap_or(0.0).max(0.0);
            if *core.gap_secs == 0.0 {
                // a gap being waited out right now is dropped rather
                // than left to expire: "off" should mean off, not
                // "off after this one".
                core.between_tracks.cancel();
                let _ = core.player.set_pause(false);
            }
            match secs {
                Some(s) if *core.gap_secs > 0.0 => op_note(format!("{s}s between tracks")),
                _ => op_note("no gap between tracks".to_string()),
            }
        }
        Op::ToggleCrossfade => {
            *core.crossfade_enabled = !*core.crossfade_enabled;
            if !*core.crossfade_enabled {
                *core.fade_in_started_at = None;
                let _ = core.player.set_volume(*core.base_volume);
            }
            surf.flash(Field::Crossfade);
            op_note(format!(
                "crossfade: {}",
                if *core.crossfade_enabled { "on" } else { "off" }
            ))
        }
        Op::Search(query) => {
            core.app.set_query(&query);
            let fired = notify_query_changed(core.app, core.remote_search_tx);
            core.settle.notified(fired, core.now);
            surf.show_results();
            op_note(format!(
                "search: {query} ({} results so far)",
                core.app.results.len()
            ))
        }
        Op::MoveSelection(delta) => surf.move_selection(SelectStep::By(delta), core),
        Op::SelectTop => surf.move_selection(SelectStep::Top, core),
        Op::SelectBottom => surf.move_selection(SelectStep::Bottom, core),
        Op::Go(GoTarget::Result) => {
            let Some(item) = surf.selection(core) else {
                return op_fail("nothing is selected");
            };
            let name = label(&item);
            let dispatch = dispatch_action(Action::Play(item), core, surf);
            (dispatch, Report::Note(format!("enter on {name}")))
        }
        Op::Go(GoTarget::BrowseRow) => surf.go_browse_row(core),
        Op::Go(GoTarget::BottomBarField) => {
            // enter on the bottom bar tweaks whichever field is
            // selected, and every one of those fields already has its
            // own command; reuse them rather than keeping a second
            // copy of the bar's own match.
            let Some(field) = surf.bar_field() else {
                return surf.decline("bar control to press");
            };
            let Some(field_op) = op_for_field(field) else {
                // enter on the scrubber opens a keyboard drag, which
                // the open console would swallow every key of; the
                // language has real seek commands instead.
                return op_fail("use moveto(1m20s) or seek(+30s) to scrub from here");
            };
            let closes = field_op.closes_console();
            let result = run_op(field_op, core, surf);
            if closes && !matches!(result.1, Report::Failed(_)) {
                surf.close_console();
            }
            result
        }
        Op::Play(target) => {
            // `go()`'s browse-row reading, for the same reason: with a
            // browse view focused, the selection *is* a row of it, and
            // playing one plays through to the end of the listing.
            if target == Target::Selection
                && surf.focus() == shellsong::Focus::Workspace
                && surf.browsing()
            {
                return run_op(Op::Go(GoTarget::BrowseRow), core, surf);
            }
            let Some(item) = shellsong_item(target, core, surf) else {
                return op_fail("nothing is selected");
            };
            // a page is opened, not played, and opening one needs a
            // pane the core does not have; asked through the surface
            // so a front-end without panes declines by name.
            if matches!(
                item,
                SearchItem::Album { .. } | SearchItem::Playlist { .. } | SearchItem::Artist { .. }
            ) {
                return surf.open_browse(&item, core);
            }
            let name = label(&item);
            let dispatch = dispatch_action(Action::Play(item), core, surf);
            (dispatch, Report::Note(format!("playing: {name}")))
        }
        Op::Enqueue { target, front } => {
            let Some(item) = shellsong_item(target, core, surf) else {
                return op_fail("nothing is selected");
            };
            if !front {
                // `Action::Queued` expands an album/playlist itself and
                // reports its own count; resolving here as well would
                // cost a second fetch of the very same track list.
                let dispatch = dispatch_action(Action::Queued(item), core, surf);
                return (dispatch, Report::Described);
            }
            match core.track_resolver.request(&item, Intent::Front) {
                Resolution::Ready(tracks) if tracks.is_empty() => {
                    op_fail("nothing to queue (empty album or playlist)")
                }
                Resolution::Ready(tracks) => match front_resolved(&tracks, core) {
                    Ok(n) => op_note(format!("queued {n} tracks next")),
                    Err(why) => op_fail(why),
                },
                Resolution::Fetching(label) => op_note(format!("queueing {label} next...")),
            }
        }
        Op::Star { target, on } => {
            let Some(item) = shellsong_item(target, core, surf) else {
                return op_fail("nothing is selected");
            };
            let verb = if on { "starred" } else { "unstarred" };
            if is_starred(&item, core.favorites, core.remote_starred) == on {
                return op_note(format!("'{}' is already {verb}", item.primary_name()));
            }
            match toggle_star(&item, core.favorites, core.remote_starred, core.star_tx) {
                Ok(_) => op_note(format!("{verb} '{}'", item.primary_name())),
                Err(why) => op_fail(why),
            }
        }
        Op::Download(target) => {
            let Some(item) = shellsong_item(target, core, surf) else {
                return op_fail("nothing is selected");
            };
            // an album or artist is a fan-out to expand, so it is
            // resolved off-thread like queueing one; a single song
            // needs no request and starts in this op.
            match core.track_resolver.request(&item, Intent::Download) {
                Resolution::Ready(tracks) => op_note(download_resolved(&tracks, core).status),
                Resolution::Fetching(label) => op_note(format!("resolving {label}...")),
            }
        }
        Op::OpenPage(target) => {
            let Some(item) = shellsong_item(target, core, surf) else {
                return op_fail("nothing is selected");
            };
            // an album/artist/playlist *is* a page, and
            // `Action::Play` on one opens it rather than playing it; a
            // song has no page of its own, so the page it belongs to
            // is its album's. A local file has neither.
            let page = match &item {
                SearchItem::Album { .. }
                | SearchItem::Artist { .. }
                | SearchItem::Playlist { .. } => Some(item.clone()),
                SearchItem::Song { album, client, .. } => {
                    album_page(&core.app.corpus, album, client)
                }
                SearchItem::LocalMusic { .. }
                | SearchItem::LocalVideo { .. }
                | SearchItem::Silence { .. } => None,
            };
            match page {
                Some(page) => {
                    let name = label(&page);
                    let dispatch = dispatch_action(Action::Play(page), core, surf);
                    (dispatch, Report::Note(format!("opened {name}")))
                }
                None => op_fail(format!("no page for '{}'", item.primary_name())),
            }
        }
        Op::ClearConsole => {
            // the report line is pushed after this returns, so the
            // console ends up holding exactly one line saying what
            // happened rather than looking like it crashed.
            core.console.lines.clear();
            op_done()
        }
        Op::ClearQueue => {
            // mpv's own `playlist-clear` keeps the file that is
            // playing and drops the rest, which is exactly what
            // clearing the queue means; `entries` only mirrors it.
            match core.player.playlist_clear() {
                Ok(()) => {
                    // or a batch in flight refills the queue the user
                    // just emptied, seconds after they emptied it.
                    core.track_resolver.invalidate();
                    core.queue.replace_upcoming(Vec::new());
                    if let Some(cursor) = surf.queue_cursor() {
                        *cursor = 0;
                    }
                    op_done()
                }
                Err(e) => op_fail(format!("error clearing the queue: {e}")),
            }
        }
        Op::RemoveFromQueue(positions) => {
            // 1-based and descending, so removing one can't reindex
            // the next; the same `remove` + `playlist_remove` pairing
            // the queue view's 'd' uses.
            let mut removed = 0;
            for position in &positions {
                let index = position.saturating_sub(1);
                if core.queue.remove(index) {
                    if let Err(e) = core.player.playlist_remove(index) {
                        return op_fail(format!("error removing: {e}"));
                    }
                    removed += 1;
                }
            }
            let left = core.queue.entries().len();
            if let Some(cursor) = surf.queue_cursor() {
                *cursor = (*cursor).min(left.saturating_sub(1));
            }
            op_note(format!("removed {removed} from the queue"))
        }
        Op::GotoQueue(position) => {
            // a `playlist-pos` write, not a reload: mpv keeps the same
            // playlist so `entries` stays index-aligned with it (same
            // as enter in the queue view).
            op_ipc(core.player.set_property(
                "playlist-pos",
                serde_json::json!(position.saturating_sub(1)),
            ))
        }
        Op::Silence { secs, front } => {
            // no network and no expansion: a silence is already its
            // own one-entry track list, so it goes straight in.
            let item = SearchItem::Silence { secs };
            let span = crate::ui::progress::fmt_span(secs);
            if front {
                match front_resolved(&[item], core) {
                    Ok(_) => op_note(format!("a {span} silence plays next")),
                    Err(why) => op_fail(why),
                }
            } else {
                // appended, never shuffled into a random slot even
                // with shuffle on: a silence is positional by nature,
                // and the one thing worse than a gap in the wrong
                // place is a gap somewhere nobody chose.
                let status = enqueue_track(core.player, &item, core.cache, core.queue, false);
                if status.starts_with("error") {
                    op_fail(status)
                } else {
                    op_note(format!("queued a {span} silence"))
                }
            }
        }
        Op::Undo | Op::Redo => {
            let back = op == Op::Undo;
            let outcome = if back {
                core.undo.undo()
            } else {
                core.undo.redo()
            };
            match outcome {
                undo::Outcome::Restored(snapshot) => match snapshot.restore(core) {
                    Ok(()) => op_done(),
                    Err(why) => op_fail(why),
                },
                // the whole point of barriers: say what cannot be
                // reversed rather than restoring around it and letting
                // the user believe it worked.
                undo::Outcome::Refused(why) => op_fail(format!("can't undo: {why}")),
                undo::Outcome::Nowhere => op_fail(if back {
                    "nothing to undo"
                } else {
                    "nothing to redo"
                }),
            }
        }
        Op::ShuffleQueue => {
            let shuffled = core.queue.shuffled_upcoming();
            let count = shuffled.len();
            let status = resync_upcoming(core.player, core.cache, core.queue, shuffled);
            if status.starts_with("error") {
                return op_fail(status);
            }
            op_note(format!("shuffled {count} upcoming"))
        }
        Op::SaveQueue(name) => match queues::save(&name, core.queue.entries()) {
            Ok(0) => op_fail("nothing in the queue to save"),
            Ok(count) => {
                *core.saved_queues = queues::list();
                op_note(format!("saved {count} tracks as '{}'", queues::slug(&name)))
            }
            Err(e) => op_fail(format!("couldn't save '{name}': {e}")),
        },
        Op::LoadQueue(name) => {
            let clients = shellsong_host::clients_of(&core.app.corpus);
            // `load_report` rather than `load`: the plain one discards
            // what it could not rebuild, so a fifty-track queue whose
            // server is gone came back as twelve tracks and said
            // nothing about the other thirty-eight.
            match queues::load_report(&name, &clients) {
                None => op_fail(namehint::unknown(
                    "saved queue",
                    &name,
                    queues::list().iter().map(String::as_str),
                )),
                Some(loaded) if loaded.items.is_empty() => {
                    let why = queues::summarise(&loaded.dropped);
                    if why.is_empty() {
                        op_fail(format!("'{name}' is empty"))
                    } else {
                        op_fail(format!("nothing in '{name}' loaded: {why}"))
                    }
                }
                Some(loaded) => {
                    let count = loaded.items.len();
                    let lost = loaded.dropped.len();
                    match start_session(loaded.items, false, core, surf) {
                        Ok(_) if lost == 0 => op_note(format!("loaded '{name}': {count} tracks")),
                        Ok(_) => op_note(format!(
                            "loaded '{name}': {count} of {} tracks; {} missing: {}",
                            count + lost,
                            lost,
                            queues::summarise(&loaded.dropped)
                        )),
                        Err(why) => op_fail(why),
                    }
                }
            }
        }
        // the terms arrive already expanded: a variable used inside
        // another is resolved while parsing, so this only ever stores
        // what it was handed.
        Op::Define { name, terms } => {
            let count = terms.len();
            let replaced = core.vars.define(&name, terms);
            shellsong_host::save_vars(core.vars);
            let verb = if replaced { "redefined" } else { "defined" };
            op_note(format!("{verb} {name}: {count} terms"))
        }
        Op::Unset(name) => {
            if !core.vars.remove(&name) {
                return op_fail(namehint::unknown("variable", &name, core.vars.names()));
            }
            shellsong_host::save_vars(core.vars);
            op_note(format!("unset {name}"))
        }
        Op::Remember { tier, setting } => {
            let Some(item) = core.now_playing.as_ref() else {
                return op_fail("nothing is playing to remember it for");
            };
            let id = shellsong_host::identity_of(item);
            let Some(name) = shellsong_host::name_at(&id, tier) else {
                return op_fail(format!(
                    "this track has no {} to hang a setting off",
                    tier.label()
                ));
            };
            let report = format!(
                "{} {} remembered for {} '{name}'",
                setting.kind().label(),
                setting.value(),
                tier.label()
            );
            core.bindings.remember(tier, &name, &setting);
            shellsong_host::save_bindings(core.bindings);
            // applied now as well as remembered, through the same arm
            // the typed command goes through, so remembering a setting
            // and setting it cannot drift.
            let (dispatch, _) = run_op(setting.op(), core, surf);
            (dispatch, Report::Note(report))
        }
        Op::Forget { tier, setting } => {
            let Some(item) = core.now_playing.as_ref() else {
                return op_fail("nothing is playing to forget anything for");
            };
            let id = shellsong_host::identity_of(item);
            let Some(name) = shellsong_host::name_at(&id, tier) else {
                return op_fail(format!("this track has no {}", tier.label()));
            };
            if !core.bindings.forget(tier, &name, setting) {
                return op_fail(match setting {
                    Some(kind) => format!(
                        "no {} was remembered for {} '{name}'",
                        kind.label(),
                        tier.label()
                    ),
                    None => format!("nothing was remembered for {} '{name}'", tier.label()),
                });
            }
            shellsong_host::save_bindings(core.bindings);
            op_note(match setting {
                Some(kind) => format!("forgot {} for {} '{name}'", kind.label(), tier.label()),
                None => format!("forgot everything for {} '{name}'", tier.label()),
            })
        }
        // the partition is the language's own: `is_screen()` targets
        // need a pane, the other five print into the console and so
        // behave identically in both binaries.
        Op::View(target) if target.is_screen() => surf.open_screen(target, core),
        Op::View(target) => print_listing(target, core),
        Op::Rescan => {
            // a fresh channel and thread, same as startup: the scan
            // signs off by dropping its sender, so the old receiver
            // can't be reused. Only the local half of the corpus is
            // dropped, or a rescan would also wipe the navidrome
            // catalog until the next sync landed.
            let (tx, rx) = mpsc::channel::<library::ScanBatch>();
            library::spawn_scan(core.cfg.library_paths.clone(), tx);
            *core.scan_rx = rx;
            core.app.corpus.drop_local();
            core.app.scanning_library = true;
            core.app.resync_results();
            op_done()
        }
        Op::Sleep(Some(secs)) => {
            *core.sleep_due = Some(core.now + Duration::from_secs_f64(secs.max(0.0)));
            op_done()
        }
        Op::Sleep(None) => {
            let was_set = core.sleep_due.take().is_some();
            if !was_set {
                return op_note("no sleep timer was set");
            }
            // the ramp may already have taken the volume down.
            let _ = core.player.set_volume(*core.base_volume);
            op_done()
        }
        Op::Fade { into, secs } => {
            let from = current_volume(core.player).unwrap_or(*core.base_volume);
            let to = if into { *core.base_volume } else { 0.0 };
            *core.fade = Some(Ramp::new(
                from,
                to,
                core.now,
                Duration::from_secs_f64(secs.max(0.0)),
            ));
            op_done()
        }
        Op::Quit => {
            let dispatch = dispatch_action(Action::Quit, core, surf);
            (dispatch, Report::Described)
        }
    }
}
/// The five `view(...)` targets that are not screens: they print into
/// the console and need no pane, so both front-ends get them for free
/// and identically. `ViewTarget::is_screen()` is the partition.
pub fn print_listing(target: ViewTarget, core: &mut CoreCtx) -> (Dispatch, Report) {
    match target {
        ViewTarget::Queue => {
            let pos = core.queue.pos();
            let lines: Vec<String> = core
                .queue
                .entries()
                .iter()
                .enumerate()
                .map(|(i, item)| {
                    let here = if Some(i) == pos { '>' } else { ' ' };
                    format!("{here}{:>3}. {}", i + 1, label(item))
                })
                .collect();
            if lines.is_empty() {
                return op_note("the queue is empty");
            }
            let count = lines.len();
            for line in lines {
                core.console.note(line);
            }
            op_note(format!("{count} in the queue"))
        }
        ViewTarget::Index => {
            let rows = core.app.corpus.breakdown();
            for (source, count) in &rows {
                core.console.note(format!("{source}: {count}"));
            }
            op_note(format!("{} sources indexed", rows.len()))
        }
        ViewTarget::History => {
            // cloned first: the scrollback is being pushed into, and
            // the history lives on the same `Console`.
            let lines: Vec<String> = core.console.history().to_vec();
            if lines.is_empty() {
                return op_note("no history yet");
            }
            let count = lines.len();
            for line in lines {
                core.console.note(format!(": {line}"));
            }
            op_note(format!("{count} past commands"))
        }
        ViewTarget::Playlists => {
            *core.saved_queues = queues::list();
            if core.saved_queues.is_empty() {
                return op_note("no saved queues; save_queue(name) makes one");
            }
            let names = core.saved_queues.clone();
            let count = names.len();
            for name in names {
                core.console.note(name);
            }
            op_note(format!("{count} saved queues"))
        }
        ViewTarget::Bindings => {
            let lines = core.bindings.lines();
            let count = if core.bindings.is_empty() {
                0
            } else {
                lines.len()
            };
            for line in lines {
                core.console.note(line);
            }
            op_note(format!("{count} remembered"))
        }
        // the language generates these lines too, as `let` statements,
        // so the listing is also valid input to paste into a `.shg`.
        ViewTarget::Vars => {
            let lines = core.vars.lines();
            let count = if core.vars.is_empty() { 0 } else { lines.len() };
            for line in lines {
                core.console.note(line);
            }
            op_note(format!("{count} variables"))
        }
        // the one listing whose content the language generates rather
        // than the app, which is what keeps `help()` and `view(help)`
        // from drifting; printing it is still the host's job.
        ViewTarget::Help => {
            let lines = shellsong::help_lines();
            let count = lines.len();
            for line in lines {
                core.console.note(line);
            }
            op_note(format!("{count} commands"))
        }
        // `is_screen()` sends these to the surface; reached only if
        // that partition and this match ever disagree.
        other => (
            Dispatch::Continue,
            Report::Failed(format!("{other:?} is a screen, not a listing")),
        ),
    }
}

/// What one frame of driving a program came to. The loop never
/// blocks: `Await` means the results a `Resolve` wants have not
/// settled, so the caller keeps the program and tries again next
/// frame (see `Program::step`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drive {
    /// The program is finished; drop it.
    Done,
    /// Suspended on a search; keep it and step again next frame.
    Await,
    /// `quit()` ran; the front-end should exit.
    Quit,
}

/// Steps a program as far as it can go this frame, applying each
/// `Emit` before the next `step`. The one driver, so wim and
/// shellswim cannot diverge on await handling, on when a program is
/// dropped, or on where a failure is reported.
pub fn drive(
    program: &mut Program,
    core: &mut CoreCtx,
    surf: &mut dyn Surface,
    settle_now: impl Fn(&CoreCtx) -> bool,
) -> Drive {
    loop {
        // rebuilt every step, not once per frame: `search(...)` makes
        // the query unsettled again, and a `Resolve` reading a stale
        // `true` would act on the previous query's results, which is
        // the exact bug `search_settled` exists to stop.
        let settled = settle_now(core);
        let env = shellsong_env(core.app, core.queue, surf, settled, core.now);
        match program.step(&env) {
            shellsong::Step::Emit(op) => {
                if apply_op(op, core, surf) == Dispatch::Quit {
                    return Drive::Quit;
                }
                if program.is_done() {
                    return Drive::Done;
                }
            }
            shellsong::Step::Note(text) => core.console.note(text),
            // a non-fatal complaint: reported and the program keeps
            // going, where `Failed` stops it.
            shellsong::Step::Warn(text) => core.console.fail(text),
            shellsong::Step::AwaitResults { .. } => return Drive::Await,
            shellsong::Step::Failed(e) => {
                core.console.fail(e.to_string());
                return Drive::Done;
            }
            shellsong::Step::Done => return Drive::Done,
        }
    }
}

/// What `drained` did about mpv running out of playlist.
#[derive(Debug)]
pub enum Drained {
    /// A stale report: something is still queued, so mpv's idle
    /// predates whatever just loaded. Nothing to do.
    Stale,
    /// The queue ran out and was cleared; the caller drops its
    /// now-playing.
    Cleared,
    /// "loop all" restarted the session from the top. The caller runs
    /// its own track-change hooks for this track, which is the one
    /// part the two front-ends genuinely do differently.
    Restarted(SearchItem),
    /// The restart could not load; the caller reports it.
    Failed(String),
}

/// mpv went back to idling, so its playlist really did run out.
///
/// Shared because it is a *decision* about the queue, not a piece of
/// either front-end: "loop all" replaying from the top is wim's own
/// bookkeeping (`PlayQueue::session`), mpv having no concept of the
/// session that was built. shellswim did not handle mpv going idle at
/// all, so a queue that ran out left the bar showing a track that had
/// finished; it gets the same answer as wim now rather than a second
/// copy of it.
///
/// The upcoming-empty check is here rather than at the call sites: it
/// guards against acting on an idle report from *before* something
/// loaded, and a guard every caller has to remember is a guard one
/// caller will forget.
/// Takes its four pieces rather than a `CoreCtx`, for two reasons:
/// wim's call site sits above where its `core_ctx!` is defined, and
/// the whole decision is testable this way without spawning mpv for
/// the half of it that never touches the socket.
pub fn drained(
    player: &mpv::Mpv,
    cache: &Arc<Mutex<CacheIndex>>,
    queue: &mut PlayQueue,
    loop_mode: LoopMode,
) -> Drained {
    match drain_decision(queue.upcoming_len(), loop_mode, queue.session().is_empty()) {
        Decision::Stale => return Drained::Stale,
        Decision::Clear => {
            queue.clear();
            return Drained::Cleared;
        }
        Decision::Restart => {}
    }
    let session = queue.session().to_vec();
    {
        let first = session[0].clone();
        let (status, played) = load_playlist(
            player,
            cache,
            queue,
            session,
            // `Keep`, not `New`: the restart point is the session
            // itself, and resetting it would make loop-all replay only
            // from wherever it last restarted.
            SessionStart::Keep,
        );
        if !played {
            return Drained::Failed(status);
        }
        Drained::Restarted(first)
    }
}

/// What to do about an idle report, with no mpv in the room.
///
/// Split out so the decision is testable without a socket: the only
/// part of `drained` that needs mpv is reloading the playlist for a
/// restart, and that is one call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decision {
    Stale,
    Clear,
    Restart,
}

fn drain_decision(upcoming: usize, loop_mode: LoopMode, session_empty: bool) -> Decision {
    if upcoming > 0 {
        // an idle report from before whatever just loaded. acting on
        // it would clear a queue that is about to play.
        return Decision::Stale;
    }
    if loop_mode == LoopMode::All && !session_empty {
        Decision::Restart
    } else {
        Decision::Clear
    }
}

/// Starts a freshly built session: mpv's playlist is replaced and
/// every "a track just started" hook fires for the first track. One
/// place, so a shellsong-built session can't quietly skip a hook the
/// keyboard runs.
///
/// This doc used to say "the same set `Action::Play` fires", which was
/// aspirational: `Action::Play` kept its own copy of the body and the
/// two were free to drift. It calls this now.
pub fn start_session(
    items: Vec<SearchItem>,
    minimize: bool,
    core: &mut CoreCtx,
    surf: &mut dyn Surface,
) -> Result<String, String> {
    let Some(first) = items.first().cloned() else {
        return Err("nothing to play".to_string());
    };
    // whatever was being resolved for the old queue has nowhere to go
    // now; without this a slow artist fan-out would land in a session
    // started after it was asked for.
    core.track_resolver.invalidate();
    let interrupted = core.queue.current().cloned();
    let (status, played) = load_playlist(
        core.player,
        core.cache,
        core.queue,
        items,
        SessionStart::New,
    );
    if !played {
        return Err(status);
    }
    fire_subtitle_fetch(core.cfg, core.subtitle_tx, &first);
    let mut dctx = DownloadCtx {
        cache: core.cache,
        download_tx: core.download_tx,
        downloading: core.downloading,
        day_threshold: core.cfg.cache_days_threshold,
    };
    note_playback_started(&first, &mut dctx, core.notifications);
    start_tracking_playback(core.cfg, &first, core.scrobble);
    *core.previous_track = interrupted;
    surf.minimize_player(minimize);
    Ok(status)
}
/// `add_to_queue(front, ...)`: append, then move the new entry in
/// right behind what is playing. `enqueue_track` still writes both
/// sides (mpv's playlist and the index map); this only reorders, with
/// the same `move_entry` + `playlist_move` pairing the queue view's
/// reorder uses. Appended unshuffled on purpose: `front` means front.
pub fn enqueue_next(item: &SearchItem, core: &mut CoreCtx) -> Result<(), String> {
    let status = enqueue_track(core.player, item, core.cache, core.queue, false);
    if status.starts_with("error") || status.starts_with("not ") {
        return Err(status);
    }
    let from = core.queue.entries().len().saturating_sub(1);
    let to = core.queue.pos().map_or(0, |p| p + 1);
    if to < from && core.queue.move_entry(from, to) {
        if let Err(e) = core.player.playlist_move(from, to) {
            return Err(format!("error moving: {e}"));
        }
    }
    Ok(())
}
/// Pushes the current query to the background `search3` thread (see
/// `spawn_background_search`) whenever typing/pasting actually changed
/// it; a no-op while the query's empty or no server has been indexed
/// yet, same as the background thread's own guard, just avoiding the
/// channel send entirely in the common "nothing to search remotely"
/// case.
///
/// Reports whether a request actually went out, which is exactly what
/// `SearchSettle` needs: with nothing fired there is nothing to wait
/// for, so the results for the new query are already as complete as
/// they will get.
pub fn notify_query_changed(
    app: &app::App,
    remote_search_tx: &mpsc::Sender<RemoteSearchQuery>,
) -> bool {
    if app.query.trim().is_empty() {
        return false;
    }
    let clients = app.corpus.remote_clients();
    if clients.is_empty() {
        return false;
    }
    remote_search_tx
        .send(RemoteSearchQuery {
            query: app.query.clone(),
            clients,
        })
        .is_ok()
}
/// The `?` list: every command name, packed into rows. Derived from
/// `shellsong::help_lines()` rather than a second table, so a command
/// added to the language appears here without anyone remembering to.
/// `help()` is the full version with signatures; this is the one a new
/// user needs before they know there is a `help()`.
pub fn short_help() -> Vec<String> {
    let mut names: Vec<String> = shellsong::help_lines()
        .iter()
        .filter_map(|line| {
            let sig = line.trim_start();
            let name = sig.split(['(', ' ']).next()?;
            (!name.is_empty()).then(|| name.to_string())
        })
        .collect();
    names.dedup();
    let mut out = vec![format!("{} commands:", names.len())];
    out.extend(pack(&names, HELP_COLUMNS));
    out.push("help() for the arguments, dry(cmd) to see without doing".to_string());
    out
}

/// Rows of at most `budget` cells, words separated by two spaces.
const HELP_COLUMNS: usize = 66;

fn pack(words: &[String], budget: usize) -> Vec<String> {
    let mut rows: Vec<String> = Vec::new();
    let mut row = String::new();
    for w in words {
        let want = if row.is_empty() {
            w.chars().count()
        } else {
            row.chars().count() + 2 + w.chars().count()
        };
        if want > budget && !row.is_empty() {
            rows.push(std::mem::take(&mut row));
        }
        if !row.is_empty() {
            row.push_str("  ");
        }
        row.push_str(w);
    }
    if !row.is_empty() {
        rows.push(row);
    }
    rows
}

/// The name of the live eq curve, which the per-identity settings
/// restore to. A hand-tuned curve has no name, so the configured preset
/// stands in; that is better than leaving one track's curve applied for
/// the rest of the session.
pub fn current_eq_name(app: &app::App, cfg: &config::Config) -> String {
    app.eq_source
        .and_then(|s| eq::source_name(s, &cfg.eq_custom_presets))
        .map(str::to_string)
        .or_else(|| cfg.eq_preset.clone())
        .unwrap_or_else(|| eq::PRESETS[0].name.to_string())
}

/// What the per-identity settings want for a track that just started:
/// the ops to apply, and the one line to toast when the sound actually
/// changed because of them.
///
/// The restore is the load-bearing half, and it lives in
/// `shellsong_host::settle`: a setting the new track does not bind goes
/// back to the value the user had chosen before any binding took over,
/// rather than keeping the last bound one (which is what leaves
/// everything quiet after one quiet song).
pub fn identity_settings(
    item: &SearchItem,
    bindings: &shellsong_host::Bindings,
    overridden: &mut shellsong_host::Overridden,
    eq_now: String,
    volume_now: f64,
    speed_now: f64,
) -> (Vec<Op>, Option<String>) {
    let wanted = bindings.resolve(&shellsong_host::identity_of(item));
    let mut ops = Vec::new();
    if let Some(name) = shellsong_host::settle(&mut overridden.eq, wanted.eq.clone(), eq_now) {
        ops.push(Op::SetEqPreset(name));
    }
    if let Some(v) = shellsong_host::settle(&mut overridden.volume, wanted.volume, volume_now) {
        ops.push(Op::SetVolume(shellsong::Level::Absolute(v)));
    }
    if let Some(s) = shellsong_host::settle(&mut overridden.speed, wanted.speed, speed_now) {
        ops.push(Op::SetSpeed(s));
    }
    // only when something moved, so a bound album says it once on its
    // first track rather than on all twelve.
    let note = (!ops.is_empty()).then(|| match wanted.summary() {
        summary if summary.is_empty() => "settings restored".to_string(),
        summary => format!("remembered: {summary}"),
    });
    (ops, note)
}

// MOVED_ITEMS_MARKER

/// What the core could not do by itself. Every method has a default
/// that declines with a one-line reason, so a front-end opts in to
/// exactly what it can honour and a new method cannot silently break
/// one. wim honours nearly all of it; shellswim honours what a shell
/// can show and declines the rest by name.
///
/// Passed alongside `CoreCtx` as a second, disjoint borrow: the
/// surface holds the front-end's pane locals, `CoreCtx` holds the
/// core's. See docs/library-seam.md for why this is a trait and not a
/// queue of requests.
pub trait Surface {
    /// Named in every decline, so the message says where the thing it
    /// is refusing does exist.
    fn front_end(&self) -> &'static str;

    /// `Target::Selection`: what is highlighted. wim resolves it the
    /// way its star/download keys do; a front-end with no highlight
    /// should answer with the head of the current result set, which is
    /// where wim's highlight starts.
    fn selection(&self, core: &CoreCtx) -> Option<SearchItem> {
        core.app.results.first().cloned()
    }

    /// `Focus` as the interpreter sees it; `go()` resolves against it.
    fn focus(&self) -> shellsong::Focus {
        shellsong::Focus::Sidebar
    }

    /// Whether a browse view is open, which widens what counts as a
    /// selection and which `go()` target is meant.
    fn browsing(&self) -> bool {
        false
    }

    /// One of the `ViewTarget::is_screen()` targets. The five that are
    /// not screens print into the console and never reach here.
    fn open_screen(&mut self, target: ViewTarget, core: &mut CoreCtx) -> (Dispatch, Report) {
        let _ = core;
        self.decline(&format!("view({})", format!("{target:?}").to_lowercase()))
    }

    fn move_selection(&mut self, step: SelectStep, core: &mut CoreCtx) -> (Dispatch, Report) {
        let _ = (step, core);
        self.decline("selection to move")
    }

    /// `go()` with a browse view open: play from the highlighted row.
    fn go_browse_row(&mut self, core: &mut CoreCtx) -> (Dispatch, Report) {
        let _ = core;
        self.decline("browse row to play from")
    }

    /// An album/playlist/artist opens its track listing rather than
    /// playing; `Action::Play` on one lands here.
    fn open_browse(&mut self, item: &SearchItem, core: &mut CoreCtx) -> (Dispatch, Report) {
        let _ = core;
        self.decline(&format!("page to open {} in", label(item)))
    }

    /// `search()` ran and this front-end has nowhere to draw the
    /// results. wim ignores it: its sidebar *is* the answer. shellswim
    /// prints them once they settle, because a bare count was the
    /// whole answer there and a count tells you nothing.
    ///
    /// Asked for here rather than inferred by the host, which cannot
    /// tell `search(x)` from the query a searching `play(x)` sets on
    /// its way to picking a match, and would echo a list after both.
    fn show_results(&mut self) {}

    /// Which bottom-bar control `go()` would press. `None` means
    /// nothing on the bar is focusable here.
    fn bar_field(&self) -> Option<Field> {
        None
    }

    /// Light a bar control for a moment. Pure feedback; a front-end
    /// with no such affordance ignores it rather than declining,
    /// because the op itself did happen.
    fn flash(&mut self, field: Field) {
        let _ = field;
    }

    /// Whether a standalone play takes over the full player pane.
    fn minimize_player(&mut self, minimize: bool) {
        let _ = minimize;
    }

    /// The queue view's cursor, for the two ops that have to reclamp
    /// it after shortening the queue under it.
    fn queue_cursor(&mut self) -> Option<&mut usize> {
        None
    }

    /// Dropped when an op takes over the screen; see
    /// `Op::closes_console`.
    fn close_console(&mut self) {}

    /// The one-line refusal. Shared so every decline reads the same
    /// and always names where the thing does exist.
    fn decline(&self, what: &str) -> (Dispatch, Report) {
        op_fail(format!("{} has no {what}; wim does", self.front_end()))
    }
}

/// Declines everything, which is what makes it the proof that the op
/// table needs no pane state: `core::tests` applies ops against this.
pub struct Headless;

impl Surface for Headless {
    fn front_end(&self) -> &'static str {
        "this front-end"
    }
}

#[cfg(test)]
mod seam_tests {
    use super::*;
    use crate::browse::fixtures::test_songs;

    /// The whole claim of the library split, as a test: an op applies
    /// with `Headless` in the room, which is a unit struct. There is
    /// no `Focus`, no browse view, no menu flag and no selection
    /// cursor anywhere in these calls, and the ops still work and
    /// still report one line each.
    ///
    /// `#[ignore]` because `CoreCtx` holds a live mpv handle and
    /// spawning one is ~400ms; the repo's convention for a test that
    /// needs mpv (see `mpv::tests`). Run it with
    /// `cargo test --lib seam -- --ignored`.
    #[test]
    #[ignore]
    fn ops_apply_with_no_ui_state_at_all() {
        let player = mpv::Mpv::spawn().expect("mpv on PATH");
        // measured on a desktop that may have a real wim playing:
        // exercise the control path, do not touch the speakers.
        let _ = player.set_property("ao", serde_json::json!("null"));
        let mut h = Harness::new();
        let mut surf = Headless;

        // a transport op: reaches mpv, reports one line, no panes.
        let (dispatch, report) = {
            let mut core = h.ctx(&player);
            (
                apply_op(
                    Op::SetVolume(shellsong::Level::Absolute(55.0)),
                    &mut core,
                    &mut surf,
                ),
                core.console.lines.last().map(|l| l.text.clone()),
            )
        };
        assert_eq!(dispatch, Dispatch::Continue);
        assert_eq!(report.as_deref(), Some("volume: 55%"));

        // a console-only listing: handled entirely in the core, so it
        // is identical in both front-ends.
        {
            let mut core = h.ctx(&player);
            apply_op(Op::View(ViewTarget::Index), &mut core, &mut surf);
        }
        assert!(
            h.console.lines.iter().any(|l| l.text.contains("indexed")),
            "view(index) must print in the core: {:?}",
            h.console.lines
        );

        // a pane op: declined, not crashed and not silently dropped.
        {
            let mut core = h.ctx(&player);
            apply_op(Op::View(ViewTarget::Eq), &mut core, &mut surf);
        }
        let last = h.console.lines.last().expect("a line").clone();
        assert_eq!(
            last.kind,
            crate::shellsong::LineKind::Error,
            "a decline is a failure line: {last:?}"
        );
        assert!(
            last.text.contains("wim"),
            "and it says where the eq is: {}",
            last.text
        );

        // every op reports exactly one line (docs/shellsong.md R9),
        // declines included.
        let before = h.console.lines.len();
        {
            let mut core = h.ctx(&player);
            apply_op(Op::SelectTop, &mut core, &mut surf);
        }
        assert_eq!(
            h.console.lines.len(),
            before + 1,
            "one line per op, no more and no fewer"
        );
    }

    /// `Env` is built with no pane state either: `Headless` answers
    /// the only two readings it takes from a front-end. Needs no mpv,
    /// so this one is not ignored.
    #[test]
    fn the_env_needs_no_front_end_state() {
        let mut app = app::App::new(crate::search::Corpus::new());
        app.set_query("hex");
        let mut queue = PlayQueue::default();
        queue.reset(test_songs(2));
        let env = shellsong_env(&app, &queue, &Headless, true, Duration::from_secs(3));
        assert_eq!(env.query, "hex");
        assert_eq!(env.queue_len, 2);
        assert_eq!(env.focus, shellsong::Focus::Sidebar);
        assert!(!env.browsing, "a headless front-end is never browsing");
    }

    /// `dry()` describes every op in both binaries, including the ones
    /// a front-end would decline: it lowers to notes and never reaches
    /// the op table at all, so there is nothing to decline.
    #[test]
    fn dry_describes_even_what_a_front_end_would_refuse() {
        let mut program = Program::parse("dry(view(eq))").expect("parses");
        let env = shellsong_env(
            &app::App::new(crate::search::Corpus::new()),
            &PlayQueue::default(),
            &Headless,
            true,
            Duration::ZERO,
        );
        let mut notes = Vec::new();
        loop {
            match program.step(&env) {
                shellsong::Step::Note(t) => notes.push(t),
                shellsong::Step::Done => break,
                other => panic!("dry must only ever emit notes, got {other:?}"),
            }
        }
        let text = notes.join("\n");
        assert!(text.contains("view(eq)"), "{text}");
        assert!(text.contains("viewing eq"), "{text}");
    }

    /// `short_help` is derived from the language's own table, so a
    /// command added to shellsong shows up under `?` with nobody
    /// remembering to add it.
    #[test]
    fn the_short_help_lists_every_command_the_language_has() {
        let lines = short_help();
        let joined = lines.join(" ");
        for expected in ["play", "queue", "volume", "view", "dry", "quit"] {
            assert!(joined.contains(expected), "? must list {expected}");
        }
        let claimed: usize = lines[0]
            .split_whitespace()
            .next()
            .and_then(|n| n.parse().ok())
            .expect("a count on the first line");
        assert_eq!(
            claimed,
            shellsong::help_lines().len(),
            "the count must match the language's own table"
        );
        assert!(
            lines.iter().all(|l| l.chars().count() <= 80),
            "every row has to fit a narrow terminal: {lines:?}"
        );
    }

    /// Everything a `CoreCtx` needs that is not the mpv handle, owned,
    /// so a test can build one without a front-end in sight.
    struct Harness {
        cfg: config::Config,
        app: app::App,
        notifications: Notifications,
        queue: PlayQueue,
        previous_track: Option<SearchItem>,
        cache: Arc<Mutex<CacheIndex>>,
        download_tx: mpsc::Sender<DownloadEvent>,
        _download_rx: mpsc::Receiver<DownloadEvent>,
        downloading: HashSet<String>,
        scrobble: ScrobbleState,
        subtitle_tx: mpsc::Sender<SubtitleReady>,
        _subtitle_rx: mpsc::Receiver<SubtitleReady>,
        console: Console,
        now_playing: Option<SearchItem>,
        favorites: favorites::Favorites,
        remote_starred: HashSet<String>,
        lyrics_client: lyrics::Client,
        lyrics_loader: lyrics::Loader,
        track_resolver: TrackResolver,
        star_tx: mpsc::Sender<StarFailed>,
        _star_rx: mpsc::Receiver<StarFailed>,
        base_volume: f64,
        crossfade_enabled: bool,
        undo: undo::Recorder,
        gap_secs: f64,
        between_tracks: BetweenTracks,
        fade_in_started_at: Option<Instant>,
        fade: Option<Ramp>,
        sleep_due: Option<Duration>,
        resume_due: Option<Duration>,
        scan_rx: mpsc::Receiver<library::ScanBatch>,
        _scan_tx: mpsc::Sender<library::ScanBatch>,
        remote_search_tx: mpsc::Sender<RemoteSearchQuery>,
        _remote_search_rx: mpsc::Receiver<RemoteSearchQuery>,
        settle: SearchSettle,
        saved_queues: Vec<String>,
        vars: shellsong::Scope,
        bindings: shellsong_host::Bindings,
    }

    impl Harness {
        fn new() -> Harness {
            let (download_tx, _download_rx) = mpsc::channel();
            let (subtitle_tx, _subtitle_rx) = mpsc::channel();
            let (_scan_tx, scan_rx) = mpsc::channel();
            let (remote_search_tx, _remote_search_rx) = mpsc::channel();
            let (star_tx, star_rx) = mpsc::channel();
            Harness {
                cfg: config::Config::default(),
                app: app::App::new(crate::search::Corpus::new()),
                notifications: Notifications::default(),
                queue: PlayQueue::default(),
                previous_track: None,
                cache: Arc::new(Mutex::new(CacheIndex::default())),
                download_tx,
                _download_rx,
                downloading: HashSet::new(),
                scrobble: ScrobbleState::default(),
                subtitle_tx,
                _subtitle_rx,
                console: Console::empty(),
                now_playing: Some(test_songs(1).remove(0)),
                favorites: favorites::Favorites::default(),
                remote_starred: HashSet::new(),
                lyrics_client: lyrics::Client::new(),
                lyrics_loader: lyrics::Loader::new(),
                track_resolver: TrackResolver::new(),
                star_tx,
                _star_rx: star_rx,
                base_volume: 100.0,
                crossfade_enabled: false,
                undo: undo::Recorder::default(),
                gap_secs: 0.0,
                between_tracks: BetweenTracks::new(),
                fade_in_started_at: None,
                fade: None,
                sleep_due: None,
                resume_due: None,
                scan_rx,
                _scan_tx,
                remote_search_tx,
                _remote_search_rx,
                settle: SearchSettle::default(),
                saved_queues: Vec::new(),
                vars: shellsong::Scope::default(),
                bindings: shellsong_host::Bindings::default(),
            }
        }

        fn ctx<'a>(&'a mut self, player: &'a mpv::Mpv) -> CoreCtx<'a> {
            CoreCtx {
                cfg: &self.cfg,
                player,
                app: &mut self.app,
                notifications: &mut self.notifications,
                queue: &mut self.queue,
                previous_track: &mut self.previous_track,
                cache: &self.cache,
                download_tx: &self.download_tx,
                downloading: &mut self.downloading,
                scrobble: &mut self.scrobble,
                subtitle_tx: &self.subtitle_tx,
                console: &mut self.console,
                now: Duration::from_secs(1),
                now_playing: &self.now_playing,
                favorites: &mut self.favorites,
                remote_starred: &mut self.remote_starred,
                lyrics_client: &self.lyrics_client,
                lyrics_loader: &mut self.lyrics_loader,
                star_tx: &self.star_tx,
                track_resolver: &mut self.track_resolver,
                base_volume: &mut self.base_volume,
                crossfade_enabled: &mut self.crossfade_enabled,
                undo: &mut self.undo,
                gap_secs: &mut self.gap_secs,
                between_tracks: &mut self.between_tracks,
                fade_in_started_at: &mut self.fade_in_started_at,
                fade: &mut self.fade,
                sleep_due: &mut self.sleep_due,
                resume_due: &mut self.resume_due,
                scan_rx: &mut self.scan_rx,
                remote_search_tx: &self.remote_search_tx,
                settle: &mut self.settle,
                saved_queues: &mut self.saved_queues,
                vars: &mut self.vars,
                bindings: &mut self.bindings,
            }
        }
    }

    // ----------------------------------------------- optimistic stars

    /// The point of the change: the local set flips before any network
    /// call can have answered, so the next frame already shows it.
    /// This used to be a round trip on the keypress.
    #[test]
    fn starring_a_remote_song_flips_the_set_without_waiting() {
        let (star_tx, _star_rx) = mpsc::channel();
        let mut favorites = favorites::Favorites::default();
        let mut starred = HashSet::new();
        let item = test_songs(1).remove(0);
        let crate::search::SearchItem::Song { id, .. } = &item else {
            panic!("expected a song");
        };
        let id = id.clone();

        let now = toggle_star(&item, &mut favorites, &mut starred, &star_tx)
            .expect("a song can be starred");
        assert!(now, "it reports the new state at once");
        assert!(starred.contains(&id), "and the set already holds it");

        let now =
            toggle_star(&item, &mut favorites, &mut starred, &star_tx).expect("and unstarred");
        assert!(!now);
        assert!(!starred.contains(&id));
    }

    /// A refusal has to be reportable: the sender carries which way the
    /// flip went, because the id alone cannot say how to undo it.
    #[test]
    fn a_refusal_carries_enough_to_undo_the_flip() {
        let failed = StarFailed {
            id: "s1".into(),
            wanted: true,
            error: "502".into(),
        };
        let mut starred = HashSet::from(["s1".to_string()]);
        // what both binaries' drains do.
        if failed.wanted {
            starred.remove(&failed.id);
        } else {
            starred.insert(failed.id.clone());
        }
        assert!(!starred.contains("s1"), "a refused star is taken back");

        let failed = StarFailed {
            id: "s2".into(),
            wanted: false,
            error: "502".into(),
        };
        let mut starred: HashSet<String> = HashSet::new();
        if failed.wanted {
            starred.remove(&failed.id);
        } else {
            starred.insert(failed.id.clone());
        }
        assert!(starred.contains("s2"), "a refused unstar is put back");
    }

    /// Local files are a file write, not a round trip, so they stay
    /// synchronous and must keep working unchanged.
    #[test]
    fn a_local_file_still_toggles_synchronously() {
        let (star_tx, _star_rx) = mpsc::channel();
        let mut favorites = favorites::Favorites::default();
        let mut starred = HashSet::new();
        let item = crate::search::SearchItem::LocalMusic {
            path: std::path::PathBuf::from("/music/a.flac"),
            title: "a".into(),
            artist: None,
            album: None,
        };
        assert!(toggle_star(&item, &mut favorites, &mut starred, &star_tx).unwrap());
        assert!(starred.is_empty(), "a local star is not a remote one");
    }

    #[test]
    fn only_tracks_can_be_starred() {
        let (star_tx, _star_rx) = mpsc::channel();
        let mut favorites = favorites::Favorites::default();
        let mut starred = HashSet::new();
        let item = crate::search::SearchItem::Playlist {
            id: "p1".into(),
            name: "mix".into(),
            client: crate::browse::tests::test_client(),
        };
        assert!(toggle_star(&item, &mut favorites, &mut starred, &star_tx).is_err());
    }

    // ------------------------------------------- the enqueue resolver

    /// The commonest action must not pay a frame for a thread: a
    /// single song is already its own track list, so it resolves
    /// instantly and the caller queues it in the same keypress.
    #[test]
    fn a_single_song_resolves_without_a_request() {
        let mut r = TrackResolver::new();
        let song = test_songs(1).remove(0);
        match r.request(&song, Intent::Back) {
            Resolution::Ready(tracks) => assert_eq!(tracks.len(), 1),
            Resolution::Fetching(_) => panic!("a song needs no round trip"),
        }
    }

    #[test]
    fn a_local_file_resolves_without_a_request() {
        let mut r = TrackResolver::new();
        let item = crate::search::SearchItem::LocalMusic {
            path: std::path::PathBuf::from("/music/a.flac"),
            title: "a".into(),
            artist: None,
            album: None,
        };
        assert!(matches!(
            r.request(&item, Intent::Back),
            Resolution::Ready(_)
        ));
    }

    /// An album needs a round trip, so the keypress returns at once
    /// with something to say rather than blocking on it.
    #[test]
    fn an_album_is_fetched_and_names_what_it_is_waiting_on() {
        let mut r = TrackResolver::new();
        let album = crate::search::SearchItem::Album {
            id: "al1".into(),
            name: "crest".into(),
            artist: "bladee".into(),
            client: crate::browse::tests::test_client(),
        };
        match r.request(&album, Intent::Back) {
            Resolution::Fetching(label) => assert_eq!(label, "crest"),
            Resolution::Ready(_) => panic!("an album is a round trip"),
        }
    }

    /// The bug this resolver shipped with, and the reason the guard
    /// counts queue replacements instead of requests.
    ///
    /// Queueing is additive: asking for two albums in a row means
    /// both. A per-request counter made the second request supersede
    /// the first, so the first album's tracks were dropped on arrival
    /// and nothing said so. Queueing an artist then an album is the
    /// realistic way in, since the artist fan-out is slow enough to
    /// still be running.
    /// Half one: asking for a second thing must not disown the
    /// first. Asserted on the epoch rather than by polling, because
    /// `request` spawns a real worker whose reply would race anything
    /// this test staged; the epoch is the whole mechanism, so it is
    /// also the honest thing to assert on.
    #[test]
    fn requesting_does_not_supersede_an_earlier_request() {
        let mut r = TrackResolver::new();
        let album = crate::search::SearchItem::Album {
            id: "al1".into(),
            name: "first".into(),
            artist: "a".into(),
            client: crate::browse::tests::test_client(),
        };
        let before = r.epoch();
        r.request(&album, Intent::Back);
        r.request(&album, Intent::Back);
        assert_eq!(
            r.epoch(),
            before,
            "a request that bumped the epoch would drop every batch \
             still in flight, which for queueing means losing tracks \
             the user asked for"
        );
    }

    /// Half two: and `poll` hands both of them over, in order.
    #[test]
    fn two_batches_at_the_current_epoch_both_arrive() {
        let mut r = TrackResolver::new();
        let at = r.epoch();
        r.deliver(at, Intent::Back, test_songs(3));
        r.deliver(at, Intent::Back, test_songs(2));
        let landed: Vec<usize> = std::iter::from_fn(|| r.poll())
            .map(|(_, _, tracks)| tracks.unwrap().len())
            .collect();
        assert_eq!(
            landed,
            vec![3, 2],
            "both requests were asked for, so both must land, in order"
        );
    }

    /// Replacing or emptying the queue has to disown what is in
    /// flight, or a batch resolved against the old queue lands in the
    /// new one. The only thing that bumps the epoch.
    #[test]
    fn invalidate_disowns_a_batch_in_flight() {
        let mut r = TrackResolver::new();
        let before = r.epoch();
        r.invalidate();
        r.deliver(before, Intent::Back, test_songs(2));
        assert!(r.poll().is_none());
    }

    /// And it disowns *every* one of them, not just the newest: the
    /// point of dropping the per-request bump is that several can be
    /// outstanding at once, so clearing the queue has to clear them
    /// all.
    #[test]
    fn invalidate_disowns_every_batch_in_flight_not_just_the_last() {
        let mut r = TrackResolver::new();
        let before = r.epoch();
        r.invalidate();
        r.deliver(before, Intent::Back, test_songs(2));
        r.deliver(before, Intent::Front, test_songs(4));
        assert!(r.poll().is_none());
    }

    /// The two measured facts `silence_url` rests on, against a real
    /// mpv, so an mpv or ffmpeg upgrade cannot quietly change either.
    ///
    /// `#[ignore]` per the repo convention for anything needing mpv
    /// (~400ms to spawn): `cargo test --lib seam -- --ignored`.
    #[test]
    #[ignore]
    fn mpv_loads_a_silence_but_misreports_its_length() {
        let player = mpv::Mpv::spawn().expect("mpv on PATH");
        // never the user's speakers, same as the other mpv test here.
        let _ = player.set_property("ao", serde_json::json!("null"));
        let _ = player.set_property("vo", serde_json::json!("null"));

        player
            .load_file(&silence_url(600))
            .expect("mpv accepts the lavfi source as a file");

        // it has to actually become the playing file, not merely be
        // accepted: a source mpv fails to open would leave the queue
        // holding an entry that never plays.
        let mut loaded = false;
        for _ in 0..50 {
            if player
                .get_property("audio-codec-name")
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .is_some()
            {
                loaded = true;
                break;
            }
            thread::sleep(std::time::Duration::from_millis(40));
        }
        assert!(loaded, "the silence never became the playing file");

        // the lie, pinned. mpv reports ~1s whatever `d` says, at every
        // spelling tried, which is why anything drawing the length of
        // a silence must read `Silence { secs }` instead.
        let reported = player
            .get_property("duration")
            .ok()
            .and_then(|v| v.as_f64())
            .expect("a duration is reported, it is just wrong");
        assert!(
            reported < 600.0 * 0.5,
            "mpv reported {reported}s for a 600s silence; if this now \
             matches, the scrubber may read mpv's duration again"
        );
    }

    /// The undo hook, end to end against the real `apply_op` and a
    /// real mpv: `apply_op` is the only place that records, so this is
    /// the only way to find out whether it does.
    ///
    /// `#[ignore]` for the same reason as the test above (~400ms to
    /// spawn mpv): `cargo test --lib seam -- --ignored`.
    #[test]
    #[ignore]
    fn an_op_is_recorded_once_and_undoing_it_puts_the_state_back() {
        let player = mpv::Mpv::spawn().expect("mpv on PATH");
        let _ = player.set_property("ao", serde_json::json!("null"));
        let mut h = Harness::new();
        let mut surf = Headless;

        // nothing recorded yet, so there is nothing to undo and it
        // says so rather than silently doing nothing.
        {
            let mut core = h.ctx(&player);
            apply_op(Op::Undo, &mut core, &mut surf);
        }
        assert!(
            h.console
                .lines
                .last()
                .unwrap()
                .text
                .contains("nothing to undo"),
            "{:?}",
            h.console.lines
        );

        {
            let mut core = h.ctx(&player);
            apply_op(Op::SetGap(Some(10.0)), &mut core, &mut surf);
        }
        assert_eq!(h.gap_secs, 10.0);
        // root (the state before) plus the one op.
        assert_eq!(h.undo.len(), 2, "one op, one node");

        {
            let mut core = h.ctx(&player);
            apply_op(Op::Undo, &mut core, &mut surf);
        }
        assert_eq!(h.gap_secs, 0.0, "undo put the gap back");
        assert_eq!(h.undo.len(), 2, "undo walks the tree, it does not grow it");

        {
            let mut core = h.ctx(&player);
            apply_op(Op::Redo, &mut core, &mut surf);
        }
        assert_eq!(h.gap_secs, 10.0, "and redo walks back down");
        assert_eq!(h.undo.len(), 2);
    }

    /// The end-of-playlist decision, which is the part both
    /// front-ends share. shellswim did not make it at all before: mpv
    /// going idle was unobserved, so a finished queue left the bar
    /// showing a track that had stopped.
    #[test]
    fn a_drained_queue_clears_unless_loop_all_has_a_session() {
        assert_eq!(
            drain_decision(0, LoopMode::Off, false),
            Decision::Clear,
            "loop off: the queue is over"
        );
        assert_eq!(
            drain_decision(0, LoopMode::One, false),
            Decision::Clear,
            "loop one is mpv's own loop-file and does not replay a queue"
        );
        assert_eq!(drain_decision(0, LoopMode::All, false), Decision::Restart);
        assert_eq!(
            drain_decision(0, LoopMode::All, true),
            Decision::Clear,
            "loop all with nothing to replay has to clear, not restart from nothing"
        );
    }

    /// The guard that keeps a *stale* idle report from clearing a
    /// queue that just got refilled. It lives inside the shared
    /// function rather than at each call site, because a guard every
    /// caller has to remember is one a caller will forget -- which is
    /// how shellswim came to have no end-of-playlist handling at all.
    #[test]
    fn a_stale_idle_report_leaves_a_live_queue_alone() {
        for mode in [LoopMode::Off, LoopMode::One, LoopMode::All] {
            assert_eq!(
                drain_decision(3, mode, false),
                Decision::Stale,
                "{mode:?} acted on a stale report"
            );
        }
    }

    /// The bar's controls are commands, and this is the one table
    /// that says which. Both enter-on-the-bar (`Op::Go`) and the
    /// click/keypress path (`main::activate_transport`) read it, so
    /// the two cannot drift the way they had: `activate_transport`
    /// used to reimplement six of these against mpv directly.
    #[test]
    fn every_bar_control_but_the_scrubber_is_an_op() {
        use crate::ui::controlsbar::FIELDS;
        for field in FIELDS {
            let op = op_for_field(field);
            if field == Field::Progress {
                assert!(
                    op.is_none(),
                    "the scrubber is a drag, not a one-shot: {op:?}"
                );
            } else {
                assert!(op.is_some(), "{field:?} has no command behind it");
            }
        }
    }

    /// Pressing a bar control has to land in the undo tree, which is
    /// the other thing routing it through an op buys. Asserted on the
    /// mapping plus `classify`, since the click itself is main.rs's.
    #[test]
    fn a_bar_control_is_undoable() {
        use crate::ui::controlsbar::FIELDS;
        for field in FIELDS {
            let Some(op) = op_for_field(field) else {
                continue;
            };
            // `View(Eq)` opens a menu and is correctly not an edit;
            // every other bar control changes playback state.
            let expected = if field == Field::Eq {
                undo::Reversibility::Ignored
            } else {
                undo::Reversibility::Restorable
            };
            assert_eq!(undo::classify(&op), expected, "{field:?}");
        }
    }

    /// The claim the suspend flag exists for: twelve of `run_op`'s
    /// arms go through `dispatch_action`, which records on its own for
    /// a keypress, so a delegated op has to come out as one row rather
    /// than two.
    #[test]
    #[ignore]
    fn a_delegated_op_is_one_row_not_two() {
        let player = mpv::Mpv::spawn().expect("mpv on PATH");
        let _ = player.set_property("ao", serde_json::json!("null"));
        let mut h = Harness::new();
        let mut surf = Headless;
        {
            let mut core = h.ctx(&player);
            // `SetLoop` is one of the twelve: the arm calls
            // `dispatch_action(Action::SetLoop(..))`.
            apply_op(Op::SetLoop(LoopMode::All), &mut core, &mut surf);
        }
        assert_eq!(h.app.loop_mode, LoopMode::All);
        assert_eq!(h.undo.len(), 2, "root plus one row, not two rows");
    }

    /// And a keypress that never goes near an op is recorded, which is
    /// the gap this closes.
    ///
    /// `Queued` rather than `SetLoop`, which is how the root-planting
    /// bug above was found: `app.handle_key` flips `loop_mode` *before*
    /// producing that action, so `SetLoop`'s arm only tells mpv about a
    /// change `App` already has. `Queued` carries its own mutation, so
    /// it actually exercises the hook.
    #[test]
    #[ignore]
    fn a_keypress_action_is_recorded_on_its_own() {
        let player = mpv::Mpv::spawn().expect("mpv on PATH");
        let _ = player.set_property("ao", serde_json::json!("null"));
        let mut h = Harness::new();
        let mut surf = Headless;

        // the host plants the root before reading any key; without it
        // nothing on this path records at all.
        {
            let core = h.ctx(&player);
            let root = undo::Snapshot::capture(&core);
            let now = core.now;
            core.undo.plant(root, now);
        }
        let before = h.queue.entries().len();

        {
            let mut core = h.ctx(&player);
            dispatch_action(
                Action::Queued(test_songs(1).remove(0)),
                &mut core,
                &mut surf,
            );
        }
        assert_eq!(h.queue.entries().len(), before + 1, "the track queued");
        assert_eq!(h.undo.len(), 2, "the keyboard path records too");

        {
            let mut core = h.ctx(&player);
            apply_op(Op::Undo, &mut core, &mut surf);
        }
        assert_eq!(
            h.queue.entries().len(),
            before,
            "and undo took the track back out"
        );
    }

    /// A failed op changed nothing, so recording it would put a row in
    /// the tree that undoes to the state it is already in.
    #[test]
    #[ignore]
    fn a_failed_op_is_not_recorded() {
        let player = mpv::Mpv::spawn().expect("mpv on PATH");
        let _ = player.set_property("ao", serde_json::json!("null"));
        let mut h = Harness::new();
        let mut surf = Headless;
        {
            let mut core = h.ctx(&player);
            // an eq preset that does not exist: resolved in memory
            // against the config, so this fails without touching a
            // socket or the disk.
            apply_op(Op::SetEqPreset("nope".into()), &mut core, &mut surf);
        }
        assert!(
            h.console
                .lines
                .last()
                .unwrap()
                .text
                .contains("no eq preset"),
            "the op has to actually fail for this to mean anything: {:?}",
            h.console.lines
        );
        // one node is the *root*, the state the session was in; what
        // must not appear is a second node for an op that changed
        // nothing, which would undo to the state it is already in.
        assert_eq!(h.undo.len(), 1, "{:?}", h.console.lines);
        {
            let mut core = h.ctx(&player);
            apply_op(Op::Undo, &mut core, &mut surf);
        }
        assert!(
            h.console
                .lines
                .last()
                .unwrap()
                .text
                .contains("nothing to undo"),
            "at the root, so there is nothing behind it: {:?}",
            h.console.lines
        );
    }

    const GAP: Duration = Duration::from_secs(10);

    fn at(base: Instant, secs: u64) -> Instant {
        base + Duration::from_secs(secs)
    }

    #[test]
    fn a_gap_begins_at_the_held_end_and_releases_when_it_is_up() {
        let mut g = BetweenTracks::new();
        let t0 = Instant::now();
        assert_eq!(g.tick(false, GAP, true, t0), None, "mid-track, nothing");
        assert_eq!(g.tick(true, GAP, true, t0), Some(GapAction::Begin));
        assert_eq!(g.tick(true, GAP, true, at(t0, 5)), None, "still holding");
        assert_eq!(
            g.tick(true, GAP, true, at(t0, 10)),
            Some(GapAction::Release)
        );
        assert!(!g.holding());
    }

    /// The latch, and the reason it exists. `Release` sends
    /// `playlist-next`, but `eof-reached` stays true until mpv's own
    /// property change lands a frame or two later. Without the latch
    /// that stale `true` starts a second gap at once and every track
    /// gets two of them.
    #[test]
    fn a_stale_eof_after_release_does_not_start_a_second_gap() {
        let mut g = BetweenTracks::new();
        let t0 = Instant::now();
        g.tick(true, GAP, true, t0);
        assert_eq!(
            g.tick(true, GAP, true, at(t0, 10)),
            Some(GapAction::Release)
        );
        // mpv has not caught up yet: still reporting the old end.
        assert_eq!(g.tick(true, GAP, true, at(t0, 10)), None);
        assert_eq!(g.tick(true, GAP, true, at(t0, 11)), None);
        // and once it has, the next track's end gaps normally.
        g.tick(false, GAP, true, at(t0, 12));
        assert_eq!(g.tick(true, GAP, true, at(t0, 20)), Some(GapAction::Begin));
    }

    /// The end of the queue stays the end of the queue: holding there
    /// would wait out a gap and then skip into nothing, and mpv under
    /// `keep-open` never goes idle, so the end-of-playlist path would
    /// never run either.
    #[test]
    fn the_last_track_does_not_gap() {
        let mut g = BetweenTracks::new();
        assert_eq!(g.tick(true, GAP, false, Instant::now()), None);
        assert!(!g.holding());
    }

    #[test]
    fn no_gap_configured_means_no_hold() {
        let mut g = BetweenTracks::new();
        assert_eq!(g.tick(true, Duration::ZERO, true, Instant::now()), None);
    }

    #[test]
    fn the_countdown_counts_down_and_never_goes_negative() {
        let mut g = BetweenTracks::new();
        let t0 = Instant::now();
        assert_eq!(g.remaining(t0), None, "no gap, nothing to count");
        g.tick(true, GAP, true, t0);
        assert_eq!(g.remaining(t0), Some(GAP));
        assert_eq!(g.remaining(at(t0, 4)), Some(Duration::from_secs(6)));
        assert_eq!(
            g.remaining(at(t0, 99)),
            Some(Duration::ZERO),
            "saturating, not a panic"
        );
    }

    /// "Get on with it" drops the hold without advancing, and must
    /// not let the same still-true `eof` immediately re-gap.
    #[test]
    fn cancelling_a_gap_does_not_immediately_re_gap() {
        let mut g = BetweenTracks::new();
        let t0 = Instant::now();
        g.tick(true, GAP, true, t0);
        g.cancel();
        assert!(!g.holding());
        assert_eq!(g.tick(true, GAP, true, t0), None);
    }

    fn empty_cache() -> Arc<Mutex<CacheIndex>> {
        Arc::new(Mutex::new(CacheIndex::default()))
    }

    fn an_album() -> SearchItem {
        crate::search::SearchItem::Album {
            id: "al1".into(),
            name: "crest".into(),
            artist: "bladee".into(),
            client: crate::browse::tests::test_client(),
        }
    }

    /// The latent desync, at the seam where it lived.
    ///
    /// `load_playlist` and `resync_upcoming` skipped an unresolvable
    /// item when appending to mpv but stored the whole vec in
    /// `PlayQueue`, so every index after it pointed at the wrong
    /// track. Pairing each kept item with the path mpv was actually
    /// given is what makes storing the wrong list impossible.
    #[test]
    fn an_unplayable_entry_is_separated_rather_than_skipped() {
        let cache = empty_cache();
        let mut items = test_songs(2);
        // an album has no path of its own: the exact shape that
        // reached mpv as 2 entries and `PlayQueue` as 3.
        items.insert(1, an_album());

        let (playable, dropped) = resolve_all(items, &cache);
        assert_eq!(playable.len(), 2, "two songs resolve");
        assert_eq!(dropped.len(), 1, "the album does not, and is reported");
        assert!(
            playable.iter().all(|(_, path)| !path.is_empty()),
            "every kept item carries the path mpv will be given"
        );
    }

    /// Order has to survive, or the pairing fixes the length and
    /// breaks the mapping a different way.
    #[test]
    fn resolving_preserves_order() {
        let cache = empty_cache();
        let songs = test_songs(3);
        let titles: Vec<String> = songs.iter().map(|s| s.primary_name().to_string()).collect();
        let mut items = songs;
        items.insert(2, an_album());

        let (playable, _) = resolve_all(items, &cache);
        let kept: Vec<String> = playable
            .iter()
            .map(|(i, _)| i.primary_name().to_string())
            .collect();
        assert_eq!(kept, titles);
    }

    /// The normal case must stay silent: every play would otherwise
    /// end in "(0 skipped)".
    #[test]
    fn nothing_lost_says_nothing() {
        assert_eq!(lost_note(0), "");
        assert_eq!(lost_note(3), " (3 skipped)");
    }

    /// Downloading an album is the same fan-out as queueing one, so it
    /// shares the resolver instead of growing a fourth loader, and so
    /// it must not block either.
    #[test]
    fn downloading_an_album_is_fetched_and_a_song_is_not() {
        let mut r = TrackResolver::new();
        let album = crate::search::SearchItem::Album {
            id: "al1".into(),
            name: "crest".into(),
            artist: "a".into(),
            client: crate::browse::tests::test_client(),
        };
        assert!(matches!(
            r.request(&album, Intent::Download),
            Resolution::Fetching(_)
        ));
        let song = test_songs(1).remove(0);
        assert!(matches!(
            r.request(&song, Intent::Download),
            Resolution::Ready(_)
        ));
    }

    /// The cost of sharing one resolver: `invalidate` means "the queue
    /// those tracks were for is gone", which is true of a queue batch
    /// and says nothing about a download. Clearing the queue must not
    /// cancel a download asked for seconds earlier.
    #[test]
    fn invalidate_does_not_cancel_an_in_flight_download() {
        let mut r = TrackResolver::new();
        let before = r.epoch();
        r.invalidate();
        r.deliver(before, Intent::Back, test_songs(2));
        r.deliver(before, Intent::Download, test_songs(5));
        let landed: Vec<(Intent, usize)> = std::iter::from_fn(|| r.poll())
            .map(|(into, _, tracks)| (into, tracks.unwrap().len()))
            .collect();
        assert_eq!(
            landed,
            vec![(Intent::Download, 5)],
            "the queue batch is disowned, the download is not"
        );
    }

    #[test]
    fn a_current_batch_arrives_with_its_destination() {
        let mut r = TrackResolver::new();
        r.deliver(r.epoch(), Intent::Front, test_songs(2));
        let (into, _, tracks) = r.poll().expect("a current batch lands");
        assert_eq!(into, Intent::Front, "it must queue where it was asked to");
        assert_eq!(tracks.unwrap().len(), 2);
    }
}

#[cfg(test)]
mod result_echo {
    use super::*;
    use crate::library::{LocalTrack, MediaKind};
    use crate::search::Corpus;
    use std::path::PathBuf;

    fn corpus(titles: &[&str]) -> Corpus {
        let mut c = Corpus::new();
        c.add_local_tracks(titles.iter().map(|t| LocalTrack {
            path: PathBuf::from(format!("/m/{t}.flac")),
            title: (*t).into(),
            artist: Some("Various".into()),
            album: Some("Radio".into()),
            duration_secs: None,
            kind: MediaKind::Music,
        }));
        c
    }

    #[test]
    fn print_the_echo() {
        let c = corpus(&[
            "Rosu si Negru",
            "Sergiu si Andrei",
            "Cine-s Sergiu si Andrei",
            "Si Daca Tara",
            "Noapte Buna",
            "Vara Mea",
            "Doar Pentru Tine",
            "Cand Noaptea Vine",
            "Inima Mea",
            "Fara Tine",
            "Zi de Zi",
        ]);
        let results = c.search("si");
        for w in [72usize, 100, 40] {
            let lines = echo_lines(&results, &mut HashSet::new(), w - 4);
            println!("--- {w} cells, {} results", results.len());
            for line in &lines {
                println!("  [{line}]");
            }
            // a floor, so this is a test and not only a printer: a
            // wide terminal must fold the list into fewer rows than
            // there are results, and no row may overrun the width.
            assert!(!lines.is_empty());
            if w > 60 {
                assert!(lines.len() < results.len(), "{w} did not use columns");
            }
            for line in &lines {
                assert!(crate::ui::textfit::width(line) <= w - 4, "{line:?}");
            }
        }
    }

    #[test]
    fn nothing_found_prints_nothing() {
        assert!(echo_lines(&[], &mut HashSet::new(), 76).is_empty());
    }

    /// The point of diffing by name rather than by index: a server
    /// answering late re-ranks the whole list, so the new matches are
    /// not a tail. Only the genuinely new ones print, and never twice.
    #[test]
    fn a_late_batch_prints_only_what_is_new() {
        let local = corpus(&["Rosu si Negru", "Zi de Zi"]);
        let first = local.search("si");
        let mut shown = HashSet::new();
        let lines = echo_lines(&first, &mut shown, 76);
        assert!(!lines.is_empty());
        let before = shown.len();

        // the same two, re-ranked, plus two that arrived later.
        let later = corpus(&["Zi de Zi", "Alb si Negru", "Rosu si Negru", "Sus si Jos"]);
        let second = later.search("si");
        let fresh = echo_lines(&second, &mut shown, 76);
        let printed = fresh.join(" ");
        assert!(printed.contains("Alb si Negru"), "{printed:?}");
        assert!(printed.contains("Sus si Jos"), "{printed:?}");
        assert!(
            !printed.contains("Rosu si Negru"),
            "already shown, must not repeat: {printed:?}"
        );
        assert_eq!(shown.len(), before + 2);

        // and nothing is left to say the third time.
        assert!(echo_lines(&second, &mut shown, 76).is_empty());
    }

    /// The cap counts what has been printed across both batches, so a
    /// late arrival cannot push the total past it.
    #[test]
    fn the_cap_spans_batches() {
        let titles: Vec<String> = (0..40).map(|i| format!("song {i:02}")).collect();
        let refs: Vec<&str> = titles.iter().map(String::as_str).collect();
        let c = corpus(&refs);
        let results = c.search("song");
        let mut shown = HashSet::new();
        echo_lines(&results, &mut shown, 76);
        assert_eq!(shown.len(), MAX_ECHOED_RESULTS);
        assert!(
            echo_lines(&results, &mut shown, 76).is_empty(),
            "the cap is already spent"
        );
    }

    /// Past the cap the list is the flood it exists to avoid, so it
    /// says how much it is not showing.
    #[test]
    fn a_huge_result_set_is_capped_and_says_so() {
        let titles: Vec<String> = (0..80).map(|i| format!("song {i:02}")).collect();
        let refs: Vec<&str> = titles.iter().map(String::as_str).collect();
        let c = corpus(&refs);
        let results = c.search("song");
        assert!(results.len() > MAX_ECHOED_RESULTS);
        let lines = echo_lines(&results, &mut HashSet::new(), 76);
        assert!(lines.last().unwrap().starts_with('+'), "{:?}", lines.last());
    }
}
