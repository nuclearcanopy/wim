use std::cmp::Ordering;

use crossterm::event::KeyCode;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::widgets::Widget;

use crate::app::LoopMode;
use crate::theme::{lerp, shade, BRIGHT, DIM, MID, NEAR_BLACK, NEAR_WHITE, WHITE};
use crate::ui::progress::readouts;
use crate::ui::textfit::{char_cells, truncate_to_width, truncate_with_ellipsis, width};

/// Every glyph on this line is drawn from a block unicode never gave
/// an emoji presentation to (latin-1, box drawing, block elements,
/// geometric shapes, math operators, and the arrows outside
/// `U+2190..U+21AA`). Same hazard `notify::Severity::icon` documents:
/// a codepoint with an emoji form can come back double-width from the
/// font stack and shove every column after it out of alignment.
/// The transport glyphs are `pub`: the maximized player draws the
/// same six controls, and a second copy of this alphabet would let the
/// two surfaces drift apart.
const GLYPH_QUEUE: char = '≡'; // U+2261 identical to
pub const GLYPH_PLAY: char = '▸'; // U+25B8 black right-pointing small triangle
pub const GLYPH_PAUSE: char = '‖'; // U+2016 double vertical line
pub const GLYPH_LOOP: char = '↻'; // U+21BB clockwise open circle arrow
pub const GLYPH_SHUFFLE: char = '⇄'; // U+21C4 rightwards arrow over leftwards arrow
pub const GLYPH_SPEED: char = '×'; // U+00D7 multiplication sign
pub const GLYPH_EQ: &str = "▂▅▃"; // U+2582/2585/2583 lower block eighths
pub const GLYPH_CROSSFADE: &str = "◢◣"; // U+25E2/25E3 black lower right/left triangle
const TRACK_DONE: char = '━'; // U+2501 box drawings heavy horizontal
const TRACK_LEFT: char = '─'; // U+2500 box drawings light horizontal
pub const TRACK_HEAD: char = '●'; // U+25CF black circle
pub const TRACK_HEAD_DRAG: char = '◆'; // U+25C6 black diamond

/// Minimum track the scrubber is ever drawn with; narrower than this
/// the whole progress element is dropped like any other, rather than
/// shown as a bar too short to read a position off.
pub const TRACK_MIN: usize = 6;

/// Brightness plate behind the selected element. A plate, not a
/// border: every element here is one to six cells wide, and a box
/// around each would be more ink than the glyph it frames.
pub const SELECT_BG: f32 = 0.20;
/// The same plate, brighter, while the scrubber is being dragged, so
/// scrub mode never reads as merely selected.
pub const SCRUB_BG: f32 = 0.34;

/// How far `speed` has to sit from 1.0 before it's worth a readout;
/// matches the two decimals the readout itself prints.
const SPEED_EPSILON: f64 = 0.005;

/// Seconds one arrow key moves the scrub target, and the coarse step
/// shift takes instead.
pub const SCRUB_STEP: f64 = 5.0;
pub const SCRUB_STEP_COARSE: f64 = 30.0;

/// How many of `lengths` (each element's own display width, in
/// priority order) fit within `available_width` once a `sep`-cell
/// separator between every pair is accounted for. Dropping whole
/// lowest-priority (rightmost) elements once the full line stops
/// fitting, rather than cutting the last one off mid-element (half a
/// crossfade icon, or a scrubber too short to show a position, which
/// is what a plain per-cell truncation at `available_width` produces).
pub fn segments_that_fit(lengths: &[usize], available_width: usize, sep: usize) -> usize {
    let mut used = 0usize;
    for (i, &len) in lengths.iter().enumerate() {
        let needed = len + if i > 0 { sep } else { 0 };
        if used + needed > available_width {
            return i;
        }
        used += needed;
    }
    lengths.len()
}

/// One selectable control on the bar. Named `Field` from when the bar
/// was a line of `label: value` text; it is now the selection cursor's
/// alphabet, so the progress bar and the play/pause glyph (both things
/// you can land on and press enter against) are in it too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Pause,
    Loop,
    Shuffle,
    Speed,
    Eq,
    Crossfade,
    Progress,
}

/// Same left-to-right order the bar itself draws them in, so left and
/// right on the selection cursor move the way the row looks.
pub const FIELDS: [Field; 7] = [
    Field::Pause,
    Field::Loop,
    Field::Shuffle,
    Field::Speed,
    Field::Eq,
    Field::Crossfade,
    Field::Progress,
];

impl Field {
    fn index(self) -> usize {
        FIELDS
            .iter()
            .position(|f| *f == self)
            .expect("every Field variant is listed in FIELDS")
    }

    pub fn next(self) -> Self {
        FIELDS[(self.index() + 1) % FIELDS.len()]
    }

    pub fn prev(self) -> Self {
        FIELDS[(self.index() + FIELDS.len() - 1) % FIELDS.len()]
    }
}

/// What a keypress does to a transport cursor. Pure, so the two
/// surfaces that carry one (this bar and the maximized player) share a
/// single answer to "which control is selected and how does a key move
/// it"; main.rs keeps only the dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nav {
    Prev,
    Next,
    Activate,
    /// Consumed on purpose: the key means nothing here and must not
    /// fall through to whatever it means in another pane.
    Inert,
}

/// Which key set a surface answers to. The bar is a one-row strip that
/// owns h/l outright and has nothing above or below it, so it also
/// swallows up/down. The maximized player is a whole screen where
/// those letters still have to reach their global bindings ('l' is
/// cycle-loop), so there it is arrows only and anything else falls
/// through untouched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavKeys {
    BarStrip,
    ArrowsOnly,
}

pub fn nav(code: KeyCode, keys: NavKeys) -> Option<Nav> {
    match (code, keys) {
        (KeyCode::Right, _) | (KeyCode::Char('l'), NavKeys::BarStrip) => Some(Nav::Next),
        (KeyCode::Left, _) | (KeyCode::Char('h'), NavKeys::BarStrip) => Some(Nav::Prev),
        (KeyCode::Enter, _) => Some(Nav::Activate),
        (KeyCode::Up | KeyCode::Down, NavKeys::BarStrip) => Some(Nav::Inert),
        _ => None,
    }
}

/// Live scrub state while enter-on-the-progress-bar holds the drag
/// open. `target_secs` is ours, not mpv's: the bar redraws from it the
/// instant a key lands and the actual seek is coalesced behind it, so
/// holding an arrow key can't queue up an ipc round-trip per keypress.
/// `duration_secs` is captured on entry for the same reason, so every
/// step clamps without asking mpv again.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Scrub {
    pub target_secs: f64,
    pub duration_secs: Option<f64>,
}

impl Scrub {
    pub fn step(self, delta: f64) -> Self {
        let target = self.target_secs + delta;
        let target = match self.duration_secs {
            Some(d) if d > 0.0 => target.clamp(0.0, d),
            // a stream mpv never reported a duration for has no end
            // to clamp against, only a start.
            _ => target.max(0.0),
        };
        Scrub {
            target_secs: target,
            ..self
        }
    }

    /// Absolute target at a fraction of the duration: what a click or a
    /// drag on the track means, against the same captured duration
    /// `step` clamps to. A stream mpv never reported a duration for has
    /// no fraction to land on, so the drag stays where it was rather
    /// than jumping to zero.
    pub fn at_fraction(self, frac: f64) -> Self {
        match self.duration_secs {
            Some(d) if d > 0.0 => Scrub {
                target_secs: frac.clamp(0.0, 1.0) * d,
                ..self
            },
            _ => self,
        }
    }
}

/// Bottom playback bar: controls and current input mode, nothing
/// else. It deliberately has no status/message line: transient
/// messages ("now playing: x", "error loading file", "starred") used
/// to land here, which meant the one permanently-visible strip of the
/// UI was mostly occupied by stale sentences about things that had
/// already happened. Anything genuinely worth surfacing goes to the
/// notification overlay instead; this bar only ever shows state that
/// is still true.
/// Every control is a glyph whose brightness is its state (dim off,
/// bright on), not a `label: value` pair; see the glyph constants
/// above for why those particular codepoints.
/// Resizable (bottom bar, vertical axis only); more rows reveal more
/// lines gracefully, fewer rows collapse the mode indicator into the
/// control line rather than dropping it.
pub struct ControlsBar<'a> {
    /// `true` while wasd/arrows move focus between panes and nothing
    /// else responds to the keyboard; see `InputMode` in main.rs.
    pub move_mode: bool,
    /// Which pane currently has focus, named so the mode line answers
    /// "where will the next keypress land" outright instead of leaving
    /// it to be inferred from a highlighted border somewhere else.
    pub focus_label: &'a str,
    /// Whether that pane is this bar, which is what decides whether
    /// the mode line hints the selection keys or the focus keys.
    pub focused: bool,
    pub queue_len: usize,
    pub loop_mode: LoopMode,
    pub shuffle_on: bool,
    pub speed: f64,
    /// Whether the eq is doing anything at all; the preset's name was
    /// the kind of word this line stopped printing, and the eq menu
    /// (enter on the eq glyph) is where the name belongs.
    pub eq_active: bool,
    pub crossfade_on: bool,
    pub paused: bool,
    /// Seconds left of a between-track gap, if one is running.
    ///
    /// During a gap mpv really is paused, so without this the bar
    /// shows a bare pause glyph and the honest reading of the screen
    /// is "something stopped this". The countdown replaces a
    /// misleading indicator rather than covering anything up, which is
    /// also why the gap does not take the album art panel the way
    /// `silence` does: ten seconds of countdown over the cover, every
    /// track, would be worse than the problem.
    pub gap_remaining: Option<f64>,
    /// Playback position the scrubber draws from; while `scrubbing`
    /// this is the pending drag target rather than mpv's own
    /// `time-pos`, so the bar tracks the keypress and not the socket.
    pub elapsed_secs: Option<f64>,
    pub duration_secs: Option<f64>,
    /// Field that changed most recently and how "hot" it still is:
    /// `1.0` right after the toggle, decaying to `0.0` back at the
    /// element's resting brightness. Acknowledges a toggle by briefly
    /// lifting the glyph that already has a permanent spot on this
    /// line, instead of writing a one-off sentence to a status row
    /// (which reads as the app typing a message at you for something
    /// the bar already shows).
    pub flash: Option<(Field, f32)>,
    /// The bar's own selection cursor, if it currently has focus;
    /// drawn as a background plate rather than a brightness tier, so
    /// brightness stays free to mean on/off (see `draw_segments`).
    pub selected: Option<Field>,
    /// `true` while enter-on-the-scrubber has a drag open.
    pub scrubbing: bool,
    pub resize_hint: Option<&'a str>,
}

/// How the mode line advertises the shellsong console, spelled from
/// `shellsong::OPEN_KEY` rather than typed out: this bar is the only
/// permanently-visible thing on screen, and the console (a whole
/// command language, see docs/shellsong.md) was advertised nowhere.
fn console_hint() -> String {
    format!(
        "{} opens the console",
        crate::keymap::key_label(crate::shellsong::OPEN_KEY)
    )
}

impl Widget for ControlsBar<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if let Some(hint) = self.resize_hint {
            draw_line(buf, area, area.y, hint, shade(NEAR_WHITE));
        }

        let (y, inline_mode) = self.control_row(area);
        let mode_label = self.mode_label();
        let mode_on_its_own_row = inline_mode.is_none();
        if mode_on_its_own_row {
            // the row directly above the controls, which `control_row`
            // already accounted for.
            let y = y - 1;
            let hint = if self.move_mode {
                "wasd/arrows: focus pane, enter: interact".to_string()
            } else if self.scrubbing {
                "arrows: seek, shift: coarse, enter/esc: done".to_string()
            } else {
                let keys = if self.focused {
                    "arrows: pick, enter: use, tab: move focus"
                } else {
                    "tab: move focus"
                };
                // the console is reachable from every one of these
                // states and had no glyph, no row and no hint
                // anywhere, so this is the only permanently-visible
                // place it can be advertised. appended only when the
                // whole thing fits, since half a key taught is worse
                // than the line it crowded out; move mode and a scrub
                // drag are the two states where `:` is genuinely inert.
                let room = area.width as usize;
                let with = format!("{keys}, {}", console_hint());
                if width(&mode_label) + 2 + width(&with) <= room {
                    with
                } else {
                    keys.to_string()
                }
            };
            let x = draw_chars(
                buf,
                area.x,
                area.x + area.width,
                y,
                &mode_label,
                // bright in move mode: it's the transient one, and
                // the one where every other key is inert.
                if self.move_mode {
                    shade(WHITE)
                } else {
                    shade(DIM)
                },
                None,
            );
            draw_chars(buf, x, area.x + area.width, y, "  ", shade(DIM), None);
            draw_chars(
                buf,
                x + 2,
                area.x + area.width,
                y,
                &hint,
                shade(NEAR_BLACK),
                None,
            );
        }
        if y >= area.y + area.height {
            return;
        }

        let segments = self.segments(inline_mode.as_deref());
        for placed in place(&segments, area, y) {
            draw_placed(
                buf,
                area,
                &placed,
                self.flash,
                self.selected,
                self.scrubbing,
            );
        }
    }
}

impl ControlsBar<'_> {
    /// Which row the controls land on, and the mode label to put in
    /// front of them when the bar is too short to give it a row of its
    /// own. Shared by `render` and `layout`: a click has to land on the
    /// line that was actually drawn.
    fn control_row(&self, area: Rect) -> (u16, Option<String>) {
        let mode_y = area.y + u16::from(self.resize_hint.is_some());
        // the mode indicator gets a whole row to itself whenever
        // there's one to spare: which mode you're in changes what
        // every other key does, so it can't be something you have to
        // hunt for. squeezed down to the minimum bar height it
        // becomes the control line's first segment instead of being
        // dropped, since that's the one thing here you can't infer
        // from the rest of the screen.
        let own_row = (area.y + area.height).saturating_sub(mode_y) >= 2;
        (
            mode_y + u16::from(own_row),
            (!own_row).then(|| self.mode_label()),
        )
    }

    fn mode_label(&self) -> String {
        if self.move_mode {
            format!("-- MOVE: {} --", self.focus_label)
        } else {
            format!("-- INTERACT: {} --", self.focus_label)
        }
    }

    /// Where every control the bar drew ended up. `pub` for the host's
    /// hit map (see `hit::HitMap`): these columns are only knowable
    /// from the same fit pass the render does, and a guess at them
    /// would put a click on a control a narrow bar had dropped.
    pub fn layout(&self, area: Rect) -> BarLayout {
        let (y, inline_mode) = self.control_row(area);
        if y >= area.y + area.height {
            return BarLayout::default();
        }
        let segments = self.segments(inline_mode.as_deref());
        let placed = place(&segments, area, y);
        BarLayout {
            controls: placed
                .iter()
                .filter_map(|p| p.seg.field.map(|f| (f, p.rect)))
                .collect(),
            track: placed.iter().find_map(|p| p.track),
        }
    }

    /// The control line's elements in priority order, which is also
    /// draw order: the scrubber is last because it's the one element
    /// that stretches, so it ends flush with the right edge.
    fn segments(&self, mode_inline: Option<&str>) -> Vec<Segment> {
        let queue = match mode_inline {
            Some(mode) => format!("{mode}  {GLYPH_QUEUE}{}", self.queue_len),
            None => format!("{GLYPH_QUEUE}{}", self.queue_len),
        };
        let speed_changed = (self.speed - 1.0).abs() >= SPEED_EPSILON;
        let readout = readouts(self.elapsed_secs, self.duration_secs);
        vec![
            Segment {
                field: None,
                lit: self.queue_len > 0,
                body: Body::Glyph(queue),
            },
            Segment {
                field: Some(Field::Pause),
                // lit means "audio is moving", so the play triangle is
                // the bright state and the pause bars the dim one. a
                // gap is dim too: nothing is moving, it is just going
                // to on its own.
                lit: !self.paused && self.gap_remaining.is_none(),
                body: Body::Glyph(match self.gap_remaining {
                    // ceil, not round: a gap with any time left reads
                    // as at least 1s, never a 0s that sits there.
                    Some(left) => format!("{GLYPH_PAUSE} {}s", left.max(0.0).ceil() as u64),
                    None if self.paused => GLYPH_PAUSE.to_string(),
                    None => GLYPH_PLAY.to_string(),
                }),
            },
            Segment {
                field: Some(Field::Loop),
                lit: self.loop_mode != LoopMode::Off,
                // off and all share one glyph and differ by
                // brightness; one gets the digit, since "repeat just
                // this track" isn't a brightness of "repeat".
                body: Body::Glyph(match self.loop_mode {
                    LoopMode::Off | LoopMode::All => GLYPH_LOOP.to_string(),
                    LoopMode::One => format!("{GLYPH_LOOP}1"),
                }),
            },
            Segment {
                field: Some(Field::Shuffle),
                lit: self.shuffle_on,
                body: Body::Glyph(GLYPH_SHUFFLE.to_string()),
            },
            Segment {
                field: Some(Field::Speed),
                lit: speed_changed,
                // 1.00x is "no change" and needs no readout, just the
                // sign; any other rate is a number no glyph carries.
                body: Body::Glyph(if speed_changed {
                    format!("{GLYPH_SPEED}{:.2}", self.speed)
                } else {
                    GLYPH_SPEED.to_string()
                }),
            },
            Segment {
                field: Some(Field::Eq),
                lit: self.eq_active,
                body: Body::Glyph(GLYPH_EQ.to_string()),
            },
            Segment {
                field: Some(Field::Crossfade),
                lit: self.crossfade_on,
                body: Body::Glyph(GLYPH_CROSSFADE.to_string()),
            },
            Segment {
                field: Some(Field::Progress),
                lit: self.elapsed_secs.is_some(),
                body: Body::Track {
                    // padded to one width for the whole song, so the
                    // track cannot shift a column mid-drag; see
                    // `progress::readouts`.
                    elapsed: readout.0,
                    total: readout.1,
                    frac: match (self.elapsed_secs, self.duration_secs) {
                        (Some(e), Some(d)) if d > 0.0 => (e / d).clamp(0.0, 1.0),
                        _ => 0.0,
                    },
                },
            },
        ]
    }
}

enum Body {
    /// A fixed-width glyph (or glyph plus numeric readout).
    Glyph(String),
    /// The scrubber: two fixed time readouts around a track that
    /// absorbs whatever width the fixed elements left over.
    Track {
        elapsed: String,
        total: String,
        frac: f64,
    },
}

struct Segment {
    field: Option<Field>,
    /// Resting brightness: an active control is `BRIGHT`, an inactive
    /// one `DIM`. This is the whole point of dropping the labels; off
    /// is a dim glyph, not the word "off".
    lit: bool,
    body: Body,
}

impl Segment {
    fn min_width(&self) -> usize {
        match &self.body {
            Body::Glyph(s) => width(s),
            // two spaces: one either side of the track.
            Body::Track { elapsed, total, .. } => width(elapsed) + width(total) + 2 + TRACK_MIN,
        }
    }
}

/// Where each control the bar drew ended up, for hit-testing a click
/// against the row that was on screen.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BarLayout {
    /// Only the controls that actually fit; a narrow bar drops whole
    /// trailing elements and they have no columns at all.
    pub controls: Vec<(Field, Rect)>,
    /// The scrubber's track cells only. The time readouts either side
    /// are part of the element, but are not positions on the track, so
    /// a fraction measured against the whole element would be wrong.
    pub track: Option<Rect>,
}

/// One element as the row draws it: the text at the width the scrubber
/// ended up with, the columns it occupies, and (for the scrubber) the
/// track cells inside those.
struct Placed<'s> {
    seg: &'s Segment,
    text: String,
    rect: Rect,
    track: Option<Rect>,
    /// Cells `track` was built at, which stays right even where the
    /// rect above got clipped by the bar's right edge; the drag redraw
    /// has to ask for the same width or the row shifts mid-drag.
    track_cells: usize,
}

/// Lays the control line out once. `render` paints from this and
/// `ControlsBar::layout` hit-tests against it, which is the whole
/// point: the fit pass (`segments_that_fit`) and the stretching
/// scrubber mean the columns are not derivable from the field list.
fn place<'s>(segments: &'s [Segment], area: Rect, y: u16) -> Vec<Placed<'s>> {
    let limit = area.x + area.width;
    let avail = area.width as usize;
    let mins: Vec<usize> = segments.iter().map(Segment::min_width).collect();
    let fits = segments_that_fit(&mins, avail, 2);
    let used: usize = mins[..fits].iter().sum::<usize>() + fits.saturating_sub(1) * 2;
    // whatever the fixed elements left over goes to the scrubber, so
    // the bar reaches the right edge instead of leaving a ragged gap
    // there; it's the only element that stretches.
    let slack = avail.saturating_sub(used);

    let mut out = Vec::with_capacity(fits);
    let mut x = area.x;
    for (i, seg) in segments.iter().take(fits).enumerate() {
        if i > 0 {
            x += 2;
        }
        if x >= limit {
            break;
        }
        let (text, track_cells) = match &seg.body {
            Body::Glyph(s) => (s.clone(), None),
            Body::Track {
                elapsed,
                total,
                frac,
            } => {
                let cells = TRACK_MIN + slack;
                (
                    format!("{elapsed} {} {total}", track(*frac, cells, false)),
                    Some((width(elapsed) + 1, cells)),
                )
            }
        };
        let drawn = (width(&text) as u16).min(limit - x);
        out.push(Placed {
            seg,
            track_cells: track_cells.map_or(TRACK_MIN, |(_, cells)| cells),
            track: track_cells.and_then(|(offset, cells)| {
                let tx = x + offset as u16;
                (tx < limit).then(|| Rect {
                    x: tx,
                    y,
                    width: (cells as u16).min(limit - tx),
                    height: 1,
                })
            }),
            rect: Rect {
                x,
                y,
                width: drawn,
                height: 1,
            },
            text,
        });
        x += drawn;
    }
    out
}

fn draw_placed(
    buf: &mut Buffer,
    area: Rect,
    placed: &Placed,
    flash: Option<(Field, f32)>,
    selected: Option<Field>,
    scrubbing: bool,
) {
    let limit = area.x + area.width;
    let seg = placed.seg;
    let is_selected = seg.field.is_some() && seg.field == selected;
    let dragging = is_selected && scrubbing && seg.field == Some(Field::Progress);
    let flash_level = match (seg.field, flash) {
        (Some(f), Some((hot_field, level))) if f == hot_field => level,
        _ => 0.0,
    };
    // the plate marks the cursor, which leaves brightness free to
    // keep meaning on/off; an inactive glyph is only lifted to MID
    // so it stays legible against the plate, still well below an
    // active one. flash lifts from wherever that lands.
    let base = if dragging {
        WHITE
    } else if seg.lit {
        BRIGHT
    } else {
        DIM
    };
    let base = if is_selected { base.max(MID) } else { base };
    let level = lerp(base, WHITE, flash_level);
    let bg = if dragging {
        Some(shade(SCRUB_BG))
    } else if is_selected {
        Some(shade(SELECT_BG))
    } else {
        None
    };
    let x = placed.rect.x;
    let y = placed.rect.y;
    if x > area.x {
        draw_chars(buf, x - 2, limit, y, "  ", shade(DIM), None);
    }
    // the drag head is a different glyph, so the text has to be built
    // knowing the mode; `place` has no business knowing it, which is
    // the one thing redrawn here rather than reused.
    let text = match (&seg.body, dragging) {
        (
            Body::Track {
                elapsed,
                total,
                frac,
            },
            true,
        ) => format!(
            "{elapsed} {} {total}",
            track(*frac, placed.track_cells, true)
        ),
        _ => placed.text.clone(),
    };
    // the plate is exactly `placed.rect`, which is also exactly what
    // `layout` reports as this control's clickable cells. it used to
    // pad a cell each side so a single-glyph element read as a plate
    // rather than one stray lit cell, which looked better and lied:
    // the lit edge was not clickable, so clicking what was visibly
    // highlighted missed. lit and clickable are one thing now.
    //
    // the alternative was growing the rect to the old plate. rejected:
    // controls sit 2 cells apart, so padding both sides would tile the
    // whole bar and a one-cell miss would press the *neighbour*. on a
    // transport bar that means skipping a track when you meant to miss,
    // and a miss that does nothing is the better failure.
    if let Some(bg) = bg {
        let end = placed.rect.x.saturating_add(placed.rect.width);
        fill_bg(buf, placed.rect.x.max(area.x), end.min(limit), y, bg);
    }
    draw_chars(buf, x, limit, y, &text, shade(level), bg);
}

/// Same scrubber convention as `ui::progress::bar` (heavy track
/// behind the head, light ahead of it); the head swaps to a diamond
/// while dragging, so scrub mode shows in the bar itself rather than
/// only in the plate behind it. `pub` for the maximized player, which
/// draws the same scrubber at its own width.
pub fn track(frac: f64, cells: usize, dragging: bool) -> String {
    let cells = cells.max(TRACK_MIN);
    let head = if dragging {
        TRACK_HEAD_DRAG
    } else {
        TRACK_HEAD
    };
    let pos = ((frac.clamp(0.0, 1.0) * (cells - 1) as f64).round() as usize).min(cells - 1);
    (0..cells)
        .map(|i| match i.cmp(&pos) {
            Ordering::Less => TRACK_DONE,
            Ordering::Equal => head,
            Ordering::Greater => TRACK_LEFT,
        })
        .collect()
}

/// One row of plate between two columns; `pub` for the player's
/// transport, which marks its own cursor the same way.
pub fn fill_bg(buf: &mut Buffer, from_x: u16, to_x: u16, y: u16, bg: Color) {
    for x in from_x..to_x {
        if let Some(cell) = buf.cell_mut((x, y)) {
            cell.set_bg(bg);
        }
    }
}

/// Draws `text` from `start_x` up to (never past) `limit`, returning
/// the next free column. Advances by each char's display width, not by
/// one per char, so a double-width glyph owns the two cells it
/// actually paints over.
fn draw_chars(
    buf: &mut Buffer,
    start_x: u16,
    limit: u16,
    y: u16,
    text: &str,
    color: Color,
    bg: Option<Color>,
) -> u16 {
    let budget = limit.saturating_sub(start_x) as usize;
    let mut x = start_x;
    for ch in truncate_to_width(text, budget).chars() {
        let cells = char_cells(ch) as u16;
        if let Some(cell) = buf.cell_mut((x, y)) {
            cell.set_char(ch);
            cell.set_fg(color);
            if let Some(bg) = bg {
                cell.set_bg(bg);
            }
        }
        // the trailing half of a wide glyph is blanked rather than
        // left holding whatever was under it, which would print twice.
        for off in 1..cells {
            if let Some(cell) = buf.cell_mut((x + off, y)) {
                cell.set_symbol("");
                if let Some(bg) = bg {
                    cell.set_bg(bg);
                }
            }
        }
        x += cells;
    }
    x
}

fn draw_line(buf: &mut Buffer, area: Rect, y: u16, text: &str, color: Color) {
    if y >= area.y + area.height {
        return;
    }
    let fitted = truncate_with_ellipsis(text, area.width as usize);
    draw_chars(buf, area.x, area.x + area.width, y, &fitted, color, None);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::testbuf::{buffer, grid};

    fn plain_bar() -> ControlsBar<'static> {
        ControlsBar {
            move_mode: false,
            focus_label: "bottom bar",
            focused: false,
            queue_len: 0,
            loop_mode: LoopMode::Off,
            shuffle_on: false,
            speed: 1.0,
            eq_active: false,
            crossfade_on: false,
            paused: false,
            gap_remaining: None,
            elapsed_secs: Some(30.0),
            duration_secs: Some(120.0),
            flash: None,
            selected: None,
            scrubbing: false,
            resize_hint: None,
        }
    }

    fn bar_at(width: u16, height: u16, f: impl FnOnce(&mut ControlsBar)) -> Buffer {
        let mut cb = plain_bar();
        f(&mut cb);
        let area = Rect::new(0, 0, width, height);
        let mut buf = buffer(width, height);
        cb.render(area, &mut buf);
        buf
    }

    /// The layout the host hit-tests against, and the buffer the same
    /// bar drew; one `f` configures both, which is the only way to
    /// assert the two agree.
    fn bar_and_layout(
        width: u16,
        height: u16,
        f: impl Fn(&mut ControlsBar),
    ) -> (Buffer, BarLayout) {
        let area = Rect::new(0, 0, width, height);
        let mut probe = plain_bar();
        f(&mut probe);
        let layout = probe.layout(area);
        (bar_at(width, height, f), layout)
    }

    /// Column the first `ch` sits in on row `y`; every assertion below
    /// locates an element by its glyph rather than a hardcoded column,
    /// so reordering the row doesn't invalidate the brightness tests.
    fn col_of(buf: &Buffer, y: u16, ch: char) -> u16 {
        (buf.area.x..buf.area.x + buf.area.width)
            .find(|x| {
                buf.cell((*x, y))
                    .is_some_and(|c| c.symbol().starts_with(ch))
            })
            .unwrap_or_else(|| panic!("no {ch:?} on row {y}:\n{}", grid(buf)))
    }

    fn fg_of(buf: &Buffer, y: u16, ch: char) -> Color {
        buf.cell((col_of(buf, y, ch), y)).unwrap().fg
    }

    fn bg_of(buf: &Buffer, y: u16, ch: char) -> Color {
        buf.cell((col_of(buf, y, ch), y)).unwrap().bg
    }

    #[test]
    fn every_segment_fits_when_there_is_room() {
        let lengths = [5, 10, 3]; // + 2*2 separators = 22
        assert_eq!(segments_that_fit(&lengths, 22, 2), 3);
        assert_eq!(segments_that_fit(&lengths, 100, 2), 3);
        // a wider separator is what the player's word-labeled row
        // spends its extra room on, and it counts against the fit.
        assert_eq!(segments_that_fit(&lengths, 22, 3), 2);
    }

    #[test]
    fn drops_whole_trailing_segments_instead_of_cutting_one_mid_element() {
        // 8 cells + "  " + a 14-cell element = 24; a width of 20 can't
        // fit the second element at all, so it's dropped entirely
        // rather than shown as the left 12 cells of itself.
        let lengths = [8, 14];
        assert_eq!(segments_that_fit(&lengths, 20, 2), 1);
    }

    #[test]
    fn keeps_the_first_segment_even_if_nothing_else_fits() {
        let lengths = [8, 14];
        assert_eq!(segments_that_fit(&lengths, 10, 2), 1);
    }

    #[test]
    fn returns_zero_when_not_even_the_first_segment_fits() {
        let lengths = [8, 14];
        assert_eq!(segments_that_fit(&lengths, 5, 2), 0);
    }

    #[test]
    fn empty_segments_list_fits_trivially() {
        assert_eq!(segments_that_fit(&[], 0, 2), 0);
    }

    #[test]
    fn the_bar_owns_hl_and_updown_but_the_player_leaves_them_alone() {
        for keys in [NavKeys::BarStrip, NavKeys::ArrowsOnly] {
            assert_eq!(nav(KeyCode::Right, keys), Some(Nav::Next));
            assert_eq!(nav(KeyCode::Left, keys), Some(Nav::Prev));
            assert_eq!(nav(KeyCode::Enter, keys), Some(Nav::Activate));
            assert_eq!(nav(KeyCode::Char('x'), keys), None);
        }
        assert_eq!(nav(KeyCode::Char('l'), NavKeys::BarStrip), Some(Nav::Next));
        assert_eq!(nav(KeyCode::Char('h'), NavKeys::BarStrip), Some(Nav::Prev));
        assert_eq!(nav(KeyCode::Up, NavKeys::BarStrip), Some(Nav::Inert));
        assert_eq!(nav(KeyCode::Down, NavKeys::BarStrip), Some(Nav::Inert));
        // on a full screen 'l' is still the global cycle-loop key and
        // up/down still mean whatever they mean elsewhere.
        assert_eq!(nav(KeyCode::Char('l'), NavKeys::ArrowsOnly), None);
        assert_eq!(nav(KeyCode::Char('h'), NavKeys::ArrowsOnly), None);
        assert_eq!(nav(KeyCode::Up, NavKeys::ArrowsOnly), None);
        assert_eq!(nav(KeyCode::Down, NavKeys::ArrowsOnly), None);
    }

    #[test]
    fn every_glyph_is_exactly_one_cell_per_codepoint() {
        // the whole reason these codepoints were picked: a glyph the
        // font stack renders emoji-wide would silently eat the cell
        // after it and shift the rest of the row.
        for ch in [
            GLYPH_QUEUE,
            GLYPH_PLAY,
            GLYPH_PAUSE,
            GLYPH_LOOP,
            GLYPH_SHUFFLE,
            GLYPH_SPEED,
            TRACK_DONE,
            TRACK_LEFT,
            TRACK_HEAD,
            TRACK_HEAD_DRAG,
        ] {
            assert_eq!(char_cells(ch), 1, "{ch:?} is not one cell wide");
        }
        assert_eq!(width(GLYPH_EQ), GLYPH_EQ.chars().count());
        assert_eq!(width(GLYPH_CROSSFADE), GLYPH_CROSSFADE.chars().count());
    }

    #[test]
    fn the_control_line_has_no_words_on_it() {
        let buf = bar_at(100, 2, |cb| {
            cb.loop_mode = LoopMode::All;
            cb.shuffle_on = true;
            cb.crossfade_on = true;
            cb.eq_active = true;
        });
        let line = grid(&buf).lines().nth(1).unwrap().to_string();
        for word in [
            "loop",
            "shuffle",
            "crossfade",
            "speed",
            "queue",
            "eq",
            "off",
        ] {
            assert!(!line.contains(word), "{word:?} still printed:\n{line}");
        }
    }

    #[test]
    fn an_off_control_renders_dim_and_an_on_control_bright() {
        let off = bar_at(100, 2, |_| {});
        assert_eq!(fg_of(&off, 1, GLYPH_SHUFFLE), shade(DIM));
        let on = bar_at(100, 2, |cb| cb.shuffle_on = true);
        assert_eq!(fg_of(&on, 1, GLYPH_SHUFFLE), shade(BRIGHT));
    }

    #[test]
    fn loop_off_and_all_differ_only_in_brightness_and_one_adds_a_digit() {
        let off = bar_at(100, 2, |_| {});
        let all = bar_at(100, 2, |cb| cb.loop_mode = LoopMode::All);
        let one = bar_at(100, 2, |cb| cb.loop_mode = LoopMode::One);
        assert_eq!(fg_of(&off, 1, GLYPH_LOOP), shade(DIM));
        assert_eq!(fg_of(&all, 1, GLYPH_LOOP), shade(BRIGHT));
        assert_eq!(fg_of(&one, 1, GLYPH_LOOP), shade(BRIGHT));

        // the three states have to be told apart without reading a
        // word: off/all by brightness, one by the digit beside it.
        let digit_col = col_of(&one, 1, GLYPH_LOOP) + 1;
        assert_eq!(one.cell((digit_col, 1)).unwrap().symbol(), "1");
        let all_next = col_of(&all, 1, GLYPH_LOOP) + 1;
        assert_eq!(all.cell((all_next, 1)).unwrap().symbol(), " ");
    }

    /// During a gap mpv really is paused, so a bare pause glyph reads
    /// as "something stopped this". The countdown says what is
    /// actually happening, and stays dim because nothing is moving.
    #[test]
    fn a_gap_counts_down_where_the_pause_glyph_would_be() {
        let gapping = bar_at(100, 2, |cb| {
            cb.paused = true;
            cb.gap_remaining = Some(6.2);
        });
        let line = grid(&gapping).lines().nth(1).unwrap().to_string();
        assert!(line.contains(&format!("{GLYPH_PAUSE} 7s")), "{line}");
        assert_eq!(fg_of(&gapping, 1, GLYPH_PAUSE), shade(DIM));
    }

    /// Ceil, not round: any time left reads as at least 1s. A "0s"
    /// sitting on the bar would look stuck.
    #[test]
    fn a_nearly_finished_gap_still_reads_a_whole_second() {
        let bar = bar_at(100, 2, |cb| {
            cb.paused = true;
            cb.gap_remaining = Some(0.2);
        });
        let line = grid(&bar).lines().nth(1).unwrap().to_string();
        assert!(line.contains(&format!("{GLYPH_PAUSE} 1s")), "{line}");
    }

    /// And with no gap the bar is exactly what it was.
    /// And with no gap the transport glyph is exactly what it was:
    /// nothing trails it. Asserted on the cell after the glyph rather
    /// than on the whole line, which carries other letters.
    #[test]
    fn no_gap_leaves_the_transport_glyph_alone() {
        let playing = bar_at(100, 2, |_| {});
        let at = col_of(&playing, 1, GLYPH_PLAY);
        assert_eq!(playing.cell((at + 1, 1)).unwrap().symbol(), " ");
    }

    /// Prints the bar either way, so the countdown can be read in
    /// place rather than inferred from a substring match:
    /// `cargo test --lib controlsbar::tests::print -- --nocapture`.
    #[test]
    fn print_the_gap_bar() {
        for gap in [None, Some(6.2)] {
            let bar = bar_at(72, 2, |cb| {
                cb.paused = gap.is_some();
                cb.gap_remaining = gap;
                cb.elapsed_secs = Some(44.0);
                cb.duration_secs = Some(214.0);
            });
            println!("--- gap={gap:?}\n{}", grid(&bar));
        }
    }

    #[test]
    fn pause_shows_playback_state_not_the_key_it_would_press() {
        let playing = bar_at(100, 2, |_| {});
        assert_eq!(fg_of(&playing, 1, GLYPH_PLAY), shade(BRIGHT));
        let paused = bar_at(100, 2, |cb| cb.paused = true);
        assert_eq!(fg_of(&paused, 1, GLYPH_PAUSE), shade(DIM));
    }

    #[test]
    fn speed_drops_its_readout_at_1x_and_shows_it_otherwise() {
        let normal = bar_at(100, 2, |_| {});
        let col = col_of(&normal, 1, GLYPH_SPEED);
        assert_eq!(normal.cell((col + 1, 1)).unwrap().symbol(), " ");
        assert_eq!(fg_of(&normal, 1, GLYPH_SPEED), shade(DIM));

        let fast = bar_at(100, 2, |cb| cb.speed = 1.25);
        let line = grid(&fast).lines().nth(1).unwrap().to_string();
        assert!(line.contains("×1.25"), "no speed readout:\n{line}");
        assert_eq!(fg_of(&fast, 1, GLYPH_SPEED), shade(BRIGHT));
    }

    /// The plate used to pad a cell either side. It looked better and
    /// it lied: the padding was not in the rect `layout` reports, so
    /// clicking the lit edge of the selected control missed. The plate
    /// is the rect now, and the cells either side go back to the row.
    #[test]
    fn the_selected_element_is_plated_over_exactly_its_own_cells() {
        let buf = bar_at(100, 2, |cb| cb.selected = Some(Field::Shuffle));
        let col = col_of(&buf, 1, GLYPH_SHUFFLE);
        assert_eq!(bg_of(&buf, 1, GLYPH_SHUFFLE), shade(SELECT_BG));
        assert_ne!(
            buf.cell((col - 1, 1)).unwrap().bg,
            shade(SELECT_BG),
            "the cell left of a single-glyph control is not part of it"
        );
        assert_ne!(
            buf.cell((col + 1, 1)).unwrap().bg,
            shade(SELECT_BG),
            "nor the cell right of it"
        );
        // nothing else on the row is plated, and an unselected
        // neighbour keeps its own state brightness.
        assert_eq!(bg_of(&buf, 1, GLYPH_LOOP), Color::Reset);
        assert_eq!(fg_of(&buf, 1, GLYPH_LOOP), shade(DIM));
    }

    #[test]
    fn selecting_an_off_control_lifts_it_enough_to_read_off_the_plate() {
        let buf = bar_at(100, 2, |cb| cb.selected = Some(Field::Shuffle));
        // still clearly below an active control, so the plate marks
        // the cursor without faking an "on" state.
        assert_eq!(fg_of(&buf, 1, GLYPH_SHUFFLE), shade(MID));
        const { assert!(MID < BRIGHT) };
    }

    #[test]
    fn scrub_mode_renders_differently_from_a_merely_selected_scrubber() {
        let selected = bar_at(100, 2, |cb| cb.selected = Some(Field::Progress));
        let dragging = bar_at(100, 2, |cb| {
            cb.selected = Some(Field::Progress);
            cb.scrubbing = true;
        });
        // the head glyph changes, so the mode is visible in the bar
        // and not only in the plate behind it.
        assert_eq!(fg_of(&selected, 1, TRACK_HEAD), shade(BRIGHT));
        assert!(!grid(&selected).contains(TRACK_HEAD_DRAG));
        assert_eq!(fg_of(&dragging, 1, TRACK_HEAD_DRAG), shade(WHITE));
        assert!(!grid(&dragging).contains(TRACK_HEAD));
        assert_eq!(bg_of(&dragging, 1, TRACK_HEAD_DRAG), shade(SCRUB_BG));
        const { assert!(SCRUB_BG > SELECT_BG) };
    }

    #[test]
    fn the_scrubber_absorbs_the_leftover_width_up_to_the_right_edge() {
        for width_cells in [60u16, 80, 120] {
            let buf = bar_at(width_cells, 2, |_| {});
            let line = grid(&buf).lines().nth(1).unwrap().to_string();
            assert!(
                !line.ends_with('.'),
                "{width_cells} cells left a gap at the edge:\n{line}"
            );
        }
    }

    #[test]
    fn a_narrow_bar_drops_whole_elements_from_the_right() {
        // wide enough for the glyph controls but not the scrubber, so
        // the scrubber goes whole rather than as a stub.
        let buf = bar_at(30, 2, |cb| {
            cb.loop_mode = LoopMode::All;
            cb.crossfade_on = true;
        });
        let line = grid(&buf).lines().nth(1).unwrap().to_string();
        assert!(line.contains(GLYPH_LOOP), "lost the loop glyph:\n{line}");
        assert!(!line.contains(TRACK_HEAD), "half a scrubber:\n{line}");
        assert!(!line.contains(TRACK_DONE), "half a scrubber:\n{line}");

        // narrower still and the trailing glyph controls go too, in
        // right-to-left order, never half-drawn.
        let tiny = bar_at(12, 2, |cb| cb.crossfade_on = true);
        let tiny_line = grid(&tiny).lines().nth(1).unwrap().to_string();
        assert!(
            !tiny_line.contains(GLYPH_CROSSFADE),
            "crossfade survived a 12-cell bar:\n{tiny_line}"
        );
        for ch in GLYPH_CROSSFADE.chars() {
            assert!(!tiny_line.contains(ch), "half a crossfade:\n{tiny_line}");
        }
    }

    #[test]
    fn the_mode_indicator_survives_the_shortest_possible_bar() {
        let buf = bar_at(100, 1, |_| {});
        let line = grid(&buf).lines().next().unwrap().to_string();
        assert!(line.contains("-- INTERACT: bottom bar --"), "{line}");
        // and it still gets the controls beside it, not instead of it.
        assert!(line.contains(GLYPH_QUEUE), "{line}");
    }

    #[test]
    fn the_mode_line_hints_the_keys_that_actually_do_something() {
        let idle = bar_at(100, 2, |_| {});
        assert!(grid(&idle).contains("tab: move focus"));
        let focused = bar_at(100, 2, |cb| cb.focused = true);
        assert!(grid(&focused).contains("arrows: pick, enter: use"));
        let dragging = bar_at(100, 2, |cb| {
            cb.focused = true;
            cb.scrubbing = true;
        });
        assert!(grid(&dragging).contains("arrows: seek"));
    }

    /// The console had no glyph on this bar, no row in any menu and no
    /// line in the help reference, so a whole command language was
    /// reachable only by guessing `:`. This line is the one thing
    /// always on screen, so it is where the key belongs.
    #[test]
    fn the_mode_line_advertises_the_console_whenever_it_fits() {
        let hint = console_hint();
        assert_eq!(hint, ": opens the console", "spelled from OPEN_KEY");
        for (w, focused) in [(100u16, false), (100, true), (80, false), (64, false)] {
            let buf = bar_at(w, 2, |cb| cb.focused = focused);
            assert!(
                grid(&buf).contains(&hint),
                "{w} cells, focused={focused}:\n{}",
                grid(&buf)
            );
        }
        // and it is dropped rather than clipped when the keys that
        // change what every other key does need the room: the line has
        // to end on the last whole hint, with no fragment of this one
        // left holding cells and teaching nothing.
        let tight = bar_at(72, 2, |cb| cb.focused = true);
        let g = grid(&tight);
        assert!(!g.contains(&hint), "no room, yet it claimed to fit:\n{g}");
        let row = g.lines().next().unwrap().trim_end_matches('.');
        assert!(
            row.ends_with("arrows: pick, enter: use, tab: move focus"),
            "a clipped hint left a fragment behind:\n{row}"
        );
    }

    /// `:` is inert in both of these, so advertising it there would be
    /// the same class of lie the queue hint was.
    #[test]
    fn the_console_is_not_advertised_where_its_key_is_inert() {
        let hint = console_hint();
        let moving = bar_at(140, 2, |cb| cb.move_mode = true);
        assert!(!grid(&moving).contains(&hint), "{}", grid(&moving));
        let dragging = bar_at(140, 2, |cb| {
            cb.focused = true;
            cb.scrubbing = true;
        });
        assert!(!grid(&dragging).contains(&hint), "{}", grid(&dragging));
    }

    #[test]
    fn a_flash_lifts_only_its_own_element() {
        let buf = bar_at(100, 2, |cb| cb.flash = Some((Field::Shuffle, 1.0)));
        assert_eq!(fg_of(&buf, 1, GLYPH_SHUFFLE), shade(WHITE));
        assert_eq!(fg_of(&buf, 1, GLYPH_LOOP), shade(DIM));
    }

    #[test]
    fn selection_moves_left_and_right_and_wraps_both_ways() {
        assert_eq!(Field::Pause.next(), Field::Loop);
        assert_eq!(Field::Loop.prev(), Field::Pause);
        // the ends wrap, so holding one arrow never dead-ends.
        assert_eq!(Field::Progress.next(), Field::Pause);
        assert_eq!(Field::Pause.prev(), Field::Progress);
        // every variant is reachable from any other by next alone.
        let mut seen = vec![Field::Pause];
        let mut f = Field::Pause;
        for _ in 1..FIELDS.len() {
            f = f.next();
            assert!(!seen.contains(&f), "{f:?} repeated before the cycle ended");
            seen.push(f);
        }
        assert_eq!(seen.len(), FIELDS.len());
    }

    #[test]
    fn a_scrub_step_clamps_to_the_track_at_both_ends() {
        let s = Scrub {
            target_secs: 10.0,
            duration_secs: Some(120.0),
        };
        assert_eq!(s.step(SCRUB_STEP).target_secs, 15.0);
        assert_eq!(s.step(-SCRUB_STEP).target_secs, 5.0);
        // seeking past either end parks at the end, it doesn't wrap or
        // hand mpv a negative/out-of-range absolute seek.
        assert_eq!(s.step(-60.0).target_secs, 0.0);
        assert_eq!(s.step(SCRUB_STEP_COARSE * 10.0).target_secs, 120.0);
    }

    #[test]
    fn a_scrub_lands_on_the_fraction_of_the_track_it_was_given() {
        let s = Scrub {
            target_secs: 10.0,
            duration_secs: Some(120.0),
        };
        assert_eq!(s.at_fraction(0.0).target_secs, 0.0);
        assert_eq!(s.at_fraction(0.5).target_secs, 60.0);
        assert_eq!(s.at_fraction(1.0).target_secs, 120.0);
        assert_eq!(s.at_fraction(-1.0).target_secs, 0.0);
        assert_eq!(s.at_fraction(9.0).target_secs, 120.0);
        // no duration means no fraction to resolve, so the drag holds
        // instead of jumping to the start of a stream.
        let live = Scrub {
            target_secs: 10.0,
            duration_secs: None,
        };
        assert_eq!(live.at_fraction(0.5).target_secs, 10.0);
    }

    /// The layout the host hit-tests a click against has to name the
    /// same columns the glyphs were painted on; a guess at them is
    /// exactly what the fit pass makes wrong.
    #[test]
    fn the_exposed_layout_points_at_the_columns_the_row_drew_on() {
        for width_cells in [20u16, 40, 60, 100, 160] {
            let (buf, layout) = bar_and_layout(width_cells, 2, |cb| {
                cb.queue_len = 3;
                cb.loop_mode = LoopMode::One;
                cb.shuffle_on = true;
                cb.speed = 1.25;
                cb.eq_active = true;
                cb.crossfade_on = true;
            });

            for (field, glyph) in [
                (Field::Pause, GLYPH_PLAY),
                (Field::Loop, GLYPH_LOOP),
                (Field::Shuffle, GLYPH_SHUFFLE),
                (Field::Speed, GLYPH_SPEED),
                (Field::Progress, TRACK_HEAD),
            ] {
                let rect = layout.controls.iter().find(|(f, _)| *f == field);
                let Some((_, rect)) = rect else {
                    // dropped at this width, so it must not be drawn
                    // either: that is the invariant worth checking.
                    assert!(
                        !grid(&buf).contains(glyph),
                        "{field:?} has no rect at {width_cells} cells but was drawn"
                    );
                    continue;
                };
                assert_eq!(rect.y, 1, "{field:?} at {width_cells} cells");
                let col = col_of(&buf, 1, glyph);
                assert!(
                    col >= rect.x && col < rect.x + rect.width,
                    "{field:?}: {glyph:?} drawn at {col} but rect is {rect:?} ({width_cells} cells)"
                );
            }

            // the track rect is the track cells and nothing else: its
            // first and last column are both track, and the cell
            // before it is the space after the elapsed readout.
            if let Some(track) = layout.track {
                for x in [track.x, track.x + track.width - 1] {
                    let sym = buf.cell((x, 1)).unwrap().symbol().to_string();
                    assert!(
                        [TRACK_DONE, TRACK_LEFT, TRACK_HEAD]
                            .iter()
                            .any(|c| sym.starts_with(*c)),
                        "column {x} of the track rect holds {sym:?} at {width_cells} cells"
                    );
                }
                assert_eq!(buf.cell((track.x - 1, 1)).unwrap().symbol(), " ");
            }
        }
    }

    /// The seam the host sits on: the layout it records fed to the
    /// resolver that reads it, clicked on the cell each glyph was
    /// actually painted on. Both halves are tested on their own; this
    /// is the only place they meet, and it is what "clicking a control
    /// presses that control" means.
    #[test]
    fn clicking_the_cell_a_control_was_painted_on_presses_that_control() {
        use crate::hit::{act, Act, Click, Control, HitMap, Kind, Transport};

        for width_cells in [20u16, 40, 60, 100, 160] {
            let (buf, layout) = bar_and_layout(width_cells, 2, |cb| {
                cb.queue_len = 3;
                cb.loop_mode = LoopMode::One;
                cb.shuffle_on = true;
                cb.speed = 1.25;
                cb.eq_active = true;
                cb.crossfade_on = true;
            });
            let m = HitMap {
                bottom: Some(Rect::new(0, 0, width_cells, 2)),
                controls: layout
                    .controls
                    .iter()
                    .map(|(field, rect)| Control {
                        transport: Transport::Bar,
                        field: *field,
                        rect: *rect,
                    })
                    .collect(),
                tracks: layout
                    .track
                    .map(|t| (Transport::Bar, t))
                    .into_iter()
                    .collect(),
                ..Default::default()
            };
            let click = |col| {
                act(
                    &m,
                    Click {
                        kind: Kind::Down,
                        col,
                        row: 1,
                    },
                    None,
                )
            };

            let mut pressed = 0;
            for (field, glyph) in [
                (Field::Pause, GLYPH_PLAY.to_string()),
                (Field::Loop, GLYPH_LOOP.to_string()),
                (Field::Shuffle, GLYPH_SHUFFLE.to_string()),
                (Field::Speed, GLYPH_SPEED.to_string()),
                (Field::Eq, GLYPH_EQ.to_string()),
                (Field::Crossfade, GLYPH_CROSSFADE.to_string()),
            ] {
                // dropped at this width, so there is nothing drawn to
                // click; the layout test either side covers that.
                if !layout.controls.iter().any(|(f, _)| *f == field) {
                    continue;
                }
                let ch = glyph.chars().next().expect("a glyph has a first char");
                let col = col_of(&buf, 1, ch);
                assert_eq!(
                    click(col),
                    Act::Press(Transport::Bar, field, None),
                    "{field:?} is drawn at column {col} of a {width_cells}-cell bar"
                );
                pressed += 1;
            }
            assert!(pressed > 0, "a {width_cells}-cell bar drew no control");

            // every cell of the track, both ways: the fraction a click
            // resolves to has to put the head back on the cell that
            // was clicked, or the seek lands off the pointer.
            if let Some(t) = layout.track {
                for x in t.x..t.x + t.width {
                    let frac = match click(x) {
                        Act::Press(Transport::Bar, Field::Progress, Some(frac)) => frac,
                        other => panic!("column {x} of the track gave {other:?}"),
                    };
                    let head = track(frac, t.width as usize, false)
                        .chars()
                        .position(|c| c == TRACK_HEAD)
                        .expect("the track carries a head");
                    assert_eq!(
                        head as u16,
                        x - t.x,
                        "track cell {} came back as {frac}",
                        x - t.x
                    );
                }
            }
        }
    }

    /// A one-row bar puts the controls on the only row there is, and a
    /// resize hint pushes them down; the layout has to follow.
    /// A one-row bar puts the controls on the only row there is, and a
    /// resize hint pushes them down; the layout has to follow, or a
    /// click in resize mode lands a row above every control.
    #[test]
    fn the_exposed_layout_follows_the_control_row_up_and_down() {
        for (height, hint) in [
            (1u16, None),
            (2, None),
            (4, None),
            (2, Some("resize mode")),
            (3, Some("resize mode")),
        ] {
            let (buf, layout) = bar_and_layout(100, height, |cb| cb.resize_hint = hint);
            let drawn_row = grid(&buf)
                .lines()
                .position(|line| line.contains(GLYPH_PLAY))
                .unwrap_or_else(|| panic!("the play glyph is on no row at height {height}"))
                as u16;
            let (_, rect) = layout
                .controls
                .iter()
                .find(|(f, _)| *f == Field::Pause)
                .unwrap_or_else(|| panic!("no pause rect at height {height}"));
            assert_eq!(rect.y, drawn_row, "height {height}, hint {hint:?}");
        }
    }

    #[test]
    fn a_scrub_step_on_an_unknown_duration_only_clamps_at_zero() {
        let s = Scrub {
            target_secs: 10.0,
            duration_secs: None,
        };
        assert_eq!(s.step(SCRUB_STEP_COARSE).target_secs, 40.0);
        assert_eq!(s.step(-100.0).target_secs, 0.0);
        // a zero/unknown duration is the same case: no end to clamp to.
        let zero = Scrub {
            target_secs: 0.0,
            duration_secs: Some(0.0),
        };
        assert_eq!(zero.step(SCRUB_STEP).target_secs, SCRUB_STEP);
    }

    #[test]
    fn the_track_marks_the_head_once_at_either_extreme() {
        let start = track(0.0, 10, false);
        assert_eq!(start.chars().next(), Some(TRACK_HEAD));
        assert_eq!(start.matches(TRACK_HEAD).count(), 1);
        let end = track(1.0, 10, false);
        assert_eq!(end.chars().last(), Some(TRACK_HEAD));
        assert_eq!(end.matches(TRACK_HEAD).count(), 1);
        // the track is exactly the cells it was asked for, which is
        // what keeps the bar flush with the right edge.
        for cells in 0..20 {
            assert_eq!(width(&track(0.5, cells, false)), cells.max(TRACK_MIN));
        }
    }

    /// A drag resolves against the rect the *previous* frame recorded.
    /// The elapsed readout used to grow from four cells to five at
    /// 9:59 -> 10:00 and push the track one column right, so for that
    /// one frame every cell of the track answered with its
    /// neighbour's fraction. The readout reserves the total's width up
    /// front now, so there is nothing left to shift.
    #[test]
    fn the_track_does_not_move_when_the_elapsed_readout_crosses_ten_minutes() {
        // 43 cells is the narrowest bar that keeps a 30 minute song's
        // scrubber at all; narrower than that the element is dropped,
        // which the reserved width also makes a decision for the whole
        // song rather than one the clock changes at 10:00.
        for width_cells in [43u16, 60, 100, 160] {
            let mut rects = Vec::new();
            for secs in [0.0, 598.0, 599.0, 599.9, 600.0, 601.0, 1799.0] {
                let (buf, layout) = bar_and_layout(width_cells, 2, |cb| {
                    cb.elapsed_secs = Some(secs);
                    cb.duration_secs = Some(1800.0);
                });
                let track = layout.track.expect("a 30 minute song has a scrubber");
                // and the rect is track cells, not a readout that grew
                // into them.
                for x in [track.x, track.x + track.width - 1] {
                    let sym = buf.cell((x, 1)).unwrap().symbol().to_string();
                    assert!(
                        [TRACK_DONE, TRACK_LEFT, TRACK_HEAD]
                            .iter()
                            .any(|c| sym.starts_with(*c)),
                        "at {secs}s column {x} of the track rect holds {sym:?}"
                    );
                }
                rects.push((secs, track));
            }
            let (_, first) = rects[0];
            for (secs, rect) in &rects {
                assert_eq!(
                    *rect, first,
                    "the track moved to {rect:?} at {secs}s on a {width_cells}-cell bar"
                );
            }
        }
    }

    #[test]
    fn nothing_playing_leaves_the_scrubber_dim_with_no_fake_position() {
        let buf = bar_at(100, 2, |cb| {
            cb.elapsed_secs = None;
            cb.duration_secs = None;
        });
        let line = grid(&buf).lines().nth(1).unwrap().to_string();
        assert!(line.contains("--:--"), "{line}");
        assert_eq!(fg_of(&buf, 1, TRACK_HEAD), shade(DIM));
    }

    #[test]
    fn the_resize_hint_takes_the_top_row_and_pushes_everything_down() {
        let buf = bar_at(100, 3, |cb| cb.resize_hint = Some("resize mode: a=sidebar"));
        let rows: Vec<String> = grid(&buf).lines().map(str::to_string).collect();
        assert!(rows[0].contains("resize mode"), "{:?}", rows[0]);
        assert!(rows[1].contains("-- INTERACT"), "{:?}", rows[1]);
        assert!(rows[2].contains(GLYPH_QUEUE), "{:?}", rows[2]);
    }

    /// The invariant the padding broke: every cell the selection plate
    /// lights is a cell `layout` reports as that control's, and every
    /// cell it reports is lit. Clicking what looks highlighted has to
    /// press it, which was the bug.
    #[test]
    fn the_selection_plate_covers_exactly_the_clickable_cells() {
        for width in [20u16, 40, 60, 100, 160] {
            let (buf, layout) = bar_and_layout(width, 1, |cb| {
                cb.focused = true;
                cb.selected = Some(Field::Loop);
            });
            let Some((_, rect)) = layout
                .controls
                .iter()
                .find(|(field, _)| *field == Field::Loop)
            else {
                continue;
            };

            // the plate is the only thing on this row with a non-default
            // background, so "lit" is just "bg differs from its row".
            let default_bg = buf.cell((width - 1, 0)).map(|c| c.bg);
            let lit: Vec<u16> = (0..width)
                .filter(|x| buf.cell((*x, 0)).map(|c| c.bg) != default_bg)
                .collect();
            let claimed: Vec<u16> = (rect.x..rect.x + rect.width).collect();
            assert_eq!(
                lit,
                claimed,
                "at {width} cells the lit plate and the clickable rect disagree:\n{}",
                grid(&buf)
            );
        }
    }
}

#[cfg(test)]
mod eyeball {
    use super::*;
    use crate::ui::testbuf::{buffer, grid};

    fn bar() -> ControlsBar<'static> {
        ControlsBar {
            move_mode: false,
            focus_label: "bottom bar",
            focused: false,
            queue_len: 3,
            loop_mode: LoopMode::Off,
            shuffle_on: false,
            speed: 1.0,
            eq_active: false,
            crossfade_on: false,
            paused: false,
            gap_remaining: None,
            elapsed_secs: Some(42.0),
            duration_secs: Some(214.0),
            flash: None,
            selected: None,
            scrubbing: false,
            resize_hint: None,
        }
    }

    /// The mode line at the widths a real terminal gives it, which is
    /// what decides whether the console hint is there at all:
    /// `cargo test --lib ui::controlsbar::eyeball -- --nocapture`.
    /// Asserts the floor too, so it is a test and not just a printer.
    #[test]
    fn print_the_mode_line_across_the_widths_the_console_hint_turns_on_at() {
        for (w, focused, move_mode) in [
            (120u16, false, false),
            (100, false, false),
            (100, true, false),
            (80, true, false),
            (72, true, false),
            (64, false, false),
            (56, false, false),
            (100, false, true),
        ] {
            let mut cb = bar();
            cb.focused = focused;
            cb.move_mode = move_mode;
            let mut buf = buffer(w, 2);
            cb.render(Rect::new(0, 0, w, 2), &mut buf);
            let text = grid(&buf);
            println!("--- {w}x2 focused={focused} move={move_mode}\n{text}");
            assert!(
                text.contains("--"),
                "{w} cells lost the mode label:\n{text}"
            );
        }
    }
}
