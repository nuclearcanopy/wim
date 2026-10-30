//! Track-listing view opened on an album or playlist. Opening is a
//! separate action from playing: picking an album/playlist from search
//! shows its tracks here first ("let me scroll through the songs"),
//! rather than jumping straight into playback. Closing (esc) always
//! returns `main_content` to whatever it'd otherwise show (now-playing,
//! lyrics, or the idle banner/logo).

use std::sync::Arc;

use ratatui::layout::Rect;

use crate::search::SearchItem;
use crate::subsonic::Client;
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
