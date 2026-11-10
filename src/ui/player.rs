
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::app::LoopMode;
use crate::browse::ArtMode;
use crate::keymap::{key_label, KeyAction, Keymap};
use crate::theme::{shade, NEAR_BLACK, NEAR_WHITE};
use crate::ui::albumart::CachedArt;
use crate::ui::draw;
use crate::ui::gap::{Gap, GapPanel};
use crate::ui::textfit::{truncate_with_ellipsis, width};

/// Rows at the bottom of the pane the spectrum owns (main.rs draws
/// it over whatever was there, on the same `main_content.height > 2`
/// condition mirrored below), so the stack stops above them.
const METER_ROWS: u16 = 2;

/// The cover is only drawn once it can be at least this big. Smaller
/// than this it is unreadable mush and the rows are worth more to the
/// stack, so a short pane degrades to title and context only.
const ART_MIN_ROWS: u16 = 6;
const ART_MIN_COLS: u16 = 14;

/// Share of the pane the cover may take, same budget rule as
/// `browse::layout`'s big-art path.
const ART_SHARE: f32 = 0.6;

/// Where every piece of the now-playing screen goes. Returned by
/// `layout` rather than decided inside `render` so the host can put a
/// `ratatui-image` overlay on exactly the `art` rect this picked, same
/// contract `browse::layout` has with `BrowseListView`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlayerLayout {
    /// Square-ish cover panel, centred horizontally; `None` when the
    /// pane cannot give it `ART_MIN_ROWS`/`ART_MIN_COLS`.
    pub art: Option<Rect>,
    pub title_y: u16,
    /// Previous/next track line; the first thing a shrinking pane
    /// drops, since the queue view answers the same question.
    pub context_y: Option<u16>,
}

/// `None` when nothing fits at all (a zero-width or zero-height pane).
pub fn layout(
    area: Rect,
    has_art: bool,
    has_context: bool,
    cell_aspect: f32,
) -> Option<PlayerLayout> {
    if area.width == 0 || area.height == 0 {
        return None;
    }
    // main.rs only draws the spectrum when the pane is taller than the
    // visualizer itself, so below that there is nothing to reserve.
    let body_h = if area.height > METER_ROWS {
        area.height - METER_ROWS
    } else {
        area.height
    };

    // title and the prev/next line; the transport itself is the bottom
    // bar's (one transport, one cursor — the duplicated controls row
    // this pane used to draw is gone).
    let context = has_context && body_h >= 2;
    let stack_h = 1 + u16::from(context);

    let remaining = body_h.saturating_sub(stack_h);
    let mut art_rect = None;
    if has_art && remaining > 1 && area.width >= ART_MIN_COLS {
        // one row of the leftovers is the gap between cover and title.
        let budget_h = (remaining - 1).min((body_h as f32 * ART_SHARE).round() as u16);
        // square first: width is what gets capped against the pane and
        // height is always re-derived from whatever width survived that
        // cap (the `min` is only a rounding guard), never carried as
        // its own number; that is the bug `browse::layout` warns about.
        let max_w = area.width.saturating_sub(2).max(1);
        let w = ((budget_h as f32 * cell_aspect).round() as u16).clamp(1, max_w);
        let h = ((w as f32 / cell_aspect).round() as u16).clamp(1, budget_h.max(1));
        if h >= ART_MIN_ROWS && w >= ART_MIN_COLS {
            art_rect = Some(Rect {
                x: area.x + (area.width - w) / 2,
                y: 0,
                width: w,
                height: h,
            });
        }
    }

    let content_h = stack_h + art_rect.map_or(0, |r: Rect| r.height + 1);
    let mut y = area.y + body_h.saturating_sub(content_h) / 2;
    let art = art_rect.map(|r| {
        let placed = Rect { y, ..r };
        y += r.height + 1;
        placed
    });
    let title_y = y;
    y += 1;
    let context_y = context.then(|| {
        let row = y;
        y += 1;
        row
    });
    Some(PlayerLayout {
        art,
        title_y,
        context_y,
    })
}

/// The full "maximized" now-playing screen: takes over `main_content`
/// entirely, same as the banner/browse view it replaces. Minimizing it
/// back down to `MiniPlayerBar` is `KeyAction::ToggleMinimize`; every
/// key hinted on this screen is remappable, so all of them are
/// resolved from `keymap` rather than printed as their default.
/// Its transport is the bottom bar's own `controlsbar::Field` set,
/// drawn bigger and with the words spelled out: one cursor alphabet,
/// one scrub mode, one enter dispatch in main.rs for both surfaces.
pub struct PlayerView<'a> {
    pub title: &'a str,
    /// Cover for the track that is *playing*, which is not necessarily
    /// the one an open `BrowseView` holds art for; the host decides
    /// that (and fetches it) and passes `None` when it has none.
    pub art: Option<&'a image::DynamicImage>,
    /// Names whatever cover `art` is, for the rasterizer's per-frame
    /// cache; `coverart::art_key` of the playing track, the same name
    /// the host rebuilds its image protocol on.
    pub art_key: &'a str,
    /// A `Full*` mode leaves the cover panel to the host's image
    /// overlay, which goes on the `PlayerLayout::art` rect this view
    /// used; same contract as `BrowseListView`.
    pub art_mode: ArtMode,
    /// `Some` while a timed silence is playing, which has no cover and
    /// never will. The cover rect goes to a countdown instead of being
    /// left blank for the length of the gap; see `ui::gap`.
    pub gap: Option<Gap>,
    /// The terminal's real cell height:width ratio
    /// (`main::cell_aspect`), threaded to `layout` and `CachedArt` so
    /// both agree on what "square" means here.
    pub cell_aspect: f32,
    /// Mid-drag this is the pending scrub target rather than mpv's own
    /// position, same as the bottom bar.
    pub elapsed_secs: Option<f64>,
    pub duration_secs: Option<f64>,
    pub paused: bool,
    /// Seconds left of a between-track gap; see
    /// `ControlsBar::gap_remaining` for why this reads here rather
    /// than taking the cover panel.
    pub gap_remaining: Option<f64>,
    pub loop_mode: LoopMode,
    pub shuffle_on: bool,
    pub speed: f64,
    pub eq_preset: &'a str,
    pub crossfade_on: bool,
    pub previous: Option<&'a str>,
    pub next: Option<&'a str>,
    pub keymap: &'a Keymap,
}

impl Widget for PlayerView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // a gap claims the cover rect the same way a cover does, so
        // the layout reserves it. `ArtMode::Off` still means off: a
        // user who turned the panel off did not ask for a big clock.
        let has_art = (self.art.is_some() || self.gap.is_some()) && self.art_mode != ArtMode::Off;
        let Some(l) = layout(
            area,
            has_art,
            self.previous.is_some() || self.next.is_some(),
            self.cell_aspect,
        ) else {
            return;
        };

        if let Some(rect) = l.art {
            // checked before the cover, not after: a silence has no
            // art, so the two cannot both apply, and ordering it this
            // way means a future entry kind with both still draws
            // something deterministic. drawn in every art mode,
            // including the `Full*` ones, because the host has no
            // image to overlay for a gap.
            if let Some(gap) = self.gap {
                GapPanel { gap }.render(rect, buf);
            } else if self.art_mode.is_ascii() {
                CachedArt {
                    image: self.art,
                    cell_aspect: self.cell_aspect,
                    key: self.art_key,
                    tone: self.art_mode.tone(),
                }
                .render(rect, buf);
            }
        }

        draw_centered(buf, area, l.title_y, self.title, shade(NEAR_WHITE));
        if let Some(y) = l.context_y {
            draw_centered(buf, area, y, &self.context_line(), shade(NEAR_BLACK));
        }
    }
}

impl PlayerView<'_> {
    fn key(&self, action: KeyAction) -> String {
        key_label(self.keymap.get(action))
    }

    /// Previous and next track, each tagged with the key that actually
    /// goes there: on a screen whose complaint was "i can't tell how to
    /// control it", the two labels are also the two hints.
    fn context_line(&self) -> String {
        // pushed, not formatted and joined, for the reason `elements`
        // states: this is on the frame and none of it is a number.
        let mut out = String::new();
        if let Some(prev) = self.previous {
            out.push_str(&self.key(KeyAction::SkipBack));
            out.push_str(" ◂ ");
            out.push_str(prev);
        }
        if let Some(next) = self.next {
            if !out.is_empty() {
                out.push_str("     ");
            }
            out.push_str(next);
            out.push_str(" ▸ ");
            out.push_str(&self.key(KeyAction::SkipNext));
        }
        out
    }
}

fn draw_centered(buf: &mut Buffer, area: Rect, y: u16, text: &str, color: ratatui::style::Color) {
    if y < area.y || y >= area.y + area.height {
        return;
    }
    // centred on display width, not char count: a wide-char title
    // used to drift right by its own overhang.
    //
    // the pad is worked out here rather than by `draw::centered`,
    // which would fit the text a second time (it cannot know this one
    // already fits) and measure it a third.
    let fitted = truncate_with_ellipsis(text, area.width as usize);
    let pad = (area.width as usize).saturating_sub(width(&fitted)) / 2;
    draw::at(
        buf,
        area.x + pad as u16,
        y,
        &fitted,
        color,
        area.width as usize - pad,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::controlsbar::GLYPH_PLAY;
    use crate::ui::testbuf::{buffer, grid};
    use image::DynamicImage;

    fn art() -> DynamicImage {
        DynamicImage::ImageRgb8(image::RgbImage::from_fn(8, 8, |x, _| {
            image::Rgb([(x * 30) as u8, (x * 30) as u8, (x * 30) as u8])
        }))
    }

    fn view<'a>(keymap: &'a Keymap, art: Option<&'a DynamicImage>) -> PlayerView<'a> {
        PlayerView {
            title: "a song",
            art,
            art_key: "a:artist\u{1}album",
            art_mode: ArtMode::AsciiBw,
            gap: None,
            cell_aspect: 2.0,
            elapsed_secs: Some(30.0),
            duration_secs: Some(120.0),
            paused: false,
            gap_remaining: None,
            loop_mode: LoopMode::Off,
            shuffle_on: false,
            speed: 1.0,
            eq_preset: "flat",
            crossfade_on: false,
            previous: None,
            next: None,
            keymap,
        }
    }

    fn render(w: u16, h: u16, f: impl FnOnce(&mut PlayerView)) -> Buffer {
        let keymap = Keymap::default();
        let cover = art();
        let mut v = view(&keymap, Some(&cover));
        f(&mut v);
        let area = Rect::new(0, 0, w, h);
        let mut buf = buffer(w, h);
        v.render(area, &mut buf);
        buf
    }

    /// A gap has no cover and never will, so it takes the cover rect
    /// rather than leaving the biggest thing on screen blank for
    /// fifteen minutes.
    #[test]
    fn a_gap_puts_a_countdown_where_the_cover_goes() {
        let buf = render(60, 24, |v| {
            v.art = None;
            v.gap = Some(Gap {
                remaining_secs: 900.0,
            });
        });
        let text = grid(&buf);
        assert!(text.contains("remaining"), "{text}");
        assert!(
            text.contains('\u{2588}'),
            "the countdown is drawn large:\n{text}"
        );
    }

    /// And only a gap does: an ordinary track with no art shows no
    /// clock, which would be a bizarre thing to invent for it.
    #[test]
    fn a_track_with_no_cover_gets_no_countdown() {
        let buf = render(60, 24, |v| {
            v.art = None;
            v.gap = None;
        });
        assert!(!grid(&buf).contains("remaining"));
    }

    /// `art = off` means the panel is off. A user who turned the cover
    /// panel off did not ask to be given a big clock instead.
    #[test]
    fn art_off_suppresses_the_countdown_too() {
        let buf = render(60, 24, |v| {
            v.art = None;
            v.art_mode = ArtMode::Off;
            v.gap = Some(Gap {
                remaining_secs: 900.0,
            });
        });
        assert!(!grid(&buf).contains("remaining"));
    }

    /// The `Full*` modes leave the cover rect to the host's image
    /// overlay, but there is no image for a gap, so the countdown has
    /// to draw itself in those modes as well or the rect stays blank.
    #[test]
    fn a_gap_draws_in_the_full_art_modes_as_well() {
        for mode in [ArtMode::FullBw, ArtMode::FullColor] {
            let buf = render(60, 24, |v| {
                v.art = None;
                v.art_mode = mode;
                v.gap = Some(Gap {
                    remaining_secs: 900.0,
                });
            });
            assert!(
                grid(&buf).contains("remaining"),
                "{mode:?} left the rect blank"
            );
        }
    }

    /// The transport lives on the bottom bar only — one transport, one
    /// cursor. The workspace draws the title and the neighbours, and
    /// none of the control glyphs the bar owns.
    #[test]
    fn the_workspace_draws_no_transport_controls() {
        let buf = render(90, 24, |v| {
            v.paused = true;
            v.gap_remaining = Some(6.2);
        });
        let text = grid(&buf);
        assert!(
            !text.contains(GLYPH_PLAY),
            "the play glyph belongs to the bottom bar:\n{text}"
        );
        assert!(
            !text.contains("paused"),
            "the bar owns the pause readout too:\n{text}"
        );
    }

    /// The prev/next line is the screen's key hint: each neighbour is
    /// tagged with the key that actually goes there, from the keymap
    /// rather than printed as its default.
    #[test]
    fn the_context_line_names_the_neighbours_and_their_keys() {
        let keymap = Keymap::default();
        let cover = art();
        let mut v = view(&keymap, Some(&cover));
        v.previous = Some("before");
        v.next = Some("after");
        let area = Rect::new(0, 0, 90, 24);
        let mut buf = buffer(90, 24);
        v.render(area, &mut buf);
        let text = grid(&buf);
        assert!(text.contains("before"), "{text}");
        assert!(text.contains("after"), "{text}");
        assert!(
            text.contains(&key_label(keymap.get(KeyAction::SkipBack))),
            "the previous hint is the real key:\n{text}"
        );
        assert!(
            text.contains(&key_label(keymap.get(KeyAction::SkipNext))),
            "the next hint is the real key:\n{text}"
        );
    }

    /// A pane too short for a readable cover drops the cover (and it
    /// alone): the title is worth more than mush.
    #[test]
    fn a_short_pane_drops_the_cover_but_keeps_the_title() {
        let l = layout(Rect::new(0, 0, 60, 8), true, false, 2.0).unwrap();
        assert!(l.art.is_none(), "{l:?}");
        let buf = render(60, 8, |_| {});
        assert!(grid(&buf).contains("a song"), "{}", grid(&buf));
    }

    /// The stack stops above the rows the spectrum owns, so the meter
    /// main.rs draws over the pane's bottom never covers the title.
    #[test]
    fn the_stack_stops_above_the_spectrum_rows() {
        for h in [24u16, 20, 16, 12] {
            let l = layout(Rect::new(0, 0, 90, h), true, true, 2.0).unwrap();
            let title_y = l.title_y - Rect::new(0, 0, 90, h).y;
            assert!(
                title_y < h - METER_ROWS,
                "title on row {title_y} of {h}, the meter owns the last {METER_ROWS}"
            );
            if let Some(y) = l.context_y {
                assert!(y < h - METER_ROWS, "context on row {y} of {h}");
            }
        }
    }
}

