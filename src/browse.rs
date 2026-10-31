//! Track-listing view opened on an album or playlist. Opening is a
//! separate action from playing: picking an album/playlist from search
//! shows its tracks here first ("let me scroll through the songs"),
//! rather than jumping straight into playback. Closing (esc) always
//! returns `main_content` to whatever it'd otherwise show (now-playing,
//! lyrics, or the idle banner/logo).
//!
//! Opening never blocks: `Loader` hands back the view at once and the
//! fetches report back over a channel the main loop drains. See
//! docs/async-open.md for the stale-reply rules.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;

use ratatui::layout::Rect;

use crate::search::SearchItem;
use crate::subsonic::{ArtistLoad, Client};
use crate::ui::raster::Tone;
use crate::ui::textfit;

/// What the art panel actually shows, named after what the user gets
/// rather than which library draws it. Two axes crossed:
///
/// - `Ascii*` is wim's own rasterizer (`ui::raster`): block glyphs,
///   nothing needed from the terminal, and twice the horizontal detail
///   a half-block renderer carries.
/// - `Full*` is a real inline graphics protocol (sixel/kitty/iterm2)
///   through `ratatui-image`, overlaid by the host on the rect the
///   widget leaves empty. Unavailable terminals never reach these; see
///   `main::has_real_protocol`.
/// - `*Bw` is greyscale, `*Color` the source's own colour.
///
/// `Off` skips the panel entirely, giving the track list the full
/// width. Colour in the two `*Color` modes is the one sanctioned
/// exception to the monochrome rule (see ROADMAP.md); nothing else on
/// screen may carry a hue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtMode {
    AsciiBw,
    AsciiColor,
    FullBw,
    FullColor,
    Off,
}

impl ArtMode {
    /// Drawn by wim's own rasterizer, i.e. the widget paints the panel
    /// itself instead of leaving the rect to the host's image overlay.
    pub fn is_ascii(self) -> bool {
        matches!(self, ArtMode::AsciiBw | ArtMode::AsciiColor)
    }

    /// Needs a real graphics protocol, so the widget draws nothing in
    /// the art rect and the host overlays an image there.
    pub fn is_full(self) -> bool {
        matches!(self, ArtMode::FullBw | ArtMode::FullColor)
    }

    /// Greyscale: the `bw` half of each pair. For a `Full*` mode this
    /// is what the host converts the image by before encoding it.
    pub fn is_bw(self) -> bool {
        matches!(self, ArtMode::AsciiBw | ArtMode::FullBw)
    }

    pub fn tone(self) -> Tone {
        if self == ArtMode::AsciiColor {
            Tone::Color
        } else {
            Tone::Mono
        }
    }

    /// What a `Full*` mode degrades to where no graphics protocol
    /// exists: the matching ascii mode, so asking for colour still
    /// gets colour and asking for grey still gets grey.
    pub fn without_graphics(self) -> Self {
        match self {
            ArtMode::FullBw => ArtMode::AsciiBw,
            ArtMode::FullColor => ArtMode::AsciiColor,
            other => other,
        }
    }

    /// The next mode in the cycle `i` walks, ignoring what the
    /// terminal can actually render.
    pub fn next(self) -> Self {
        match self {
            ArtMode::AsciiBw => ArtMode::AsciiColor,
            ArtMode::AsciiColor => ArtMode::FullBw,
            ArtMode::FullBw => ArtMode::FullColor,
            ArtMode::FullColor => ArtMode::Off,
            ArtMode::Off => ArtMode::AsciiBw,
        }
    }

    /// `next`, but stepping straight past the `Full*` modes when the
    /// terminal has no graphics protocol: a full mode there is a blank
    /// pane, not "ascii anyway", so cycling into one looks broken. The
    /// flag says one was skipped, which is what the host says out loud
    /// once. `Off` stays reachable either way.
    pub fn cycle(self, real_graphics: bool) -> (Self, bool) {
        let mut next = self.next();
        let mut skipped = false;
        while next.is_full() && !real_graphics {
            next = next.next();
            skipped = true;
        }
        (next, skipped)
    }

    /// How the mode is named on screen (the settings menu) and in
    /// config.toml, hyphen for hyphen.
    pub fn label(self) -> &'static str {
        match self {
            ArtMode::AsciiBw => "ascii-bw",
            ArtMode::AsciiColor => "ascii-color",
            ArtMode::FullBw => "full-bw",
            ArtMode::FullColor => "full-color",
            ArtMode::Off => "off",
        }
    }
}

/// A plain rule, `draw_header`'s own title+rule line, and the one
/// blank gap row after it; fixed because the header's own content is
/// fixed, not computed dynamically per open view. The leading plain
/// rule exists so this view's very first row is a continuation of
/// the sidebar's own search-box top border (`ui::junctions` joins the
/// two) rather than title text sharing a row with that border and
/// reading as crammed against it.
const HEADER_HEIGHT: u16 = 4;

/// Artist bios get up to 3 wrapped lines plus one gap row; albums/
/// playlists never have one so this just doesn't apply to them.
const BIO_LINES: usize = 3;
const BIO_HEIGHT: u16 = BIO_LINES as u16 + 1;

/// Single source of truth for where the art panel and the track list
/// each land, so the widget (which draws into them) and main.rs
/// (which overlays a real image on the same spot in `Real` mode) can
/// never disagree about where that spot actually is. `cell_aspect` is
/// the terminal's actual cell height:width ratio (see
/// `main::cell_aspect`), not a hardcoded guess: using a fixed "2" here
/// while the widget sampled with a different fixed "2" happened to
/// agree with *each other*, but not necessarily with the terminal's
/// real font metrics, which is what actually made the art look
/// slightly too tall at some sizes.
pub fn layout(
    area: Rect,
    has_art: bool,
    has_bio: bool,
    is_artist: bool,
    cell_aspect: f32,
) -> (Option<Rect>, Rect) {
    let bio_height = if has_bio { BIO_HEIGHT } else { 0 };
    let y = area.y + HEADER_HEIGHT + bio_height;
    let body_height = (area.y + area.height).saturating_sub(y);

    if has_art && !is_artist && body_height > 1 {
        // Plain album view: cover big across the top, full-width track
        // list below it, rather than the narrow-side-column split used
        // for the artist profile. Art gets up to ~60% of the vertical
        // space as a starting budget, but (same "square first" rule as
        // below) width is what's actually capped against the pane, and
        // height is re-derived from that so the art never overflows
        // sideways or crowds out every row of the track list under it.
        let budget_height = ((body_height as f32 * 0.6).round() as u16)
            .min(body_height.saturating_sub(1))
            .max(1);
        let art_width = ((budget_height as f32 * cell_aspect).round() as u16)
            .min(area.width)
            .max(1);
        let art_height = ((art_width as f32 / cell_aspect).round() as u16)
            .max(1)
            .min(body_height.saturating_sub(1));
        let art_rect = Some(Rect {
            x: area.x,
            y,
            width: art_width,
            height: art_height,
        });
        let list_y = y + art_height + 1; // gap row between art and list
        let list_height = (area.y + area.height).saturating_sub(list_y);
        let list_rect = Rect {
            x: area.x,
            y: list_y,
            width: area.width,
            height: list_height,
        };
        return (art_rect, list_rect);
    }

    // Artist profile (two columns, no big-art path applies) or no art
    // at all: original side-by-side split. width wants to be
    // `cell_aspect`x height in cells to look square. Capping width
    // against `area.width / 3` without also shrinking height the same
    // way left the art rect itself non-square (full body_height,
    // clipped width); the rect handed to the widget has to be square
    // first, so height is always re-derived from whatever width
    // actually ends up being, never kept independently.
    let art_width = if has_art && body_height > 0 {
        ((body_height as f32 * cell_aspect).round() as u16)
            .min(area.width / 3)
            .max(1)
    } else {
        0
    };
    let art_height = ((art_width as f32 / cell_aspect).round() as u16)
        .max(1)
        .min(body_height);
    let art_rect = (art_width > 0).then_some(Rect {
        x: area.x,
        y,
        width: art_width,
        height: art_height,
    });
    let list_x = area.x + art_width + if art_width > 0 { 1 } else { 0 };
    let list_width = (area.x + area.width).saturating_sub(list_x);
    let list_rect = Rect {
        x: list_x,
        y,
        width: list_width,
        height: body_height,
    };
    (art_rect, list_rect)
}

pub struct BrowseView {
    pub title: String,
    /// Artist name for an album, empty for a playlist (playlists have
    /// no single artist).
    pub subtitle: String,
    /// Release year; album only, `getAlbumList2` doesn't carry it but
    /// `getAlbum` does. Always `None` for playlists.
    pub year: Option<u32>,
    pub tracks: Vec<SearchItem>,
    pub selected: usize,
    /// Decoded cover art, fetched once on open; `None` for playlists
    /// (no single piece of art) or if the fetch/decode failed, in
    /// which case the art panel just shows nothing rather than erroring
    /// the whole view.
    pub art: Option<image::DynamicImage>,
    /// Artist bio (navidrome's `getArtistInfo2`, proxied from
    /// last.fm); only ever set when this view was opened on an artist,
    /// `None` for albums/playlists.
    pub bio: Option<String>,
    /// Only set for artist views: every album credited to them,
    /// shown as its own column alongside the (filterable) song list
    /// rather than mixed into one flat list. Empty for album/playlist
    /// views, which keep the single-list layout they always had.
    pub is_artist: bool,
    pub albums: Vec<SearchItem>,
    pub albums_selected: usize,
    /// left/right switches which column navigation (j/k/enter/*)
    /// applies to; meaningless for album/playlist views (`is_artist`
    /// is false, there's only ever one column).
    pub albums_focused: bool,
    /// Narrows `tracks` down to whatever matches (plain case-
    /// insensitive substring on the row label, not the sidebar's full
    /// fuzzy search: this is "find a song in one artist's already-
    /// small catalog", not "search everything"). Empty matches all.
    pub filter_query: String,
    pub filtering: bool,
    /// Whether track rows repeat the artist name (`"title · artist"`)
    /// or just show the bare title. Only worth showing when a list can
    /// genuinely mix artists (a playlist, or the merged-across-everything
    /// starred-songs view); it's redundant noise on a single album or an
    /// artist profile's own songs column, where every row is obviously
    /// the same artist.
    pub show_track_artist: bool,
    /// Whether `Loader` still has fetches in flight for this view. The
    /// header says so and an empty pane reads "loading ..." rather
    /// than "(no tracks)", which is a lie while the answer is still on
    /// its way.
    pub loading: bool,
}

/// What a pane shows in place of rows: still fetching, filtered down
/// to nothing, or genuinely empty. Same vocabulary as
/// `main::loading_message` (lowercase, trailing ellipsis, short enough
/// for a narrow column); `what` names the rows, e.g. "tracks".
pub fn pane_message(loading: bool, filtered: bool, what: &str) -> String {
    if loading {
        format!("loading {what}...")
    } else if filtered {
        "(no matches)".to_string()
    } else {
        format!("(no {what})")
    }
}

/// The bio's own rect, directly below the header and above whatever
/// `layout` gives the art/track list; `None` when there's no bio to
/// show. Kept separate from `layout` itself since the bio doesn't
/// share a row with the art panel the way the track list does.
pub fn bio_rect(area: Rect, has_bio: bool) -> Option<Rect> {
    has_bio.then(|| Rect {
        x: area.x,
        y: area.y + HEADER_HEIGHT,
        width: area.width,
        height: BIO_HEIGHT - 1,
    })
}

/// Naive greedy word-wrap, no hyphenation; the last line gets an
/// ellipsis if there was more text than `max_lines` could hold, same
/// "truncate rather than overflow" rule as everywhere else in the ui.
/// Measured in display cells throughout (`textfit`), not bytes: a
/// cyrillic/CJK bio measured by `str::len` wraps at a third to a half
/// of the width it was given.
pub fn wrap_bio(text: &str, width: usize, max_lines: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut truncated = false;

    for word in text.split_whitespace() {
        if lines.len() >= max_lines {
            truncated = true;
            break;
        }
        let would_be = if current.is_empty() {
            textfit::width(word)
        } else {
            textfit::width(&current) + 1 + textfit::width(word)
        };
        if would_be > width && !current.is_empty() {
            lines.push(std::mem::take(&mut current));
            if lines.len() >= max_lines {
                truncated = true;
                break;
            }
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(word);
    }
    if !truncated && !current.is_empty() {
        lines.push(current);
    }

    if truncated {
        if let Some(last) = lines.last_mut() {
            // always re-fit, not only when it overflows: the '…' has
            // to fit inside `width` alongside whatever's kept.
            *last = textfit::truncate_to_width(last, width.saturating_sub(1));
            last.push('…');
        }
    }
    lines
}

impl BrowseView {
    /// An empty view already marked as fetching, for `Loader` to fill
    /// in; the caller overrides the title/subtitle it already knows
    /// from the `SearchItem` it was opened on.
    fn loading(is_artist: bool) -> BrowseView {
        BrowseView {
            title: String::new(),
            subtitle: String::new(),
            year: None,
            tracks: Vec::new(),
            selected: 0,
            art: None,
            bio: None,
            is_artist,
            albums: Vec::new(),
            albums_selected: 0,
            albums_focused: false,
            filter_query: String::new(),
            filtering: false,
            show_track_artist: false,
            loading: true,
        }
    }

    /// A stable name for whatever `art` this view holds, for the
    /// rasterizer's per-frame cache (`ui::raster::Memo`). A view is
    /// opened on one album or artist and keeps the single cover it
    /// fetched then, so the view's own identity *is* the cover's.
    /// Everything that could pick out a different record is in it,
    /// the year included, so it discriminates at least as finely as
    /// `coverart::Key` does.
    pub fn art_key(&self) -> String {
        let kind = if self.is_artist { "artist" } else { "album" };
        let year = self.year.unwrap_or(0);
        format!(
            "bv:{kind}\u{1}{}\u{1}{}\u{1}{year}",
            self.subtitle, self.title
        )
    }

    /// Indices into `tracks` that match `filter_query`: a plain
    /// case-insensitive substring match on the same label the row
    /// itself shows. Empty query matches everything, so this is
    /// always safe to use as "the currently visible tracks."
    pub fn visible_track_indices(&self) -> Vec<usize> {
        if self.filter_query.is_empty() {
            return (0..self.tracks.len()).collect();
        }
        let needle = self.filter_query.to_lowercase();
        self.tracks
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                crate::ui::results_list::label(item)
                    .to_lowercase()
                    .contains(&needle)
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Forces `selected` back inside `tracks` *and* onto a row the
    /// filter actually shows. Call after anything that mutates either
    /// one: a stale `selected` isn't visible as a stale cursor (the
    /// draw path falls back to row 0 when it can't find the selected
    /// index), so enter silently acts on a different track than the
    /// `>` marker points at.
    pub fn clamp_selection(&mut self) {
        if self.tracks.is_empty() {
            self.selected = 0;
            return;
        }
        self.selected = self.selected.min(self.tracks.len() - 1);
        let visible = self.visible_track_indices();
        if !visible.contains(&self.selected) {
            self.selected = visible.first().copied().unwrap_or(0);
        }
    }

    /// The one way to replace the track list after construction, so
    /// the `selected`-stays-in-`tracks` invariant can't be dropped by
    /// whoever assigns it next.
    pub fn set_tracks(&mut self, tracks: Vec<SearchItem>) {
        self.tracks = tracks;
        self.clamp_selection();
    }

    pub fn next(&mut self) {
        if self.albums_focused {
            if self.albums_selected + 1 < self.albums.len() {
                self.albums_selected += 1;
            }
            return;
        }
        let visible = self.visible_track_indices();
        match visible.iter().position(|&i| i == self.selected) {
            Some(pos) => {
                if let Some(&next_idx) = visible.get(pos + 1) {
                    self.selected = next_idx;
                }
            }
            None => self.selected = visible.first().copied().unwrap_or(0),
        }
    }

    pub fn prev(&mut self) {
        if self.albums_focused {
            self.albums_selected = self.albums_selected.saturating_sub(1);
            return;
        }
        let visible = self.visible_track_indices();
        match visible.iter().position(|&i| i == self.selected) {
            Some(pos) if pos > 0 => self.selected = visible[pos - 1],
            Some(_) => {}
            None => self.selected = visible.first().copied().unwrap_or(0),
        }
    }
}

/// Which open a reply belongs to. Every `Loader::open` bumps it, so a
/// reply tagged with an older generation is for a view the user has
/// since closed or replaced, and is dropped on arrival rather than
/// overwriting the newer open. See docs/async-open.md.
type Generation = u64;

/// One piece of an open that finished. An album or playlist answers in
/// a single round trip (`Tracks`); an artist sends `Albums` after its
/// first one and then an `AlbumTracks` per album, so the songs column
/// fills album by album instead of all at the end.
enum Load {
    Tracks {
        year: Option<u32>,
        tracks: Vec<SearchItem>,
    },
    Art(image::DynamicImage),
    Bio(Option<String>),
    Albums(Vec<SearchItem>),
    AlbumTracks {
        index: usize,
        tracks: Vec<SearchItem>,
    },
    /// Nothing more is coming for this open.
    Done,
    /// The open failed outright; the view stays up (empty) and the
    /// host toasts this, rather than no view appearing at all.
    Failed(String),
}

struct Reply {
    generation: Generation,
    load: Load,
}

/// A worker's end of the loader's channel, tagged with the open it is
/// fetching for. `send` reports false once that open is stale or the
/// app is gone, which is every worker's cue to stop: an artist is
/// dozens of round trips and none of them are worth finishing for a
/// view nobody is looking at any more.
struct ReplySender {
    tx: Sender<Reply>,
    current: Arc<AtomicU64>,
    generation: Generation,
}

impl ReplySender {
    fn send(&self, load: Load) -> bool {
        if self.current.load(Ordering::SeqCst) != self.generation {
            return false;
        }
        self.tx
            .send(Reply {
                generation: self.generation,
                load,
            })
            .is_ok()
    }
}

/// What a `poll` produced that only the host can act on.
#[derive(Default)]
pub struct Polled {
    /// Cover art that just landed. The `ratatui-image` protocol is
    /// built by the host because the `Picker` lives there, not here.
    pub art: Option<image::DynamicImage>,
    pub errors: Vec<String>,
}

/// Opens browse views without blocking the main thread. `open` returns
/// the view to show immediately (title and subtitle are already in the
/// `SearchItem`) and the network happens on worker threads that report
/// back over one channel, drained by `poll` each pass of the main loop;
/// same shape as `coverart::Cache`'s `request`/`poll`.
pub struct Loader {
    tx: Sender<Reply>,
    rx: Receiver<Reply>,
    /// The open every reply is checked against, shared with the workers
    /// so they can stop early too.
    current: Arc<AtomicU64>,
    /// Artist views only: one slot per album, filled as the stream
    /// answers (out of order). `tracks` is rebuilt from this in album
    /// order, so the list stays chronological however the fetches race.
    pending: Vec<Option<Vec<SearchItem>>>,
}

impl Default for Loader {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        Loader {
            tx,
            rx,
            current: Arc::new(AtomicU64::new(0)),
            pending: Vec::new(),
        }
    }
}

impl Loader {
    /// The view to show right now for `item`, with its fetches already
    /// running; `None` for anything that isn't an album, playlist or
    /// artist (those are one track, nothing to browse).
    pub fn open(&mut self, item: &SearchItem) -> Option<BrowseView> {
        let view = self.begin(item)?;
        self.fetch(item);
        Some(view)
    }

    /// Nothing still in flight may touch the browse view any more.
    /// Call this wherever `browse_view` is closed or replaced by a view
    /// the loader did not open (esc, the starred listing), or a reply
    /// from the previous open lands in a view it was never requested
    /// for.
    pub fn invalidate(&mut self) {
        self.current.fetch_add(1, Ordering::SeqCst);
        self.pending.clear();
    }

    /// Takes delivery of whatever finished since the last call and
    /// folds it into `view`. Replies for a stale generation are
    /// dropped, which is the whole stale-reply guard: a late answer can
    /// neither land in a closed view nor overwrite a newer open.
    pub fn poll(&mut self, view: &mut Option<BrowseView>) -> Polled {
        let mut polled = Polled::default();
        while let Ok(reply) = self.rx.try_recv() {
            if reply.generation != self.current.load(Ordering::SeqCst) {
                continue;
            }
            let Some(view) = view.as_mut() else { continue };
            self.apply(reply.load, view, &mut polled);
        }
        polled
    }

    fn apply(&mut self, load: Load, view: &mut BrowseView, polled: &mut Polled) {
        match load {
            Load::Tracks { year, tracks } => {
                view.year = year;
                view.set_tracks(tracks);
                // one round trip, so this list is already complete.
                view.loading = false;
            }
            Load::Art(image) => {
                polled.art = Some(image.clone());
                view.art = Some(image);
            }
            Load::Bio(bio) => view.bio = bio,
            Load::Albums(albums) => {
                self.pending = vec![None; albums.len()];
                view.albums = albums;
                view.albums_selected = view
                    .albums_selected
                    .min(view.albums.len().saturating_sub(1));
            }
            Load::AlbumTracks { index, tracks } => {
                if let Some(slot) = self.pending.get_mut(index) {
                    *slot = Some(tracks);
                }
                let so_far: Vec<SearchItem> =
                    self.pending.iter().flatten().flatten().cloned().collect();
                view.subtitle = song_count(so_far.len());
                // the cursor follows the song it is on, not its index:
                // albums land out of order, so an album that answers
                // late inserts rows *above* a selection the user has
                // already moved, and enter would act on another track.
                let anchor = view
                    .tracks
                    .get(view.selected)
                    .and_then(song_id)
                    .map(str::to_string);
                view.set_tracks(so_far);
                if let Some(anchor) = anchor {
                    if let Some(at) = view
                        .tracks
                        .iter()
                        .position(|track| song_id(track) == Some(anchor.as_str()))
                    {
                        view.selected = at;
                        view.clamp_selection();
                    }
                }
            }
            Load::Done => {
                view.loading = false;
                if view.is_artist {
                    view.subtitle = song_count(view.tracks.len());
                }
            }
            Load::Failed(msg) => {
                view.loading = false;
                polled.errors.push(msg);
            }
        }
    }

    /// Starts a new open: the generation moves, so every reply still in
    /// flight for the previous one is now stale. Split from `fetch` so
    /// tests can drive the two halves without a server.
    fn begin(&mut self, item: &SearchItem) -> Option<BrowseView> {
        self.invalidate();
        Some(match item {
            SearchItem::Album { name, artist, .. } => BrowseView {
                title: name.clone(),
                subtitle: artist.clone(),
                show_track_artist: false,
                ..BrowseView::loading(false)
            },
            SearchItem::Playlist { name, .. } => BrowseView {
                title: name.clone(),
                // playlists mix artists, so rows name theirs
                show_track_artist: true,
                ..BrowseView::loading(false)
            },
            SearchItem::Artist { name, .. } => BrowseView {
                title: name.clone(),
                ..BrowseView::loading(true)
            },
            _ => return None,
        })
    }

    fn fetch(&self, item: &SearchItem) {
        let sender = ReplySender {
            tx: self.tx.clone(),
            current: self.current.clone(),
            generation: self.current.load(Ordering::SeqCst),
        };
        match item {
            SearchItem::Album {
                id,
                name,
                artist,
                client,
            } => {
                let (id, name, artist, client) =
                    (id.clone(), name.clone(), artist.clone(), client.clone());
                thread::spawn(move || {
                    match client.get_album(&id) {
                        Ok(details) => {
                            let tracks = songs_to_items(details.songs, &client);
                            if !sender.send(Load::Tracks {
                                year: details.year,
                                tracks,
                            }) {
                                return;
                            }
                        }
                        Err(e) => {
                            sender.send(Load::Failed(format!("couldn't open album: {e}")));
                            return;
                        }
                    }
                    // a second round trip, so it rides behind the track
                    // list rather than holding it up.
                    if let Some(art) = fetch_cover_art(&client, &id, &artist, &name) {
                        sender.send(Load::Art(art));
                    }
                    sender.send(Load::Done);
                });
            }
            SearchItem::Playlist { id, client, .. } => {
                let (id, client) = (id.clone(), client.clone());
                thread::spawn(move || {
                    match client.get_playlist_songs(&id) {
                        Ok(songs) => {
                            sender.send(Load::Tracks {
                                year: None,
                                tracks: songs_to_items(songs, &client),
                            });
                        }
                        Err(e) => {
                            sender.send(Load::Failed(format!("couldn't open playlist: {e}")));
                        }
                    }
                    sender.send(Load::Done);
                });
            }
            SearchItem::Artist { id, name, client } => {
                let (id, name, client) = (id.clone(), name.clone(), client.clone());
                thread::spawn(move || stream_artist(&sender, &client, &id, &name));
            }
            _ => {}
        }
    }

    #[cfg(test)]
    fn generation(&self) -> Generation {
        self.current.load(Ordering::SeqCst)
    }

    /// Delivers a reply as if a worker had sent it, so the stale guard
    /// and the per-album assembly are testable without a server; same
    /// trick `coverart::Cache::deliver` uses.
    #[cfg(test)]
    fn deliver(&mut self, generation: Generation, load: Load, view: &mut Option<BrowseView>) {
        self.tx.send(Reply { generation, load }).unwrap();
        let _ = self.poll(view);
    }
}

/// The artist profile: `getArtist` first (the ALBUMS column renders off
/// that alone), then one `getAlbum` per album streamed back as it
/// lands, plus the bio. Only songs credited to this artist are kept;
/// see `main::artist_tracks` for the Global/Various-Artists reasoning.
fn stream_artist(sender: &ReplySender, client: &Arc<Client>, id: &str, name: &str) {
    let (stream_tx, stream_rx) = mpsc::channel();
    let albums = match client.stream_artist_tracks(id, stream_tx) {
        Ok(albums) => albums,
        Err(e) => {
            sender.send(Load::Failed(format!("couldn't load artist: {e}")));
            return;
        }
    };
    let items: Vec<SearchItem> = albums
        .iter()
        .map(|album| SearchItem::Album {
            id: album.id.clone(),
            name: album.name.clone(),
            artist: name.to_string(),
            client: client.clone(),
        })
        .collect();
    if !sender.send(Load::Albums(items)) {
        return;
    }
    for event in stream_rx {
        let delivered = match event {
            ArtistLoad::Bio(bio) => sender.send(Load::Bio(bio)),
            ArtistLoad::Album(album) => {
                let tracks = songs_to_items(
                    album
                        .songs
                        .into_iter()
                        .filter(|song| song.artist == name)
                        .collect(),
                    client,
                );
                sender.send(Load::AlbumTracks {
                    index: album.index,
                    tracks,
                })
            }
        };
        // dropping `stream_rx` here is what stops the album fetches
        // still queued behind this one.
        if !delivered {
            return;
        }
    }
    sender.send(Load::Done);
}

fn song_id(item: &SearchItem) -> Option<&str> {
    match item {
        SearchItem::Song { id, .. } => Some(id),
        _ => None,
    }
}

fn song_count(n: usize) -> String {
    if n == 1 {
        "1 song".to_string()
    } else {
        format!("{n} songs")
    }
}

/// An opened album's cover. Goes through the shared on-disk store, so
/// reopening an album costs nothing and a cover the player already
/// fetched for one of its tracks is a hit here (and the reverse).
///
/// Keyed by the album id first, which is what the player's own key
/// prefers too (`coverart::Key::of`); the names are the fallback for a
/// track whose album id nothing has reported, so both sides have to
/// try them in the same order or the two stop sharing.
fn fetch_cover_art(
    client: &Client,
    album_id: &str,
    artist: &str,
    album: &str,
) -> Option<image::DynamicImage> {
    let key = crate::coverart::Key::album_id(album_id)
        .or_else(|| crate::coverart::Key::album(artist, album))
        .unwrap_or_else(|| crate::coverart::Key::Track(format!("album:{album_id}")));
    crate::coverart::fetch_stored(client, &key, album_id)
}

pub fn songs_to_items(songs: Vec<crate::subsonic::Song>, client: &Arc<Client>) -> Vec<SearchItem> {
    songs
        .into_iter()
        .map(|s| SearchItem::Song {
            id: s.id,
            title: s.title,
            artist: s.artist,
            album: s.album,
            client: client.clone(),
        })
        .collect()
}

// `pub(crate)` purely so `ui::browse`'s own render tests can reuse
// these fixtures instead of keeping a second copy of them.
#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A client that never talks to anything; remote `SearchItem`s all
    /// carry one, and building it is offline (no ping on construct).
    pub(crate) fn test_client() -> Arc<Client> {
        Arc::new(
            Client::new(&crate::config::NavidromeConfig {
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

    pub(crate) fn test_songs(n: usize) -> Vec<SearchItem> {
        let client = test_client();
        (0..n)
            .map(|i| SearchItem::Song {
                id: format!("song-{i}"),
                title: format!("song {i}"),
                artist: "artist".to_string(),
                album: "album".to_string(),
                client: client.clone(),
            })
            .collect()
    }

    pub(crate) fn test_view(tracks: Vec<SearchItem>) -> BrowseView {
        BrowseView {
            title: "album".to_string(),
            subtitle: String::new(),
            year: None,
            tracks,
            selected: 0,
            art: None,
            bio: None,
            is_artist: false,
            albums: Vec::new(),
            albums_selected: 0,
            albums_focused: false,
            filter_query: String::new(),
            filtering: false,
            show_track_artist: false,
            loading: false,
        }
    }

    #[test]
    fn set_tracks_clamps_selection_into_the_shorter_list() {
        let mut view = test_view(test_songs(5));
        view.selected = 4;
        view.set_tracks(test_songs(3));
        assert_eq!(
            view.selected, 2,
            "selected must stay a valid index into tracks"
        );
    }

    #[test]
    fn set_tracks_resets_selection_when_the_list_empties() {
        let mut view = test_view(test_songs(5));
        view.selected = 3;
        view.set_tracks(Vec::new());
        assert_eq!(view.selected, 0);
    }

    #[test]
    fn clamp_selection_snaps_onto_a_still_visible_filtered_row() {
        let mut view = test_view(test_songs(5));
        view.filter_query = "song 3".to_string();
        view.clamp_selection();
        assert_eq!(view.selected, 3);
    }

    #[test]
    fn clamp_selection_fixes_an_out_of_bounds_index() {
        let mut view = test_view(test_songs(2));
        view.selected = 99;
        view.clamp_selection();
        assert_eq!(view.selected, 1);
    }

    fn test_album(name: &str) -> SearchItem {
        SearchItem::Album {
            id: format!("al-{name}"),
            name: name.to_string(),
            artist: "artist".to_string(),
            client: test_client(),
        }
    }

    fn test_artist() -> SearchItem {
        SearchItem::Artist {
            id: "ar-1".to_string(),
            name: "artist".to_string(),
            client: test_client(),
        }
    }

    fn songs_named(names: &[&str]) -> Vec<SearchItem> {
        let client = test_client();
        names
            .iter()
            .map(|name| SearchItem::Song {
                id: (*name).to_string(),
                title: (*name).to_string(),
                artist: "artist".to_string(),
                album: "album".to_string(),
                client: client.clone(),
            })
            .collect()
    }

    fn titles(view: &BrowseView) -> Vec<String> {
        view.tracks
            .iter()
            .map(|track| match track {
                SearchItem::Song { title, .. } => title.clone(),
                _ => String::new(),
            })
            .collect()
    }

    /// `begin` rather than `open`: the view it hands back is the whole
    /// point (no network has happened yet), and tests never talk to a
    /// server.
    #[test]
    fn an_album_view_has_its_title_before_a_single_track_arrives() {
        let mut loader = Loader::default();
        let view = loader.begin(&test_album("kid a")).unwrap();
        assert_eq!(view.title, "kid a");
        assert_eq!(view.subtitle, "artist", "the item already carried it");
        assert!(view.loading, "and it says it is still fetching");
        assert!(view.tracks.is_empty());
    }

    #[test]
    fn an_artist_view_opens_on_its_name_with_both_columns_empty() {
        let mut loader = Loader::default();
        let view = loader.begin(&test_artist()).unwrap();
        assert_eq!(view.title, "artist");
        assert!(view.is_artist);
        assert!(view.loading);
        assert!(view.albums.is_empty() && view.tracks.is_empty());
    }

    #[test]
    fn a_song_has_nothing_to_browse() {
        let mut loader = Loader::default();
        assert!(loader.begin(&songs_named(&["one"])[0]).is_none());
    }

    #[test]
    fn tracks_land_in_the_view_they_were_requested_for() {
        let mut loader = Loader::default();
        let mut view = Some(loader.begin(&test_album("kid a")).unwrap());
        let generation = loader.generation();
        loader.deliver(
            generation,
            Load::Tracks {
                year: Some(2000),
                tracks: songs_named(&["everything in its right place", "kid a"]),
            },
            &mut view,
        );
        let view = view.unwrap();
        assert_eq!(view.tracks.len(), 2);
        assert_eq!(view.year, Some(2000));
        assert!(!view.loading, "one round trip, so the list is complete");
    }

    /// The fast-clicking case: open one album, open another before the
    /// first answers, and the first answer must not land in the second
    /// view. Remove the generation check in `Loader::poll` and this
    /// fails.
    #[test]
    fn a_reply_from_the_previous_open_never_lands_in_the_newer_view() {
        let mut loader = Loader::default();
        let _stale_view = loader.begin(&test_album("kid a")).unwrap();
        let stale = loader.generation();

        let mut view = Some(loader.begin(&test_album("amnesiac")).unwrap());
        loader.deliver(
            stale,
            Load::Tracks {
                year: Some(2000),
                tracks: songs_named(&["kid a"]),
            },
            &mut view,
        );
        let view = view.unwrap();
        assert_eq!(view.title, "amnesiac");
        assert!(
            view.tracks.is_empty() && view.year.is_none(),
            "the first album's answer overwrote the second's view"
        );
        assert!(view.loading, "and left it claiming it had finished");
    }

    /// Same album twice is still two different opens; nothing is keyed
    /// by album id.
    #[test]
    fn reopening_the_same_album_still_discards_the_first_opens_reply() {
        let mut loader = Loader::default();
        loader.begin(&test_album("kid a")).unwrap();
        let first = loader.generation();
        let mut view = Some(loader.begin(&test_album("kid a")).unwrap());
        loader.deliver(
            first,
            Load::Tracks {
                year: None,
                tracks: songs_named(&["kid a"]),
            },
            &mut view,
        );
        assert!(view.unwrap().tracks.is_empty());
    }

    #[test]
    fn a_reply_after_the_view_was_closed_reopens_nothing() {
        let mut loader = Loader::default();
        let view = loader.begin(&test_album("kid a")).unwrap();
        let generation = loader.generation();
        // esc: the host drops the view and disowns the fetch.
        drop(view);
        let mut closed: Option<BrowseView> = None;
        loader.invalidate();
        loader.deliver(
            generation,
            Load::Tracks {
                year: None,
                tracks: songs_named(&["kid a"]),
            },
            &mut closed,
        );
        assert!(closed.is_none());
    }

    /// `invalidate` is also what the starred listing calls, since the
    /// loader did not open that view.
    #[test]
    fn a_reply_never_lands_in_a_view_the_loader_did_not_open() {
        let mut loader = Loader::default();
        loader.begin(&test_album("kid a")).unwrap();
        let generation = loader.generation();
        let mut view = Some(test_view(Vec::new()));
        loader.invalidate();
        loader.deliver(
            generation,
            Load::Tracks {
                year: None,
                tracks: songs_named(&["kid a"]),
            },
            &mut view,
        );
        assert!(
            view.unwrap().tracks.is_empty(),
            "an album's tracks landed in the starred listing"
        );
    }

    #[test]
    fn the_open_after_an_invalidate_still_gets_its_own_replies() {
        let mut loader = Loader::default();
        loader.invalidate();
        let mut view = Some(loader.begin(&test_album("kid a")).unwrap());
        let generation = loader.generation();
        loader.deliver(
            generation,
            Load::Tracks {
                year: None,
                tracks: songs_named(&["kid a"]),
            },
            &mut view,
        );
        assert_eq!(view.unwrap().tracks.len(), 1, "the gate stayed shut");
    }

    #[test]
    fn an_artist_songs_column_stays_in_album_order_however_the_fetches_race() {
        let mut loader = Loader::default();
        let mut view = Some(loader.begin(&test_artist()).unwrap());
        let generation = loader.generation();
        loader.deliver(
            generation,
            Load::Albums(vec![
                test_album("first"),
                test_album("second"),
                test_album("third"),
            ]),
            &mut view,
        );
        // the last album answers first, the middle one never answers
        loader.deliver(
            generation,
            Load::AlbumTracks {
                index: 2,
                tracks: songs_named(&["c1", "c2"]),
            },
            &mut view,
        );
        assert_eq!(titles(view.as_ref().unwrap()), ["c1", "c2"]);
        loader.deliver(
            generation,
            Load::AlbumTracks {
                index: 0,
                tracks: songs_named(&["a1"]),
            },
            &mut view,
        );
        let view = view.unwrap();
        assert_eq!(
            titles(&view),
            ["a1", "c1", "c2"],
            "tracks must read in album order, not arrival order"
        );
        assert_eq!(view.albums.len(), 3, "the column filled on one trip");
        assert_eq!(view.subtitle, "3 songs");
        assert!(view.loading, "the middle album is still coming");
    }

    /// The cursor must stay on the song it was parked on, even when a
    /// late album inserts rows above it.
    #[test]
    fn a_late_album_does_not_drag_the_cursor_onto_another_song() {
        let mut loader = Loader::default();
        let mut view = Some(loader.begin(&test_artist()).unwrap());
        let generation = loader.generation();
        loader.deliver(
            generation,
            Load::Albums(vec![test_album("first"), test_album("second")]),
            &mut view,
        );
        loader.deliver(
            generation,
            Load::AlbumTracks {
                index: 1,
                tracks: songs_named(&["b1", "b2"]),
            },
            &mut view,
        );
        view.as_mut().unwrap().selected = 1;
        loader.deliver(
            generation,
            Load::AlbumTracks {
                index: 0,
                tracks: songs_named(&["a1", "a2", "a3"]),
            },
            &mut view,
        );
        let view = view.unwrap();
        assert_eq!(titles(&view), ["a1", "a2", "a3", "b1", "b2"]);
        assert_eq!(
            titles(&view)[view.selected],
            "b2",
            "the cursor moved to a different song under the user"
        );
    }

    #[test]
    fn an_artists_albums_arrive_before_any_of_its_songs() {
        let mut loader = Loader::default();
        let mut view = Some(loader.begin(&test_artist()).unwrap());
        let generation = loader.generation();
        loader.deliver(
            generation,
            Load::Albums(vec![test_album("first")]),
            &mut view,
        );
        let view = view.unwrap();
        assert_eq!(view.albums.len(), 1);
        assert!(view.tracks.is_empty());
        assert!(view.loading);
    }

    #[test]
    fn done_clears_loading_and_fixes_the_final_count() {
        let mut loader = Loader::default();
        let mut view = Some(loader.begin(&test_artist()).unwrap());
        let generation = loader.generation();
        loader.deliver(
            generation,
            Load::Albums(vec![test_album("first")]),
            &mut view,
        );
        loader.deliver(
            generation,
            Load::AlbumTracks {
                index: 0,
                tracks: songs_named(&["a1"]),
            },
            &mut view,
        );
        loader.deliver(generation, Load::Done, &mut view);
        let view = view.unwrap();
        assert!(!view.loading);
        assert_eq!(view.subtitle, "1 song");
    }

    #[test]
    fn a_failed_open_leaves_the_view_up_and_reports_the_reason() {
        let mut loader = Loader::default();
        let mut view = Some(loader.begin(&test_album("kid a")).unwrap());
        let generation = loader.generation();
        loader
            .tx
            .send(Reply {
                generation,
                load: Load::Failed("couldn't open album: nope".to_string()),
            })
            .unwrap();
        let polled = loader.poll(&mut view);
        assert_eq!(polled.errors, ["couldn't open album: nope"]);
        let view = view.unwrap();
        assert_eq!(view.title, "kid a", "the view stays up, empty");
        assert!(!view.loading);
    }

    #[test]
    fn art_is_handed_to_the_host_as_well_as_the_view() {
        let mut loader = Loader::default();
        let mut view = Some(loader.begin(&test_album("kid a")).unwrap());
        let generation = loader.generation();
        let image = image::DynamicImage::new_rgb8(2, 2);
        loader
            .tx
            .send(Reply {
                generation,
                load: Load::Art(image),
            })
            .unwrap();
        let polled = loader.poll(&mut view);
        assert!(polled.art.is_some(), "the host builds the image protocol");
        assert!(view.unwrap().art.is_some());
    }

    #[test]
    fn a_stale_cover_is_dropped_before_the_host_ever_sees_it() {
        let mut loader = Loader::default();
        loader.begin(&test_album("kid a")).unwrap();
        let stale = loader.generation();
        let mut view = Some(loader.begin(&test_album("amnesiac")).unwrap());
        loader
            .tx
            .send(Reply {
                generation: stale,
                load: Load::Art(image::DynamicImage::new_rgb8(2, 2)),
            })
            .unwrap();
        let polled = loader.poll(&mut view);
        assert!(polled.art.is_none(), "wrong album's cover");
        assert!(view.unwrap().art.is_none());
    }

    #[test]
    fn a_pane_says_it_is_loading_rather_than_claiming_to_be_empty() {
        assert_eq!(pane_message(true, false, "tracks"), "loading tracks...");
        assert_eq!(pane_message(true, false, "albums"), "loading albums...");
        assert_eq!(pane_message(false, false, "tracks"), "(no tracks)");
        assert_eq!(pane_message(false, true, "tracks"), "(no matches)");
        // a filter typed while the fetch is still running: still
        // loading is the honest answer, there is more to match against.
        assert_eq!(pane_message(true, true, "tracks"), "loading tracks...");
    }

    #[test]
    fn loading_messages_fit_a_narrow_column() {
        for what in ["tracks", "albums"] {
            for (loading, filtered) in [(true, false), (false, false), (false, true)] {
                let msg = pane_message(loading, filtered, what);
                assert!(textfit::width(&msg) <= 20, "too wide for a column: {msg:?}");
            }
        }
    }

    #[test]
    fn wrap_bio_fits_short_text_on_one_line() {
        let lines = wrap_bio("a short bio", 40, 3);
        assert_eq!(lines, vec!["a short bio".to_string()]);
    }

    #[test]
    fn wrap_bio_truncates_with_ellipsis_past_max_lines() {
        let text = "one two three four five six seven eight nine ten eleven twelve";
        let lines = wrap_bio(text, 10, 2);
        assert_eq!(lines.len(), 2);
        assert!(
            lines[1].ends_with('…'),
            "last line should end truncated: {:?}",
            lines[1]
        );
    }

    #[test]
    fn wrap_bio_handles_empty_text() {
        assert!(wrap_bio("", 40, 3).is_empty());
    }

    #[test]
    fn wrap_bio_measures_cells_not_bytes() {
        // five cyrillic words, 2 bytes per char: by `str::len` this
        // would wrap after the first word, by display width all five
        // fit on one 30-cell line.
        let lines = wrap_bio("гнев это дар судьбы наш", 30, 3);
        assert_eq!(lines, vec!["гнев это дар судьбы наш".to_string()]);
    }

    #[test]
    fn wrap_bio_keeps_wide_characters_inside_the_width() {
        // each cjk char is two cells, so three words of three chars
        // can't share an 8-cell line.
        let lines = wrap_bio("日本語 音楽家 東京都", 8, 3);
        assert_eq!(lines.len(), 3);
        for line in &lines {
            assert!(
                textfit::width(line) <= 8,
                "line wider than the pane: {line:?}"
            );
        }
    }

    /// The four modes plus off, in order, and back to the start.
    #[test]
    fn the_cycle_visits_all_four_modes_and_off() {
        let mut seen = vec![ArtMode::AsciiBw];
        let mut m = ArtMode::AsciiBw;
        for _ in 0..5 {
            let (next, skipped) = m.cycle(true);
            assert!(!skipped, "nothing to skip on a terminal with a protocol");
            m = next;
            seen.push(m);
        }
        assert_eq!(
            seen,
            vec![
                ArtMode::AsciiBw,
                ArtMode::AsciiColor,
                ArtMode::FullBw,
                ArtMode::FullColor,
                ArtMode::Off,
                ArtMode::AsciiBw,
            ]
        );
    }

    /// With no graphics protocol the cycle is ascii-bw, ascii-color,
    /// off: never a full mode (which would draw nothing at all), and
    /// `off` still reachable.
    #[test]
    fn the_cycle_skips_the_full_modes_with_no_graphics_protocol() {
        let (after_color, skipped) = ArtMode::AsciiColor.cycle(false);
        assert_eq!(after_color, ArtMode::Off);
        assert!(skipped, "the host has to be able to say why");

        let mut m = ArtMode::AsciiBw;
        for _ in 0..9 {
            m = m.cycle(false).0;
            assert!(!m.is_full(), "cycled into an unrenderable mode: {m:?}");
        }
        assert!(
            ArtMode::AsciiBw.cycle(false).0 == ArtMode::AsciiColor
                && !ArtMode::AsciiBw.cycle(false).1
        );
    }

    /// A full mode that cannot render falls back to the *matching*
    /// ascii mode, so bw stays bw and colour stays colour.
    #[test]
    fn a_full_mode_degrades_to_its_own_ascii_half() {
        assert_eq!(ArtMode::FullBw.without_graphics(), ArtMode::AsciiBw);
        assert_eq!(ArtMode::FullColor.without_graphics(), ArtMode::AsciiColor);
        for m in [ArtMode::AsciiBw, ArtMode::AsciiColor, ArtMode::Off] {
            assert_eq!(m.without_graphics(), m, "{m:?} needs no fallback");
        }
    }

    /// Only `ascii-color` rasterizes in colour; `full-color` is the
    /// graphics protocol's own true colour, not the rasterizer's.
    #[test]
    fn only_ascii_color_asks_the_rasterizer_for_colour() {
        assert_eq!(ArtMode::AsciiColor.tone(), Tone::Color);
        for m in [
            ArtMode::AsciiBw,
            ArtMode::FullBw,
            ArtMode::FullColor,
            ArtMode::Off,
        ] {
            assert_eq!(m.tone(), Tone::Mono, "{m:?} must not tint the rasterizer");
        }
    }

    #[test]
    fn wrap_bio_truncation_fits_the_ellipsis_in_cells() {
        let text = "日本語 音楽家 東京都 作曲家 演奏者";
        let lines = wrap_bio(text, 8, 2);
        assert_eq!(lines.len(), 2);
        assert!(lines[1].ends_with('…'));
        assert!(
            textfit::width(&lines[1]) <= 8,
            "truncated line wider than the pane: {:?}",
            lines[1]
        );
    }
}
