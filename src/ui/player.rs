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

/// Whether the layout reserves the cover panel, as `PlayerView`'s
/// render decides it. The host needs the same answer to place the
/// status card, and the two must not drift: a rect the view did not
/// reserve is the title's rows. Video claims the
/// rect in every art mode — it is not a cover, and `ArtMode::Off` is
/// the cover-panel switch, not the video one.
pub fn has_art(video: bool, art: bool, gap: bool, art_mode: ArtMode) -> bool {
    video || ((art || gap) && art_mode != ArtMode::Off)
}

pub fn video_sync_target(layout: PlayerLayout, area: Rect, video: bool) -> Option<Rect> {
    layout.video_sync.or_else(|| {
        video.then_some(Rect {
            x: area.x + area.width.saturating_sub(20) / 2,
            y: layout.title_y,
            width: area.width.min(20),
            height: 1,
        })
    })
}

/// Where every piece of the now-playing screen goes. Returned by
/// `layout` rather than decided inside `render` so the host can put a
/// `ratatui-image` overlay on exactly the `art` rect this picked, same
/// contract `browse::layout` has with `BrowseListView`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlayerLayout {
    /// Square-ish cover panel, centred horizontally; `None` when the
    /// pane cannot give it `ART_MIN_ROWS`/`ART_MIN_COLS`.
    pub art: Option<Rect>,
    /// Click target for opening subtitle synchronization while a video plays.
    pub video_sync: Option<Rect>,
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
    let video_sync = art.map(|rect| Rect {
        x: rect.x + rect.width.saturating_sub(20) / 2,
        y: rect.y + rect.height / 2 + 1,
        width: rect.width.min(20),
        height: 1,
    });
    Some(PlayerLayout {
        art,
        video_sync,
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
    /// A video is playing in mpv: reserve the cover rect for the native
    /// player status card, independent of the album-art preference.
    pub video: bool,
    /// Actionable status shown in the reserved video area.
    pub video_note: Option<&'a str>,
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
        // the layout reserves it. `ArtMode::Off` still means off for a
        // cover: a user who turned the panel off did not ask for a big
        // clock. a video claims it too, in every mode — `has_art` is
        // the one place that rule lives, so the status card reserves
        // the same rect this view does.
        let has_art = has_art(
            self.video,
            self.art.is_some(),
            self.gap.is_some(),
            self.art_mode,
        );
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
            // something deterministic. Video uses the rect for its
            // status card rather than a cover that could confuse the
            // user about which window owns playback.
            if let Some(gap) = self.gap {
                GapPanel { gap }.render(rect, buf);
            } else if self.video {
                if let Some(note) = self.video_note {
                    draw_centered(
                        buf,
                        rect,
                        rect.y + rect.height / 2 - 1,
                        note,
                        shade(NEAR_WHITE),
                    );
                    if let Some(button) = video_sync_target(l, area, self.video) {
                        let key = self.key(KeyAction::OpenSubtitleMenu);
                        let full = format!("[ LYRIC SYNC · {key} ]");
                        let label = if width(&full) <= button.width as usize {
                            full
                        } else {
                            format!("[ SYNC · {key} ]")
                        };
                        draw_centered(buf, rect, button.y, &label, shade(NEAR_WHITE));
                    }
                }
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

        let compact_title;
        let title = if self.video && l.art.is_none() {
            compact_title = format!("[ LYRIC SYNC · {} ]", self.key(KeyAction::OpenSubtitleMenu));
            compact_title.as_str()
        } else {
            self.title
        };
        draw_centered(buf, area, l.title_y, title, shade(NEAR_WHITE));
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

    /// A cover with a real hue in it, for the `ascii-color` test: a
    /// grey source would pass it without proving anything.
    fn hued_art() -> DynamicImage {
        DynamicImage::ImageRgb8(image::RgbImage::from_fn(8, 8, |x, y| {
            image::Rgb([(x * 31) as u8, (y * 17) as u8, 200])
        }))
    }

    fn view<'a>(keymap: &'a Keymap, art: Option<&'a DynamicImage>) -> PlayerView<'a> {
        PlayerView {
            title: "a song",
            art,
            art_key: "a:artist\u{1}album",
            art_mode: ArtMode::AsciiBw,
            video: false,
            video_note: None,
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

    /// Recovered from before the bottom-bar-only refactor, along with
    /// the layout function they test: the cover rect is square in
    /// *cells given the cell aspect*, centred, and above the title.
    #[test]
    fn the_cover_rect_is_square_ish_and_derived_from_the_width_it_got() {
        for (w, h, aspect) in [(80u16, 40u16, 2.0f32), (40, 40, 2.0), (120, 30, 2.4)] {
            let l = layout(Rect::new(0, 0, w, h), true, false, aspect).unwrap();
            let art = l.art.expect("a pane this size has room for art");
            // cells are `aspect` times taller than wide, so a square
            // cover is that many times wider in cells than it is tall.
            let ideal = art.height as f32 * aspect;
            assert!(
                (art.width as f32 - ideal).abs() <= 1.0,
                "{w}x{h}@{aspect}: {art:?} is not square"
            );
            assert!(art.x + art.width <= w, "{art:?} overflows a {w}-cell pane");
            // centred, and above the title rather than beside it.
            assert!(art.y + art.height < l.title_y, "{art:?} vs {l:?}");
            assert_eq!(art.x, (w - art.width) / 2);
        }
    }

    /// The host-overlay contract: in a `Full*` mode the widget draws
    /// nothing into the art rect and the host's image protocol goes on
    /// exactly the rect `layout` reported — both sides get the same
    /// answer from the same arguments.
    #[test]
    fn the_rect_the_widget_drew_into_is_the_rect_the_host_overlays() {
        let area = Rect::new(3, 2, 70, 34);
        let reported = layout(area, true, false, 2.0).unwrap().art.unwrap();
        let keymap = Keymap::default();
        let cover = art();

        let mut real = view(&keymap, Some(&cover));
        real.art_mode = ArtMode::FullColor;
        let mut real_buf = buffer(80, 40);
        real.render(area, &mut real_buf);

        let mut ascii = view(&keymap, Some(&cover));
        ascii.art_mode = ArtMode::AsciiBw;
        let mut ascii_buf = buffer(80, 40);
        ascii.render(area, &mut ascii_buf);

        for y in reported.y..reported.y + reported.height {
            for x in reported.x..reported.x + reported.width {
                assert_eq!(
                    real_buf.cell((x, y)).unwrap().symbol(),
                    ".",
                    "real mode drew into the host's overlay rect at {x},{y}"
                );
                // which block glyph a cell gets depends on its own
                // four samples, so the claim under test is that every
                // cell in the reported rect was painted at all.
                assert_ne!(
                    ascii_buf.cell((x, y)).unwrap().symbol(),
                    ".",
                    "ascii art did not fill the rect it reported at {x},{y}"
                );
            }
        }
        // ...and nothing outside it, in either mode.
        assert_eq!(
            ascii_buf
                .cell((reported.x, reported.y + reported.height))
                .unwrap()
                .symbol(),
            "."
        );
    }

    /// The monochrome exception, bounded: `ascii-color` may put hues in
    /// the cover rect and nowhere else. The title and context stay
    /// `theme::shade`, and `ascii-bw` stays strictly grey over the
    /// whole pane including the cover.
    #[test]
    fn ascii_color_tints_the_cover_and_nothing_else() {
        use ratatui::style::Color;
        let is_grey = |c: Color| match c {
            Color::Rgb(r, g, b) => r == g && g == b,
            // `Reset` is the untouched default, not a hue.
            Color::Reset => true,
            other => panic!("not a shade or a tint: {other:?}"),
        };
        let area = Rect::new(0, 0, 80, 40);
        let cover = hued_art();
        let reported = layout(area, true, false, 2.0).unwrap().art.unwrap();
        let keymap = Keymap::default();

        for (mode, want_tint) in [(ArtMode::AsciiColor, true), (ArtMode::AsciiBw, false)] {
            let mut v = view(&keymap, Some(&cover));
            v.art_mode = mode;
            let mut buf = buffer(80, 40);
            v.render(area, &mut buf);

            let mut tinted = 0;
            for y in area.y..area.y + area.height {
                for x in area.x..area.x + area.width {
                    let cell = buf.cell((x, y)).unwrap();
                    let grey = is_grey(cell.fg) && is_grey(cell.bg);
                    let in_art = (reported.x..reported.x + reported.width).contains(&x)
                        && (reported.y..reported.y + reported.height).contains(&y);
                    assert!(
                        grey || in_art,
                        "{mode:?} leaked a hue outside the cover at {x},{y}: {cell:?}"
                    );
                    tinted += u32::from(!grey);
                }
            }
            assert_eq!(
                tinted > 0,
                want_tint,
                "{mode:?} painted {tinted} tinted cells"
            );
        }
    }

    #[test]
    fn art_is_dropped_below_the_size_it_stops_being_readable_at() {
        // too few rows: the title is worth more than a 2-row thumbnail.
        assert!(layout(Rect::new(0, 0, 80, 12), true, false, 2.0)
            .unwrap()
            .art
            .is_none());
        // and too narrow, even with rows to spare.
        assert!(layout(Rect::new(0, 0, 13, 40), true, false, 2.0)
            .unwrap()
            .art
            .is_none());
        // the pane that does get art keeps the title and context too.
        let l = layout(Rect::new(0, 0, 80, 30), true, true, 2.0).unwrap();
        assert!(l.art.is_some() && l.context_y.is_some());
    }

    #[test]
    fn no_art_at_all_lays_out_exactly_like_a_short_pane() {
        let with = layout(Rect::new(0, 0, 80, 12), true, false, 2.0).unwrap();
        let without = layout(Rect::new(0, 0, 80, 12), false, false, 2.0).unwrap();
        assert_eq!(with, without);
    }

    #[test]
    fn nothing_is_drawn_outside_the_pane_or_under_the_level_meter() {
        for h in 1..40u16 {
            let area = Rect::new(0, 0, 40, h);
            let l = layout(area, true, true, 2.0).unwrap();
            // the bottom two rows belong to the spectrum whenever
            // there is a meter at all.
            let last = if h > METER_ROWS { h - METER_ROWS } else { h };
            for row in [Some(l.title_y), l.context_y].into_iter().flatten() {
                assert!(
                    row < last,
                    "row {row} of {l:?} is outside 0..{last} (h={h})"
                );
            }
            if let Some(art) = l.art {
                assert!(art.y + art.height < last, "{art:?} under the meter (h={h})");
            }
        }
    }

    #[test]
    fn degenerate_panes_render_without_panicking() {
        // a zero-width or zero-height pane has no layout at all.
        for (w, h) in [(0u16, 0u16), (1, 0), (0, 10), (0, 1)] {
            assert!(layout(Rect::new(0, 0, w, h), true, true, 2.0).is_none());
        }
        for (w, h) in [(1u16, 1u16), (2, 1), (1, 40), (3, 3), (14, 16), (40, 2)] {
            let buf = render(w, h, |v| {
                v.previous = Some("before");
                v.next = Some("after");
            });
            assert_eq!(buf.area, Rect::new(0, 0, w, h));
        }
    }

    /// Video claims the cover rect in every art mode — `ArtMode::Off`
    /// is the cover-panel switch, not the video one — and the view
    /// draws no cover beneath its status card.
    #[test]
    fn video_claims_the_rect_in_every_art_mode_for_its_status_card() {
        let keymap = Keymap::default();
        let cover = art();
        for mode in [
            ArtMode::Off,
            ArtMode::AsciiBw,
            ArtMode::AsciiColor,
            ArtMode::FullBw,
            ArtMode::FullColor,
        ] {
            let area = Rect::new(0, 0, 60, 24);
            let mut v = view(&keymap, Some(&cover));
            v.art_mode = mode;
            v.video = true;
            let mut buf = buffer(60, 24);
            v.render(area, &mut buf);

            let want = has_art(true, true, false, mode);
            let l = layout(area, want, false, 2.0).unwrap();
            let got = l.art;
            let text = grid(&buf);
            if let Some(rect) = got {
                // the view reserved for video status, and there is no
                // unrelated cover beneath it.
                for y in rect.y..rect.y + rect.height {
                    for x in rect.x..rect.x + rect.width {
                        let symbol = buf.cell((x, y)).unwrap().symbol();
                        if l.video_sync.is_some_and(|button| {
                            x >= button.x && x < button.x + button.width && y == button.y
                        }) {
                            continue;
                        }
                        assert_eq!(symbol, ".", "{mode:?}: unexpected paint at {x},{y}");
                    }
                }
                assert!(text.contains("a song"), "{mode:?}: the title still draws");
            } else {
                panic!("{mode:?}: a video this size has a rect to show in");
            }
        }
        // and with the cover panel off and no video, there is no rect:
        // `Off` still means off for a cover.
        assert!(!has_art(false, true, false, ArtMode::Off));
    }

    #[test]
    fn video_status_is_drawn_in_the_reserved_rect() {
        let keymap = Keymap::default();
        let area = Rect::new(0, 0, 60, 24);
        let mut v = view(&keymap, None);
        v.video = true;
        v.video_note = Some("VIDEO PLAYING IN SEPARATE MPV WINDOW");
        let mut buf = buffer(60, 24);
        v.render(area, &mut buf);
        let text = grid(&buf);
        assert!(text.contains("VIDEO PLAYING"), "{text}");
        assert!(text.contains("LYRIC SYNC"), "{text}");
        let target =
            video_sync_target(layout(area, true, false, 2.0).unwrap(), area, true).unwrap();
        assert!(target.width > 0 && target.height == 1);
        assert!(target.x >= area.x && target.x + target.width <= area.x + area.width);
        assert!(target.y >= area.y && target.y < area.y + area.height);
    }

    #[test]
    fn compact_video_panel_keeps_a_clickable_sync_hint() {
        let keymap = Keymap::default();
        let area = Rect::new(0, 0, 18, 20);
        let mut view = view(&keymap, None);
        view.video = true;
        view.video_note = Some("VIDEO IN SEPARATE MPV WINDOW");
        let mut buf = buffer(area.width, area.height);
        view.render(area, &mut buf);
        let layout = layout(area, true, false, 2.0).unwrap();
        let target = video_sync_target(layout, area, true).unwrap();
        assert_eq!(target.width, 16);
        assert!(grid(&buf).contains("SYNC · t"), "{}", grid(&buf));
        assert_eq!(target.height, 1);
    }
}
