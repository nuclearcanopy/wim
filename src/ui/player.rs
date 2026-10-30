use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::app::LoopMode;
use crate::browse::ArtMode;
use crate::keymap::{key_label, KeyAction, Keymap};
use crate::theme::{shade, BRIGHT, DIM, MID, NEAR_BLACK, NEAR_WHITE, WHITE};
use crate::ui::albumart::CachedArt;
use crate::ui::controlsbar::{
    fill_bg, segments_that_fit, track, Field, GLYPH_CROSSFADE, GLYPH_EQ, GLYPH_LOOP, GLYPH_PAUSE,
    GLYPH_PLAY, GLYPH_SHUFFLE, GLYPH_SPEED, SCRUB_BG, SELECT_BG, TRACK_MIN,
};
use crate::ui::draw;
use crate::ui::progress::fmt_time;
use crate::ui::textfit::{truncate_with_ellipsis, width};

/// Rows at the bottom of the pane the level meter owns (main.rs draws
/// it over whatever was there, on the same `main_content.height > 2`
/// condition mirrored below), so the stack stops above them.
pub const METER_ROWS: u16 = 2;

/// Title, control row, scrubber: the three rows a player is, which no
/// amount of shrinking drops as long as there are rows for them.
const CORE_ROWS: u16 = 3;

/// The cover is only drawn once it can be at least this big. Smaller
/// than this it is unreadable mush and the rows are worth more to the
/// transport, so a short pane degrades to exactly the old screen.
const ART_MIN_ROWS: u16 = 6;
const ART_MIN_COLS: u16 = 14;

/// Share of the pane the cover may take, same budget rule as
/// `browse::layout`'s big-art path.
const ART_SHARE: f32 = 0.6;

/// Longest scrubber the now-playing screen draws. It is centred under
/// the title rather than stretched edge to edge like the bottom bar's:
/// on a wide terminal a full-width track turns a 5-second step into a
/// one-cell twitch.
const TRACK_MAX: usize = 56;

/// Separator between controls. Words get a cell more than the bottom
/// bar's two so that neighbouring selection plates (which pad a cell
/// either side) cannot touch and read as one wide plate.
const SEP_WORDS: usize = 3;
const SEP_GLYPHS: usize = 2;

/// Between hints on the bottom row; a dot, since those are sentences
/// about keys and two spaces alone did not read as a list.
const HINT_SEP: &str = "  ·  ";

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
    pub controls_y: Option<u16>,
    pub progress_y: Option<u16>,
    pub hint_y: Option<u16>,
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
    // main.rs only draws the level meter when the pane is taller than
    // the meter itself, so below that there is nothing to reserve.
    let body_h = if area.height > METER_ROWS {
        area.height - METER_ROWS
    } else {
        area.height
    };

    // the core three (title, controls, scrubber) first, then extra
    // rows in the order they earn their keep: a gap under the title,
    // the key hint, the hint's own gap, the prev/next line. each
    // number is the body height that row starts being worth having.
    let progress = body_h >= 2;
    let controls = body_h >= CORE_ROWS;
    let gap_title = body_h >= 4;
    let hint = body_h >= 5;
    let gap_hint = body_h >= 6;
    let context = has_context && body_h >= 7;
    let stack_h = 1
        + u16::from(context)
        + u16::from(gap_title)
        + u16::from(controls)
        + u16::from(progress)
        + u16::from(gap_hint)
        + u16::from(hint);

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
    if gap_title {
        y += 1;
    }
    let controls_y = controls.then(|| {
        let row = y;
        y += 1;
        row
    });
    let progress_y = progress.then(|| {
        let row = y;
        y += 1;
        row
    });
    if gap_hint {
        y += 1;
    }
    Some(PlayerLayout {
        art,
        title_y,
        context_y,
        controls_y,
        progress_y,
        hint_y: hint.then_some(y),
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
    /// `Real` leaves the cover panel to the host's image overlay,
    /// which goes on the `PlayerLayout::art` rect this view used; same
    /// contract as `BrowseListView`.
    pub art_mode: ArtMode,
    /// The terminal's real cell height:width ratio
    /// (`main::cell_aspect`), threaded to `layout` and `CachedArt` so
    /// both agree on what "square" means here.
    pub cell_aspect: f32,
    /// Mid-drag this is the pending scrub target rather than mpv's own
    /// position, same as the bottom bar.
    pub elapsed_secs: Option<f64>,
    pub duration_secs: Option<f64>,
    pub paused: bool,
    pub loop_mode: LoopMode,
    pub shuffle_on: bool,
    pub speed: f64,
    pub eq_preset: &'a str,
    pub crossfade_on: bool,
    pub previous: Option<&'a str>,
    pub next: Option<&'a str>,
    /// Transport cursor; `Some` exactly while this pane has focus, so
    /// it doubles as "are the arrows mine right now" for the hint.
    pub selected: Option<Field>,
    /// `true` while enter-on-the-scrubber has a drag open.
    pub scrubbing: bool,
    pub keymap: &'a Keymap,
}

impl Widget for PlayerView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let has_art = self.art.is_some() && self.art_mode != ArtMode::Off;
        let Some(l) = layout(
            area,
            has_art,
            self.previous.is_some() || self.next.is_some(),
            self.cell_aspect,
        ) else {
            return;
        };

        if let (Some(rect), ArtMode::Ascii) = (l.art, self.art_mode) {
            CachedArt {
                image: self.art,
                cell_aspect: self.cell_aspect,
                key: self.art_key,
            }
            .render(rect, buf);
        }

        draw_centered(buf, area, l.title_y, self.title, shade(NEAR_WHITE));
        if let Some(y) = l.context_y {
            draw_centered(buf, area, y, &self.context_line(), shade(NEAR_BLACK));
        }
        if let Some(y) = l.controls_y {
            let (controls, sep) = self.controls(area.width as usize);
            self.draw_row(buf, area, y, &controls, sep);
        }
        if let Some(y) = l.progress_y {
            self.draw_row(
                buf,
                area,
                y,
                &[self.scrubber(area.width as usize)],
                SEP_WORDS,
            );
        }
        if let Some(y) = l.hint_y {
            let hint = self.hint(area.width as usize);
            draw_centered(buf, area, y, &hint, shade(NEAR_BLACK));
        }
    }
}

/// How much of a control's label the row has room for: glyph, name and
/// value; glyph and name; or the bottom bar's bare glyph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Detail {
    Full,
    Named,
    Glyph,
}

/// One drawn transport element: its text, whether its control is
/// active (resting brightness, exactly as on the bottom bar) and which
/// `Field` the cursor has to be on for it to be the selected one.
struct Element {
    field: Field,
    text: String,
    lit: bool,
}

impl PlayerView<'_> {
    fn key(&self, action: KeyAction) -> String {
        key_label(self.keymap.get(action))
    }

    /// Previous and next track, each tagged with the key that actually
    /// goes there: on a screen whose complaint was "i can't tell how to
    /// control it", the two labels are also the two hints.
    fn context_line(&self) -> String {
        let mut parts = Vec::new();
        if let Some(prev) = self.previous {
            parts.push(format!("{} ◂ {prev}", self.key(KeyAction::SkipBack)));
        }
        if let Some(next) = self.next {
            parts.push(format!("{next} ▸ {}", self.key(KeyAction::SkipNext)));
        }
        parts.join("     ")
    }

    /// The six toggles, and the separator the row ended up using.
    /// Words while they fit: this screen has a whole pane where the
    /// bottom bar has one strip, and "crossfade off" is legible where a
    /// dim `◢◣` is only legible once you already know it. The glyph
    /// stays in front of the word so the two surfaces still read as the
    /// same control. A pane too narrow for the values keeps the names
    /// (brightness already says on/off, same as the bar) before falling
    /// back to the bar's bare glyphs; words are never truncated.
    fn controls(&self, avail: usize) -> (Vec<Element>, usize) {
        for (detail, sep) in [
            (Detail::Full, SEP_WORDS),
            (Detail::Named, SEP_GLYPHS),
            (Detail::Glyph, SEP_GLYPHS),
        ] {
            let items = self.elements(detail);
            let lengths: Vec<usize> = items.iter().map(|e| width(&e.text)).collect();
            let fits = segments_that_fit(&lengths, avail, sep);
            if fits == items.len() {
                return (items, sep);
            }
            // bare glyphs are the last tier, so this is where whole
            // elements start going from the right rather than a wider
            // form getting cut off mid-word.
            if detail == Detail::Glyph {
                let mut items = items;
                items.truncate(fits);
                return (items, sep);
            }
        }
        unreachable!("the glyph tier always returns")
    }

    fn elements(&self, detail: Detail) -> Vec<Element> {
        let speed_changed = (self.speed - 1.0).abs() >= 0.005;
        let onoff = |on: bool| if on { "on" } else { "off" };
        // name without value: the glyph's brightness already carries
        // on/off, so the middle tier spends its cells on which control
        // this is, which is the part a glyph can't teach you.
        let named = |g: char, name: &str, value: String| match detail {
            Detail::Full => format!("{g} {name} {value}"),
            Detail::Named => format!("{g} {name}"),
            Detail::Glyph => g.to_string(),
        };
        vec![
            Element {
                field: Field::Pause,
                // lit means "audio is moving", same as the bar: the
                // play triangle is the bright state.
                lit: !self.paused,
                text: match (detail, self.paused) {
                    (Detail::Glyph, true) => GLYPH_PAUSE.to_string(),
                    (Detail::Glyph, false) => GLYPH_PLAY.to_string(),
                    (_, true) => format!("{GLYPH_PAUSE} paused"),
                    (_, false) => format!("{GLYPH_PLAY} playing"),
                },
            },
            Element {
                field: Field::Loop,
                lit: self.loop_mode != LoopMode::Off,
                text: match detail {
                    Detail::Glyph if self.loop_mode == LoopMode::One => {
                        format!("{GLYPH_LOOP}1")
                    }
                    Detail::Glyph => GLYPH_LOOP.to_string(),
                    _ => named(GLYPH_LOOP, "loop", self.loop_mode.label().to_string()),
                },
            },
            Element {
                field: Field::Shuffle,
                lit: self.shuffle_on,
                text: named(GLYPH_SHUFFLE, "shuffle", onoff(self.shuffle_on).to_string()),
            },
            Element {
                field: Field::Speed,
                lit: speed_changed,
                // the rate stays even at 1.00x on the two word tiers:
                // the bar drops it to save cells, this screen has them
                // and "what speed am i playing at" is a thing it should
                // answer without being asked.
                text: match detail {
                    Detail::Full => format!("{GLYPH_SPEED} speed {:.2}x", self.speed),
                    Detail::Named => format!("{GLYPH_SPEED} {:.2}x", self.speed),
                    Detail::Glyph if speed_changed => {
                        format!("{GLYPH_SPEED}{:.2}", self.speed)
                    }
                    Detail::Glyph => GLYPH_SPEED.to_string(),
                },
            },
            Element {
                field: Field::Eq,
                lit: self.eq_preset != "flat",
                text: match detail {
                    Detail::Full => format!("{GLYPH_EQ} eq {}", self.eq_preset),
                    Detail::Named => format!("{GLYPH_EQ} eq"),
                    Detail::Glyph => GLYPH_EQ.to_string(),
                },
            },
            Element {
                field: Field::Crossfade,
                lit: self.crossfade_on,
                text: match detail {
                    Detail::Full => {
                        format!("{GLYPH_CROSSFADE} crossfade {}", onoff(self.crossfade_on))
                    }
                    Detail::Named => format!("{GLYPH_CROSSFADE} crossfade"),
                    Detail::Glyph => GLYPH_CROSSFADE.to_string(),
                },
            },
        ]
    }

    fn scrubber(&self, avail: usize) -> Element {
        let elapsed = fmt_time(self.elapsed_secs);
        let total = fmt_time(self.duration_secs);
        let frac = match (self.elapsed_secs, self.duration_secs) {
            (Some(e), Some(d)) if d > 0.0 => (e / d).clamp(0.0, 1.0),
            _ => 0.0,
        };
        let dragging = self.scrubbing && self.selected == Some(Field::Progress);
        // two spaces around the track, two cells for the plate.
        let fixed = width(&elapsed) + width(&total) + 4;
        let text = match avail.checked_sub(fixed + TRACK_MIN) {
            Some(slack) => format!(
                "{elapsed} {} {total}",
                track(frac, TRACK_MIN + slack.min(TRACK_MAX - TRACK_MIN), dragging)
            ),
            // too narrow for a track anyone could read a position off,
            // so the position goes as text rather than as a stub bar.
            None => format!("{elapsed} / {total}"),
        };
        Element {
            field: Field::Progress,
            lit: self.elapsed_secs.is_some(),
            text,
        }
    }

    /// What the keys do right now, resolved from the live keymap. The
    /// whole point of this row: the transport used to be legible only
    /// from the help overlay. Listed most useful first and cut whole
    /// hints at a time, so a narrow pane loses "minimize" rather than
    /// half of "move focus".
    fn hint(&self, avail: usize) -> String {
        let minimize = format!("{}: minimize", self.key(KeyAction::ToggleMinimize));
        let parts = if self.scrubbing {
            vec![
                "arrows: seek".into(),
                "shift: coarse".into(),
                "enter/esc: done".into(),
            ]
        } else if let Some(field) = self.selected {
            vec![
                self.activate_hint(field),
                "arrows: pick".into(),
                "tab: move focus".into(),
                minimize,
            ]
        } else {
            vec!["tab: move focus".into(), minimize]
        };
        let lengths: Vec<usize> = parts.iter().map(|p| width(p)).collect();
        let fits = segments_that_fit(&lengths, avail, HINT_SEP.len()).max(1);
        parts[..fits].join(HINT_SEP)
    }

    /// What enter does on the selected control, plus the global key
    /// that does the same thing where there is one.
    fn activate_hint(&self, field: Field) -> String {
        let with_key = |what: &str, action| format!("enter: {what} (or {})", self.key(action));
        match field {
            Field::Pause => with_key("play/pause", KeyAction::TogglePause),
            Field::Loop => with_key("cycle loop", KeyAction::CycleLoop),
            Field::Shuffle => with_key("toggle shuffle", KeyAction::ToggleShuffle),
            Field::Speed => with_key("speed up", KeyAction::SpeedUp),
            // enter opens the menu here; the global key cycles presets
            // instead, so naming it beside "enter" would be a lie.
            Field::Eq => "enter: eq menu".into(),
            Field::Crossfade => with_key("crossfade", KeyAction::ToggleCrossfade),
            Field::Progress => "enter: seek in this track".into(),
        }
    }

    /// One centred row of elements, each plated when the cursor is on
    /// it. Same convention as the bar: the plate marks the cursor, so
    /// brightness stays free to mean on/off, and an inactive control is
    /// only lifted to `MID` so it reads against its own plate.
    fn draw_row(&self, buf: &mut Buffer, area: Rect, y: u16, items: &[Element], sep: usize) {
        if items.is_empty() || y >= area.y + area.height {
            return;
        }
        let total: usize =
            items.iter().map(|e| width(&e.text)).sum::<usize>() + sep * (items.len() - 1);
        let limit = area.x + area.width;
        let mut x = area.x + ((area.width as usize).saturating_sub(total) / 2) as u16;
        for (i, item) in items.iter().enumerate() {
            if i > 0 {
                x += sep as u16;
            }
            if x >= limit {
                return;
            }
            let is_selected = self.selected == Some(item.field);
            let dragging = is_selected && self.scrubbing && item.field == Field::Progress;
            let level = if dragging {
                WHITE
            } else if item.lit {
                BRIGHT
            } else {
                DIM
            };
            let level = if is_selected { level.max(MID) } else { level };
            let bg = if dragging {
                Some(shade(SCRUB_BG))
            } else if is_selected {
                Some(shade(SELECT_BG))
            } else {
                None
            };
            // one cell of padding each side, so a one-glyph element's
            // plate reads as a plate and not as a stray lit cell.
            if let Some(bg) = bg {
                let end = x.saturating_add(width(&item.text) as u16 + 1);
                fill_bg(buf, x.saturating_sub(1).max(area.x), end.min(limit), y, bg);
            }
            x = draw::at_styled(
                buf,
                x,
                y,
                &item.text,
                shade(level),
                bg,
                limit.saturating_sub(x) as usize,
            );
        }
    }
}

fn draw_centered(buf: &mut Buffer, area: Rect, y: u16, text: &str, color: ratatui::style::Color) {
    // centred on display width, not char count: a wide-char title
    // used to drift right by its own overhang.
    let text = truncate_with_ellipsis(text, area.width as usize);
    draw::centered(buf, area, y, &text, color);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::controlsbar::{TRACK_HEAD, TRACK_HEAD_DRAG};
    use crate::ui::testbuf::{buffer, grid};
    use crossterm::event::KeyCode;
    use image::DynamicImage;
    use ratatui::style::Color;

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
            art_mode: ArtMode::Ascii,
            cell_aspect: 2.0,
            elapsed_secs: Some(30.0),
            duration_secs: Some(120.0),
            paused: false,
            loop_mode: LoopMode::Off,
            shuffle_on: false,
            speed: 1.0,
            eq_preset: "flat",
            crossfade_on: false,
            previous: None,
            next: None,
            selected: None,
            scrubbing: false,
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

    fn col_of(buf: &Buffer, y: u16, ch: char) -> u16 {
        (buf.area.x..buf.area.x + buf.area.width)
            .find(|x| {
                buf.cell((*x, y))
                    .is_some_and(|c| c.symbol().starts_with(ch))
            })
            .unwrap_or_else(|| panic!("no {ch:?} on row {y}:\n{}", grid(buf)))
    }

    fn row(buf: &Buffer, y: u16) -> String {
        grid(buf).lines().nth(y as usize).unwrap().to_string()
    }

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

    #[test]
    fn the_rect_the_widget_drew_into_is_the_rect_the_host_overlays() {
        // `Real` mode: the widget draws no ascii art, and the host puts
        // its image on the rect `layout` reported. Both sides have to
        // get the same answer from the same arguments.
        let area = Rect::new(3, 2, 70, 34);
        let reported = layout(area, true, false, 2.0).unwrap().art.unwrap();
        let keymap = Keymap::default();
        let cover = art();

        let mut real = view(&keymap, Some(&cover));
        real.art_mode = ArtMode::Real;
        let mut real_buf = buffer(80, 40);
        real.render(area, &mut real_buf);

        let mut ascii = view(&keymap, Some(&cover));
        ascii.art_mode = ArtMode::Ascii;
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

    #[test]
    fn art_is_dropped_below_the_size_it_stops_being_readable_at() {
        // too few rows: the transport wants them more than a 2-row
        // thumbnail does.
        assert!(layout(Rect::new(0, 0, 80, 12), true, false, 2.0)
            .unwrap()
            .art
            .is_none());
        // and too narrow, even with rows to spare.
        assert!(layout(Rect::new(0, 0, 13, 40), true, false, 2.0)
            .unwrap()
            .art
            .is_none());
        // the pane that does get art keeps the whole transport too.
        let l = layout(Rect::new(0, 0, 80, 30), true, false, 2.0).unwrap();
        assert!(l.art.is_some() && l.controls_y.is_some() && l.progress_y.is_some());
    }

    #[test]
    fn no_art_at_all_lays_out_exactly_like_a_short_pane() {
        let with = layout(Rect::new(0, 0, 80, 12), true, false, 2.0).unwrap();
        let without = layout(Rect::new(0, 0, 80, 12), false, false, 2.0).unwrap();
        assert_eq!(with, without);
    }

    #[test]
    fn a_small_pane_keeps_the_transport_and_drops_everything_else() {
        // five rows, two of which the level meter owns: title,
        // controls, scrubber and nothing else.
        let core = layout(Rect::new(0, 0, 40, 5), true, true, 2.0).unwrap();
        assert!(core.art.is_none() && core.context_y.is_none() && core.hint_y.is_none());
        assert_eq!(
            (core.title_y, core.controls_y, core.progress_y),
            (0, Some(1), Some(2))
        );
        // two rows (no meter at that height): title and position,
        // which is what you actually need; the buttons are still on
        // the bottom bar.
        let two = layout(Rect::new(0, 0, 40, 2), true, true, 2.0).unwrap();
        assert_eq!((two.controls_y, two.progress_y), (None, Some(1)));
        let one = layout(Rect::new(0, 0, 40, 1), true, true, 2.0).unwrap();
        assert_eq!((one.title_y, one.progress_y), (0, None));
    }

    #[test]
    fn nothing_is_drawn_outside_the_pane_or_under_the_level_meter() {
        for h in 1..40u16 {
            let area = Rect::new(0, 0, 40, h);
            let l = layout(area, true, true, 2.0).unwrap();
            // the bottom two rows belong to the level meter whenever
            // there is a meter at all.
            let last = if h > METER_ROWS { h - METER_ROWS } else { h };
            for row in [
                Some(l.title_y),
                l.context_y,
                l.controls_y,
                l.progress_y,
                l.hint_y,
            ]
            .into_iter()
            .flatten()
            {
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
                v.selected = Some(Field::Progress);
                v.scrubbing = true;
                v.previous = Some("before");
                v.next = Some("after");
            });
            assert_eq!(buf.area, Rect::new(0, 0, w, h));
        }
    }

    #[test]
    fn the_control_row_spells_the_words_out_when_there_is_room() {
        let buf = render(100, 30, |_| {});
        let l = layout(Rect::new(0, 0, 100, 30), true, false, 2.0).unwrap();
        let line = row(&buf, l.controls_y.unwrap());
        for word in [
            "playing",
            "loop off",
            "shuffle off",
            "speed 1.00x",
            "eq flat",
            "crossfade off",
        ] {
            assert!(line.contains(word), "{word:?} missing:\n{line}");
        }
        // the glyphs stay in front of the words, so a control reads as
        // the same control it is on the bottom bar.
        assert!(line.contains(GLYPH_SHUFFLE), "{line}");
    }

    #[test]
    fn a_pane_too_narrow_for_the_values_keeps_the_names() {
        let buf = render(60, 30, |v| {
            v.shuffle_on = true;
            v.crossfade_on = true;
        });
        let l = layout(Rect::new(0, 0, 60, 30), true, false, 2.0).unwrap();
        let y = l.controls_y.unwrap();
        let line = row(&buf, y);
        // the names survive, which is the part a glyph cannot teach;
        // on/off goes back to being brightness, same as on the bar.
        for word in ["shuffle", "crossfade", "loop", "eq"] {
            assert!(line.contains(word), "{word:?} missing:\n{line}");
        }
        assert!(!line.contains("shuffle on"), "no room for that:\n{line}");
        assert_eq!(
            buf.cell((col_of(&buf, y, GLYPH_SHUFFLE), y)).unwrap().fg,
            shade(BRIGHT)
        );
        assert_eq!(
            buf.cell((col_of(&buf, y, GLYPH_LOOP), y)).unwrap().fg,
            shade(DIM)
        );
        // the speed still reads as a rate rather than a bare sign.
        assert!(line.contains("1.00x"), "{line}");
    }

    #[test]
    fn a_narrow_pane_falls_back_to_the_bars_glyphs_rather_than_cut_words() {
        let buf = render(30, 30, |_| {});
        let l = layout(Rect::new(0, 0, 30, 30), true, false, 2.0).unwrap();
        let line = row(&buf, l.controls_y.unwrap());
        assert!(!line.contains("shuffle"), "truncated words:\n{line}");
        assert!(line.contains(GLYPH_SHUFFLE), "lost the glyph:\n{line}");
        assert!(line.contains(GLYPH_LOOP), "lost the glyph:\n{line}");
        // narrower still and whole elements go from the right, never
        // half an element.
        let tiny = render(10, 30, |_| {});
        let tl = layout(Rect::new(0, 0, 10, 30), true, false, 2.0).unwrap();
        let tiny_line = row(&tiny, tl.controls_y.unwrap());
        for ch in GLYPH_CROSSFADE.chars() {
            assert!(!tiny_line.contains(ch), "half a crossfade:\n{tiny_line}");
        }
    }

    #[test]
    fn the_plate_lands_on_exactly_the_selected_control() {
        let buf = render(100, 30, |v| v.selected = Some(Field::Shuffle));
        let l = layout(Rect::new(0, 0, 100, 30), true, false, 2.0).unwrap();
        let y = l.controls_y.unwrap();
        let col = col_of(&buf, y, GLYPH_SHUFFLE);
        let plate = shade(SELECT_BG);
        assert_eq!(buf.cell((col, y)).unwrap().bg, plate);
        // padded a cell either side, and the word is on the plate too
        // ("⇄ shuffle off" is 13 cells, so the plate ends at +13).
        assert_eq!(buf.cell((col - 1, y)).unwrap().bg, plate);
        assert_eq!(buf.cell((col + 13, y)).unwrap().bg, plate);
        assert_eq!(buf.cell((col + 14, y)).unwrap().bg, Color::Reset);
        // nothing else on the row is plated, and the neighbour keeps
        // its own off-state brightness.
        let loop_col = col_of(&buf, y, GLYPH_LOOP);
        assert_eq!(buf.cell((loop_col, y)).unwrap().bg, Color::Reset);
        assert_eq!(buf.cell((loop_col, y)).unwrap().fg, shade(DIM));
        // selecting an off control lifts it enough to read off the
        // plate without faking an "on" state.
        assert_eq!(buf.cell((col, y)).unwrap().fg, shade(MID));
        const { assert!(MID < BRIGHT) };
        // and the scrubber one row down is untouched.
        assert_eq!(
            buf.cell((
                col_of(&buf, l.progress_y.unwrap(), TRACK_HEAD),
                l.progress_y.unwrap()
            ))
            .unwrap()
            .bg,
            Color::Reset
        );
    }

    #[test]
    fn scrub_mode_renders_differently_from_a_merely_selected_scrubber() {
        let l = layout(Rect::new(0, 0, 100, 30), true, false, 2.0).unwrap();
        let y = l.progress_y.unwrap();
        let selected = render(100, 30, |v| v.selected = Some(Field::Progress));
        let dragging = render(100, 30, |v| {
            v.selected = Some(Field::Progress);
            v.scrubbing = true;
        });
        assert!(!grid(&selected).contains(TRACK_HEAD_DRAG));
        assert_eq!(
            selected
                .cell((col_of(&selected, y, TRACK_HEAD), y))
                .unwrap()
                .bg,
            shade(SELECT_BG)
        );
        let head = col_of(&dragging, y, TRACK_HEAD_DRAG);
        assert!(!grid(&dragging).contains(TRACK_HEAD));
        assert_eq!(dragging.cell((head, y)).unwrap().bg, shade(SCRUB_BG));
        assert_eq!(dragging.cell((head, y)).unwrap().fg, shade(WHITE));
        const { assert!(SCRUB_BG > SELECT_BG) };
    }

    #[test]
    fn the_scrubber_goes_to_text_when_no_readable_track_fits() {
        let buf = render(16, 30, |_| {});
        let l = layout(Rect::new(0, 0, 16, 30), true, false, 2.0).unwrap();
        let line = row(&buf, l.progress_y.unwrap());
        assert!(line.contains("0:30"), "lost the position:\n{line}");
        assert!(!line.contains(TRACK_HEAD), "a stub track:\n{line}");
    }

    #[test]
    fn every_hint_follows_the_live_keymap() {
        let mut keymap = Keymap::default();
        keymap.set(KeyAction::ToggleMinimize, KeyCode::Char('z'));
        keymap.set(KeyAction::TogglePause, KeyCode::F(5));
        keymap.set(KeyAction::SkipBack, KeyCode::Char('('));
        let cover = art();
        let mut v = view(&keymap, Some(&cover));
        v.selected = Some(Field::Pause);
        v.previous = Some("before");
        let area = Rect::new(0, 0, 100, 30);
        let mut buf = buffer(100, 30);
        v.render(area, &mut buf);
        let out = grid(&buf);
        assert!(out.contains("z: minimize"), "{out}");
        assert!(out.contains("enter: play/pause (or f5)"), "{out}");
        assert!(out.contains("( ◂ before"), "{out}");
        assert!(!out.contains("m: minimize"), "stale default key:\n{out}");
        assert!(!out.contains("(or space)"), "stale default key:\n{out}");
    }

    #[test]
    fn the_hint_says_what_the_arrows_do_only_when_the_pane_has_them() {
        let unfocused = grid(&render(100, 30, |_| {}));
        assert!(unfocused.contains("tab: move focus"), "{unfocused}");
        assert!(!unfocused.contains("arrows: pick"), "{unfocused}");

        let focused = grid(&render(100, 30, |v| v.selected = Some(Field::Loop)));
        assert!(focused.contains("arrows: pick"), "{focused}");
        assert!(focused.contains("enter: cycle loop (or l)"), "{focused}");

        // on the scrubber it answers the question the whole screen was
        // failing to answer: how do you move inside a song.
        let on_track = grid(&render(100, 30, |v| v.selected = Some(Field::Progress)));
        assert!(on_track.contains("enter: seek in this track"), "{on_track}");
        let scrubbing = grid(&render(100, 30, |v| {
            v.selected = Some(Field::Progress);
            v.scrubbing = true;
        }));
        assert!(scrubbing.contains("arrows: seek"), "{scrubbing}");
        assert!(scrubbing.contains("shift: coarse"), "{scrubbing}");
    }

    #[test]
    fn a_narrow_hint_drops_whole_hints_from_the_right() {
        let narrow = grid(&render(52, 30, |v| v.selected = Some(Field::Progress)));
        // what enter does comes first and survives; "minimize" is the
        // one that goes, and no hint is left half-printed.
        assert!(narrow.contains("enter: seek in this track"), "{narrow}");
        assert!(!narrow.contains("minimize"), "{narrow}");
        assert!(!narrow.contains('…'), "a hint was cut mid-word:\n{narrow}");
        // and a pane too narrow even for the first one keeps it rather
        // than drawing an empty row.
        let tiny = grid(&render(18, 30, |v| v.selected = Some(Field::Progress)));
        assert!(tiny.contains("enter: seek"), "{tiny}");
    }

    #[test]
    fn the_state_a_control_is_in_is_readable_without_pressing_it() {
        let buf = render(100, 30, |v| {
            v.loop_mode = LoopMode::All;
            v.shuffle_on = true;
            v.speed = 1.25;
            v.eq_preset = "bass";
            v.crossfade_on = true;
            v.paused = true;
        });
        let l = layout(Rect::new(0, 0, 100, 30), true, false, 2.0).unwrap();
        let y = l.controls_y.unwrap();
        let line = row(&buf, y);
        for word in [
            "paused",
            "loop all",
            "shuffle on",
            "speed 1.25x",
            "eq bass",
            "crossfade on",
        ] {
            assert!(line.contains(word), "{word:?} missing:\n{line}");
        }
        // on controls are bright, and pause is dim while paused for the
        // same reason as on the bar: lit means audio is moving.
        assert_eq!(
            buf.cell((col_of(&buf, y, GLYPH_SHUFFLE), y)).unwrap().fg,
            shade(BRIGHT)
        );
        assert_eq!(
            buf.cell((col_of(&buf, y, GLYPH_PAUSE), y)).unwrap().fg,
            shade(DIM)
        );
    }
}
