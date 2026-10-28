//! The eq menu: the built-in presets, the user's own saved curves and
//! the ten bands, as one scrolling list under three section headings,
//! with a vertical-bar plot of the live curve at the head of the band
//! group. Opened with enter on the bottom bar's Eq field.
//!
//! Three groups and a name editor is more than a flat list can carry
//! legibly, so the groups get the same full-width centred rule the
//! results list and the queue use, and the plot carries the shape of
//! the curve that ten `+3.0 dB` strings cannot.

use std::time::Duration;

use crossterm::event::KeyCode;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::anim;
use crate::eq::{self, CustomPresets, Source, BAND_HZ, GAIN_LIMIT, MAX_NAME_LEN, PRESETS};
use crate::keymap::key_label;
use crate::theme::{lerp, shade, BRIGHT, DIM, MID, NEAR_WHITE, WHITE};
use crate::ui::draw;
use crate::ui::header::draw_header;
use crate::ui::results_list::{draw_header_row, fill_row_bg};
use crate::ui::scroll;
use crate::ui::textfit::{tail_fit, truncate_with_ellipsis, width};

/// The menu's own keys, as the constants both the hint line and
/// main.rs's handler read: a hint spelling its own letter could
/// describe a key the handler never acts on, which is exactly how
/// stale hints happened before. Modal-local, like the profile
/// manager's `d d`, so deliberately not `KeyAction`s (keymap.rs owns
/// only the global remappable keys; see its module doc).
pub const SAVE_KEY: KeyCode = KeyCode::Char('s');
pub const RENAME_KEY: KeyCode = KeyCode::Char('r');
pub const DELETE_KEY: KeyCode = KeyCode::Char('d');

pub const PRESET_COUNT: usize = PRESETS.len();
pub const BAND_COUNT: usize = BAND_HZ.len();

/// Rows j/k can actually land on: the built-ins, then the saved
/// curves, then the ten bands. Headings and the plot are drawn rows
/// but not selectable ones, same split the queue view makes.
pub fn selectable_count(saved: usize) -> usize {
    PRESET_COUNT + saved + BAND_COUNT
}

/// What the cursor is on, resolved from the flat selection index the
/// caller keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cursor {
    Builtin(usize),
    Saved(usize),
    Band(usize),
}

/// Clamped, never wrapped: wrapping off the last band back onto a
/// preset row is what used to destroy ten hand-tuned gains.
pub fn cursor(selected: usize, saved: usize) -> Cursor {
    let selected = selected.min(selectable_count(saved) - 1);
    if selected < PRESET_COUNT {
        Cursor::Builtin(selected)
    } else if selected < PRESET_COUNT + saved {
        Cursor::Saved(selected - PRESET_COUNT)
    } else {
        Cursor::Band(selected - PRESET_COUNT - saved)
    }
}

pub fn first_band_row(saved: usize) -> usize {
    PRESET_COUNT + saved
}

/// Where the cursor sits when the menu opens: on the row that is
/// actually applied, so the first thing shown is where you already
/// are. A nameless curve has no row to claim it, so the bands it was
/// tuned with get the cursor instead.
pub fn opening_selection(source: Option<Source>, saved: usize) -> usize {
    match source {
        Some(Source::Builtin(i)) if i < PRESET_COUNT => i,
        Some(Source::Saved(i)) if i < saved => PRESET_COUNT + i,
        _ => first_band_row(saved),
    }
}

/// Two-press confirm for one destructive action, armed on the exact
/// target it will act on and disarmed by any other key; the profile
/// manager's `d d` in reusable form. `press` returning true is the
/// only thing allowed to destroy anything, so the gate cannot be
/// bypassed by a caller that forgot to check a separate flag.
#[derive(Debug, Clone, PartialEq)]
pub struct Confirm<T> {
    armed: Option<T>,
}

impl<T> Default for Confirm<T> {
    fn default() -> Self {
        Confirm { armed: None }
    }
}

impl<T: PartialEq> Confirm<T> {
    /// True only on a second press on the same target, which also
    /// disarms: a third press arms again rather than firing twice.
    pub fn press(&mut self, target: T) -> bool {
        if self.armed.as_ref() == Some(&target) {
            self.armed = None;
            return true;
        }
        self.armed = Some(target);
        false
    }

    pub fn disarm(&mut self) {
        self.armed = None;
    }

    pub fn armed(&self) -> Option<&T> {
        self.armed.as_ref()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorMode {
    /// Save the live curve under a new name.
    Save,
    /// Rename the saved preset at this index.
    Rename(usize),
}

/// The name input, reusing the login form's approach (tail-fit the
/// value so what you are typing stays visible, block cursor at the
/// writer's returned x) rather than a third hand-rolled text field.
#[derive(Debug, Clone)]
pub struct NameEditor {
    pub mode: EditorMode,
    pub text: String,
    /// Armed overwrite of the saved preset at this index; a second
    /// enter commits, same gate a delete goes through.
    pub overwrite: Confirm<usize>,
}

impl NameEditor {
    pub fn save() -> Self {
        NameEditor {
            mode: EditorMode::Save,
            text: String::new(),
            overwrite: Confirm::default(),
        }
    }

    pub fn rename(index: usize, current: &str) -> Self {
        NameEditor {
            mode: EditorMode::Rename(index),
            text: current.to_string(),
            overwrite: Confirm::default(),
        }
    }

    /// Which saved preset this edit is allowed to collide with
    /// (itself, when renaming).
    pub fn rename_target(&self) -> Option<usize> {
        match self.mode {
            EditorMode::Rename(i) => Some(i),
            EditorMode::Save => None,
        }
    }

    /// Control characters would be invisible in the menu and in
    /// config.toml alike; the cap is the store's own (see
    /// `eq::MAX_NAME_LEN`), enforced here so typing stops at the
    /// limit instead of being rejected on enter.
    pub fn push(&mut self, ch: char) {
        if ch.is_control() || self.text.chars().count() >= MAX_NAME_LEN {
            return;
        }
        self.text.push(ch);
        self.overwrite.disarm();
    }

    pub fn pop(&mut self) {
        self.text.pop();
        self.overwrite.disarm();
    }

    fn prompt(&self) -> &'static str {
        match self.mode {
            EditorMode::Save => "save as: ",
            EditorMode::Rename(_) => "rename to: ",
        }
    }
}

/// Everything the menu owns beyond the selection index (which stays a
/// plain `usize` in main.rs, since the shellsong `view(eq)` path
/// writes it too).
#[derive(Debug, Clone, Default)]
pub struct EqMenuState {
    pub editor: Option<NameEditor>,
    pub delete: Confirm<usize>,
    /// One line of feedback: a confirm prompt, a refused name, or
    /// what the last save/rename/delete did. Empty when there is
    /// nothing to say.
    pub message: String,
}

impl EqMenuState {
    /// Called on every keypress that is not the delete key, so moving
    /// off the armed row (or doing anything else at all) cancels a
    /// pending delete rather than leaving it live.
    pub fn disarm_delete(&mut self) {
        self.delete.disarm();
    }
}

/// Rows above and below the zero line. Seven rows at 4 dB each covers
/// the full ±12 dB without pushing the preset lists off a normal
/// terminal, and the half-block glyphs halve that again so a single
/// 1 dB nudge still moves something.
const PLOT_HALF: usize = 3;
const PLOT_ROWS: usize = PLOT_HALF * 2 + 1;
/// `"  +12 ┤"`: the axis gutter left of the first bar.
const AXIS_WIDTH: usize = 7;
/// Three cells of bar and one of gap. A one-cell bar reads as a
/// dotted line rather than a curve.
const BAR_WIDTH: usize = 3;
const SLOT: usize = BAR_WIDTH + 1;

/// One drawn line of the menu body. Flat, including the plot's own
/// rows, so the whole body scrolls through one offset and no group
/// can be half-clipped by its own private layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    Heading(&'static str),
    /// The saved group's heading carries the management keys, since
    /// they are the group's own: a legend over the rows it acts on
    /// beats a header line long enough to be truncated.
    SavedHeading,
    Builtin(usize),
    Saved(usize),
    /// Stands in for an empty saved group: a heading over a gap says
    /// nothing about how to fill it.
    NothingSaved,
    /// One line of the curve plot, 0 at the top.
    Plot(usize),
    /// The frequency labels under the plot.
    PlotAxis,
    Band(usize),
}

fn rows(saved: usize) -> Vec<Row> {
    let mut out = Vec::with_capacity(PRESET_COUNT + saved + BAND_COUNT + PLOT_ROWS + 5);
    out.push(Row::Heading("built in"));
    out.extend((0..PRESET_COUNT).map(Row::Builtin));
    out.push(Row::SavedHeading);
    if saved == 0 {
        out.push(Row::NothingSaved);
    } else {
        out.extend((0..saved).map(Row::Saved));
    }
    out.push(Row::Heading("bands"));
    out.extend((0..PLOT_ROWS).map(Row::Plot));
    out.push(Row::PlotAxis);
    out.extend((0..BAND_COUNT).map(Row::Band));
    out
}

fn row_of(rows: &[Row], cursor: Cursor) -> usize {
    let wanted = match cursor {
        Cursor::Builtin(i) => Row::Builtin(i),
        Cursor::Saved(i) => Row::Saved(i),
        Cursor::Band(i) => Row::Band(i),
    };
    rows.iter().position(|r| *r == wanted).unwrap_or(0)
}

/// Half-cells of bar band `gain` earns. A nonzero gain never rounds
/// to nothing: the smallest nudge the menu can make has to show.
fn bar_halves(gain: f64) -> usize {
    if gain == 0.0 {
        return 0;
    }
    let full = (PLOT_HALF * 2) as f64;
    let halves = (gain.abs() / GAIN_LIMIT * full).round() as usize;
    halves.clamp(1, PLOT_HALF * 2)
}

/// Glyph for one plot row of one bar: a full block for a covered row,
/// a half block pointing the way the bar grows for a half-covered
/// one, nothing past the bar's top.
fn bar_glyph(halves: usize, row_from_zero: usize, up: bool) -> Option<char> {
    match halves.saturating_sub(row_from_zero * 2) {
        0 => None,
        1 => Some(if up { '▄' } else { '▀' }),
        _ => Some('█'),
    }
}

/// Monochrome has height and brightness and nothing else, so both
/// carry the gain: brightness tracks magnitude, and the band being
/// edited steps to a clearly brighter band of levels so it is
/// findable in the plot without a second colour.
fn bar_level(gain: f64, selected: bool) -> f32 {
    let magnitude = (gain.abs() / GAIN_LIMIT) as f32;
    if selected {
        lerp(BRIGHT, WHITE, magnitude)
    } else {
        lerp(DIM, MID, magnitude)
    }
}

/// "31", "500", "1k", "16k": the plot's x axis has three cells per
/// band, which the full figure only fits under 1 kHz.
fn axis_label(hz: f64) -> String {
    if hz >= 1000.0 {
        format!("{}k", (hz / 1000.0).round())
    } else {
        format!("{hz:.0}")
    }
}

pub struct EqMenu<'a> {
    /// The live, *requested* gains (never the compensated ones; see
    /// `eq::compensated_gains`).
    pub gains: &'a [f64; 10],
    pub saved: &'a CustomPresets,
    /// Which named curve the live gains came from, if any.
    pub source: Option<Source>,
    /// The live gains no longer match `source`, i.e. a band has been
    /// nudged since: the third state, neither that preset nor an
    /// anonymous custom curve, which the menu used to hide entirely.
    pub modified: bool,
    pub selected: usize,
    pub state: &'a EqMenuState,
    pub elapsed: Duration,
}

impl EqMenu<'_> {
    /// Tag on a preset row: the applied one says so, and says when it
    /// has been nudged off its saved values, so no row is ever
    /// silently crediting a curve that is not what you hear. An armed
    /// delete outranks both; what the next keypress destroys belongs
    /// on the row it will destroy, not only in the message line.
    fn tag(&self, row: Cursor) -> String {
        if let Cursor::Saved(i) = row {
            if self.state.delete.armed() == Some(&i) {
                return format!("(press {} again)", key_label(DELETE_KEY));
            }
        }
        let claimed = match (self.source, row) {
            (Some(Source::Builtin(i)), Cursor::Builtin(j)) => i == j,
            (Some(Source::Saved(i)), Cursor::Saved(j)) => i == j,
            _ => false,
        };
        match (claimed, self.modified) {
            (true, false) => "(applied)".to_string(),
            (true, true) => "(modified)".to_string(),
            _ => String::new(),
        }
    }

    fn draw_preset_row(&self, buf: &mut Buffer, area: Rect, y: u16, row: Cursor, name: &str) {
        let selected = cursor(self.selected, self.saved.len()) == row;
        if selected {
            // the selection reads off the row's background, never a
            // border; the brightness bump below sits on top of it.
            fill_row_bg(buf, area, y, anim::selection_bg(self.elapsed, true));
        }
        let tag = self.tag(row);
        let level = if selected {
            BRIGHT
        } else if tag.is_empty() {
            DIM
        } else {
            MID
        };
        let marker = if selected { "> " } else { "  " };
        let tag_room = if tag.is_empty() { 0 } else { width(&tag) + 2 };
        let budget = (area.width as usize).saturating_sub(tag_room);
        draw::at(
            buf,
            area.x,
            y,
            &truncate_with_ellipsis(&format!("{marker}{name}"), budget),
            shade(level),
            budget,
        );
        if !tag.is_empty() && tag_room < area.width as usize {
            let x = area.x + area.width - width(&tag) as u16 - 1;
            draw::at(
                buf,
                x,
                y,
                &tag,
                shade(if selected { NEAR_WHITE } else { MID }),
                width(&tag),
            );
        }
    }

    fn draw_plot_row(&self, buf: &mut Buffer, area: Rect, y: u16, row: usize) {
        let zero = row == PLOT_HALF;
        let (label, tick) = match row {
            0 => (format!("+{GAIN_LIMIT:.0}"), '┤'),
            r if r == PLOT_HALF => ("0".to_string(), '┼'),
            r if r == PLOT_ROWS - 1 => (format!("-{GAIN_LIMIT:.0}"), '┤'),
            _ => (String::new(), '│'),
        };
        draw::at(
            buf,
            area.x,
            y,
            &format!("{label:>5} {tick}"),
            shade(DIM),
            area.width as usize,
        );

        let selected_band = match cursor(self.selected, self.saved.len()) {
            Cursor::Band(b) => Some(b),
            _ => None,
        };
        let right = area.x + area.width;
        for (band, gain) in self.gains.iter().enumerate() {
            let left = area.x + (AXIS_WIDTH + band * SLOT) as u16;
            let is_selected = selected_band == Some(band);
            // the zero line runs under every band, gap included, so
            // it reads as an axis rather than ten stubs.
            let glyph = if zero {
                Some('─')
            } else if (*gain > 0.0 && row < PLOT_HALF) || (*gain < 0.0 && row > PLOT_HALF) {
                let up = *gain > 0.0;
                let from_zero = if up {
                    PLOT_HALF - 1 - row
                } else {
                    row - PLOT_HALF - 1
                };
                bar_glyph(bar_halves(*gain), from_zero, up)
            } else {
                None
            };
            let Some(glyph) = glyph else { continue };
            let level = if zero && *gain == 0.0 && !is_selected {
                DIM
            } else {
                bar_level(*gain, is_selected)
            };
            let span = if zero { SLOT } else { BAR_WIDTH };
            for col in 0..span as u16 {
                if left + col >= right {
                    break;
                }
                draw::put(buf, left + col, y, glyph, shade(level));
            }
        }
    }

    fn draw_plot_axis(&self, buf: &mut Buffer, area: Rect, y: u16) {
        let selected_band = match cursor(self.selected, self.saved.len()) {
            Cursor::Band(b) => Some(b),
            _ => None,
        };
        let right = area.x + area.width;
        for (band, hz) in BAND_HZ.iter().enumerate() {
            let x = area.x + (AXIS_WIDTH + band * SLOT) as u16;
            if x >= right {
                break;
            }
            let selected = selected_band == Some(band);
            // the label is also how a selected band with no gain at
            // all stays visible: it has no bar to brighten.
            draw::at(
                buf,
                x,
                y,
                &format!("{:>BAR_WIDTH$}", axis_label(*hz)),
                shade(if selected { NEAR_WHITE } else { DIM }),
                (right - x) as usize,
            );
        }
    }

    fn draw_band_row(&self, buf: &mut Buffer, area: Rect, y: u16, band: usize) {
        let selected = cursor(self.selected, self.saved.len()) == Cursor::Band(band);
        if selected {
            fill_row_bg(buf, area, y, anim::selection_bg(self.elapsed, true));
        }
        let marker = if selected { "> " } else { "  " };
        let label = format!("{:.0} Hz", BAND_HZ[band]);
        // the gain right-aligned in its own column, so the ten
        // numbers line up on the decimal point instead of stepping
        // in and out with the sign and the digit count.
        let gain = format!("{:+.1} dB", self.gains[band]);
        let text = format!("{marker}{label:>8}  {gain:>9}");
        draw::at(
            buf,
            area.x,
            y,
            &truncate_with_ellipsis(&text, area.width as usize),
            shade(if selected { BRIGHT } else { DIM }),
            area.width as usize,
        );
    }

    /// The name input plus, on the right of the same row, whatever
    /// the menu has to say about it; an overwrite confirm belongs
    /// next to the name it is about to replace, not three rows away.
    fn draw_editor_row(&self, buf: &mut Buffer, area: Rect, y: u16, editor: &NameEditor) {
        let prompt = editor.prompt();
        let message_room = if self.state.message.is_empty() {
            0
        } else {
            width(&self.state.message) + 2
        };
        let budget = (area.width as usize)
            .saturating_sub(width(prompt) + 1)
            .saturating_sub(message_room);
        let after_prompt = draw::at(buf, area.x, y, prompt, shade(DIM), area.width as usize);
        let text_end = draw::at(
            buf,
            after_prompt,
            y,
            &tail_fit(&editor.text, budget),
            shade(NEAR_WHITE),
            budget,
        );
        // same cursor as the search boxes: a block at the writer's
        // returned x, dimmed rather than hidden on the off phase so a
        // single frame still answers "is this taking my keys".
        let level = if anim::blink(self.elapsed, anim::CURSOR_BLINK) {
            NEAR_WHITE
        } else {
            DIM
        };
        draw::put(buf, text_end, y, '█', shade(level));
        if message_room > 0 && message_room < area.width as usize {
            let x = area.x + area.width - width(&self.state.message) as u16 - 1;
            if x > text_end {
                draw::at(
                    buf,
                    x,
                    y,
                    &self.state.message,
                    shade(MID),
                    width(&self.state.message),
                );
            }
        }
    }
}

impl Widget for EqMenu<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let keys = |key| key_label(key);
        let instructions = match self.state.editor {
            Some(_) => "enter: save the name, esc: cancel",
            None => "enter apply · h/l nudge · esc close",
        };
        let mut y = draw_header(
            buf,
            area,
            area.y,
            "eq",
            instructions,
            shade(DIM),
            shade(crate::theme::NEAR_BLACK),
        );

        // which of the three applied states is live, spelled out
        // rather than left to be inferred from which row is lit.
        let label = eq::applied_label(self.source, self.modified, self.saved);
        let after = draw::at(buf, area.x, y, "applied: ", shade(DIM), area.width as usize);
        let end = draw::at(
            buf,
            after,
            y,
            &label,
            shade(NEAR_WHITE),
            (area.x + area.width).saturating_sub(after) as usize,
        );
        if self.source.is_none() {
            draw::at(
                buf,
                end,
                y,
                &format!(" (unsaved; {} names it)", keys(SAVE_KEY)),
                shade(DIM),
                (area.x + area.width).saturating_sub(end) as usize,
            );
        }
        y += 1;

        match &self.state.editor {
            Some(editor) => self.draw_editor_row(buf, area, y, editor),
            None => draw::line(
                buf,
                area,
                y,
                &truncate_with_ellipsis(&self.state.message, area.width as usize),
                shade(MID),
            ),
        }
        y += 2;

        let body_height = (area.y + area.height).saturating_sub(y) as usize;
        if body_height == 0 {
            return;
        }
        let rows = rows(self.saved.len());
        let offset = scroll::offset_for(
            row_of(&rows, cursor(self.selected, self.saved.len())),
            body_height,
        );
        for (i, row) in rows.iter().skip(offset).take(body_height).enumerate() {
            let y = y + i as u16;
            match row {
                Row::Heading(label) => draw_header_row(buf, area, y, label),
                // the legend goes away while the editor owns the
                // keyboard: those letters type a name right now, so
                // advertising them would be a hint that lies.
                Row::SavedHeading if self.state.editor.is_some() => {
                    draw_header_row(buf, area, y, "saved")
                }
                Row::SavedHeading => draw_header_row(
                    buf,
                    area,
                    y,
                    &format!(
                        "saved · {} save · {} rename · {k} {k} delete",
                        keys(SAVE_KEY),
                        keys(RENAME_KEY),
                        k = keys(DELETE_KEY),
                    ),
                ),
                Row::Builtin(i) => {
                    self.draw_preset_row(buf, area, y, Cursor::Builtin(*i), PRESETS[*i].name)
                }
                Row::Saved(i) => {
                    let name = self.saved.name_at(*i).unwrap_or("");
                    self.draw_preset_row(buf, area, y, Cursor::Saved(*i), name)
                }
                Row::NothingSaved => draw::line(
                    buf,
                    area,
                    y,
                    &truncate_with_ellipsis(
                        &format!(
                            "  nothing saved yet; {} saves the live curve under a name",
                            keys(SAVE_KEY)
                        ),
                        area.width as usize,
                    ),
                    shade(DIM),
                ),
                Row::Plot(r) => self.draw_plot_row(buf, area, y, *r),
                Row::PlotAxis => self.draw_plot_axis(buf, area, y),
                Row::Band(b) => self.draw_band_row(buf, area, y, *b),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::testbuf::{buffer, grid};
    use ratatui::style::Color;

    const TUNED: [f64; 10] = [12.0, 6.0, 2.0, 0.0, -2.0, -6.0, -12.0, 0.0, 0.0, 1.0];
    const W: u16 = 72;
    const H: u16 = 44;

    fn store(names: &[&str]) -> CustomPresets {
        let mut saved = CustomPresets::default();
        for name in names {
            saved.insert(name, TUNED);
        }
        saved
    }

    struct Case {
        gains: [f64; 10],
        saved: CustomPresets,
        source: Option<Source>,
        modified: bool,
        selected: usize,
        state: EqMenuState,
    }

    impl Default for Case {
        fn default() -> Self {
            Case {
                gains: [0.0; 10],
                saved: CustomPresets::default(),
                source: Some(Source::Builtin(0)),
                modified: false,
                selected: 0,
                state: EqMenuState::default(),
            }
        }
    }

    impl Case {
        fn buf(&self, w: u16, h: u16) -> Buffer {
            let area = Rect::new(0, 0, w, h);
            let mut buf = buffer(w, h);
            EqMenu {
                gains: &self.gains,
                saved: &self.saved,
                source: self.source,
                modified: self.modified,
                selected: self.selected,
                state: &self.state,
                elapsed: Duration::ZERO,
            }
            .render(area, &mut buf);
            buf
        }

        fn grid(&self) -> String {
            grid(&self.buf(W, H))
        }
    }

    fn plated_rows(buf: &Buffer) -> Vec<u16> {
        (0..buf.area.height)
            .filter(|y| {
                (0..buf.area.width).all(|x| buf.cell((x, *y)).is_some_and(|c| c.bg != Color::Reset))
            })
            .collect()
    }

    /// The list row for `name`, not the "applied: ..." status line
    /// that also mentions it.
    fn preset_row<'a>(out: &'a str, name: &str) -> &'a str {
        out.lines()
            .find(|l| l.contains(name) && !l.starts_with("applied:"))
            .unwrap_or_else(|| panic!("no list row for {name}:\n{out}"))
    }

    fn row_text(buf: &Buffer, y: u16) -> String {
        (0..buf.area.width)
            .map(|x| crate::ui::testbuf::char_at(buf, x, y))
            .collect()
    }

    #[test]
    fn the_three_groups_each_get_a_heading() {
        let out = Case {
            saved: store(&["night"]),
            ..Case::default()
        }
        .grid();
        for heading in ["built in", "saved", "bands"] {
            assert!(out.contains(heading), "missing {heading} heading:\n{out}");
        }
        // the headings are rules, not bare text rows, so the groups
        // read as groups (see results_list::draw_header_row).
        assert!(out.contains("───"), "headings must be ruled:\n{out}");
        assert!(out.contains("night"), "the saved group lists it:\n{out}");
        assert!(out.contains("bass boost"), "{out}");
        assert!(out.contains("16000 Hz"), "{out}");
    }

    #[test]
    fn an_empty_saved_group_says_how_to_fill_it() {
        let out = Case::default().grid();
        assert!(out.contains("nothing saved yet"), "{out}");
        assert!(out.contains("s saves the live curve"), "{out}");
    }

    /// The plot is the point of the group: a boost has to put blocks
    /// above the zero line and a cut below it, in that band's own
    /// column, at a height that follows the gain.
    #[test]
    fn the_plot_draws_a_bar_per_band_scaled_to_its_gain() {
        let mut gains = [0.0; 10];
        gains[0] = GAIN_LIMIT;
        gains[1] = GAIN_LIMIT / 2.0;
        gains[2] = -GAIN_LIMIT;
        let buf = Case {
            gains,
            ..Case::default()
        }
        .buf(W, H);

        let zero_row = (0..H)
            .find(|y| row_text(&buf, *y).starts_with("    0 ┼"))
            .expect("the plot's zero line");
        let col = |band: usize| (AXIS_WIDTH + band * SLOT) as u16;

        // a full boost reaches the top row, a half boost does not.
        let top = zero_row - PLOT_HALF as u16;
        assert_eq!(crate::ui::testbuf::char_at(&buf, col(0), top), '█');
        assert_eq!(
            crate::ui::testbuf::char_at(&buf, col(1), top),
            '.',
            "half the gain must not reach the top:\n{}",
            grid(&buf)
        );
        assert_eq!(
            crate::ui::testbuf::char_at(&buf, col(1), zero_row - 1),
            '█',
            "but it fills the row above zero"
        );
        // a full cut hangs the other way, and only the other way.
        assert_eq!(
            crate::ui::testbuf::char_at(&buf, col(2), zero_row + PLOT_HALF as u16),
            '█'
        );
        assert_eq!(crate::ui::testbuf::char_at(&buf, col(2), top), '.');
        // an untouched band is just the axis.
        assert_eq!(crate::ui::testbuf::char_at(&buf, col(3), top), '.');
        assert_eq!(crate::ui::testbuf::char_at(&buf, col(3), zero_row), '─');
    }

    /// A 1 dB nudge is a sixth of one plot row; it still has to show,
    /// or the plot would look flat while the audio is not.
    #[test]
    fn the_smallest_nudge_still_draws_something() {
        assert_eq!(bar_halves(0.0), 0);
        assert_eq!(bar_halves(1.0), 1);
        assert_eq!(bar_halves(-1.0), 1);
        assert_eq!(bar_halves(GAIN_LIMIT), PLOT_HALF * 2);
        assert_eq!(bar_glyph(1, 0, true), Some('▄'));
        assert_eq!(bar_glyph(1, 0, false), Some('▀'));
        assert_eq!(bar_glyph(6, 2, true), Some('█'));
        assert_eq!(bar_glyph(4, 2, true), None);
    }

    #[test]
    fn the_bars_brightness_follows_the_gain_and_the_selection() {
        let quiet = bar_level(1.0, false);
        let loud = bar_level(GAIN_LIMIT, false);
        assert!(quiet < loud, "{quiet} vs {loud}");
        assert!(
            bar_level(1.0, true) > loud,
            "the edited band must outrank any unselected one"
        );
    }

    #[test]
    fn an_applied_builtin_is_tagged_on_its_own_row() {
        let out = Case {
            source: Some(Source::Builtin(1)),
            selected: 9,
            ..Case::default()
        }
        .grid();
        let row = preset_row(&out, "bass boost");
        assert!(row.contains("(applied)"), "{row}");
        assert_eq!(out.matches("(applied)").count(), 1, "one row only:\n{out}");
        assert!(out.contains("applied: bass boost"), "{out}");
    }

    #[test]
    fn an_applied_saved_preset_is_tagged_on_its_own_row() {
        let out = Case {
            saved: store(&["night"]),
            source: Some(Source::Saved(0)),
            gains: TUNED,
            selected: 20,
            ..Case::default()
        }
        .grid();
        let row = preset_row(&out, "night");
        assert!(row.contains("(applied)"), "{row}");
        assert!(out.contains("applied: night"), "{out}");
    }

    /// The third state: a saved curve nudged since. It is neither
    /// that preset nor an anonymous custom one, and both the row and
    /// the status line have to say so.
    #[test]
    fn a_nudged_saved_preset_reads_as_modified_not_applied() {
        let out = Case {
            saved: store(&["night"]),
            source: Some(Source::Saved(0)),
            modified: true,
            gains: TUNED,
            selected: 20,
            ..Case::default()
        }
        .grid();
        let row = preset_row(&out, "night");
        assert!(row.contains("(modified)"), "{row}");
        assert!(
            !out.contains("(applied)"),
            "nothing is applied as-is:\n{out}"
        );
        assert!(out.contains("applied: night (modified)"), "{out}");
    }

    #[test]
    fn a_nameless_curve_claims_no_row_and_offers_to_be_saved() {
        let out = Case {
            source: None,
            gains: TUNED,
            selected: 20,
            ..Case::default()
        }
        .grid();
        assert!(out.contains("applied: custom"), "{out}");
        assert!(out.contains("(unsaved; s names it)"), "{out}");
        assert!(!out.contains("(applied)"), "no row may claim it:\n{out}");
        assert!(!out.contains("(modified)"), "{out}");
    }

    #[test]
    fn the_plate_lands_on_exactly_the_selected_row() {
        let saved = store(&["night", "car"]);
        // a built-in, a saved one, and a band: one plate each, on the
        // row holding that exact label.
        for (selected, expected) in [
            (4usize, "rock"),
            (PRESET_COUNT + 1, "car"),
            (first_band_row(2) + 2, "125 Hz"),
        ] {
            let case = Case {
                saved: saved.clone(),
                selected,
                gains: TUNED,
                ..Case::default()
            };
            let buf = case.buf(W, H);
            let plated = plated_rows(&buf);
            assert_eq!(plated.len(), 1, "one plate for {expected}: {plated:?}");
            let row = row_text(&buf, plated[0]);
            assert!(row.contains(expected), "plate on the wrong row: {row:?}");
            assert!(row.starts_with("> "), "and it is the marked one: {row:?}");
        }
    }

    #[test]
    fn the_selection_pulses_rather_than_sitting_on_one_level() {
        let case = Case {
            selected: 2,
            ..Case::default()
        };
        let trough = case.buf(W, H);
        let area = Rect::new(0, 0, W, H);
        let mut peak = buffer(W, H);
        EqMenu {
            gains: &case.gains,
            saved: &case.saved,
            source: case.source,
            modified: case.modified,
            selected: case.selected,
            state: &case.state,
            elapsed: anim::SELECTION_PULSE_PERIOD / 2,
        }
        .render(area, &mut peak);
        let y = plated_rows(&trough)[0];
        assert_ne!(
            trough.cell((0, y)).unwrap().bg,
            peak.cell((0, y)).unwrap().bg
        );
    }

    #[test]
    fn the_name_editor_shows_what_was_typed_with_a_cursor_after_it() {
        let mut editor = NameEditor::save();
        for ch in "night bus".chars() {
            editor.push(ch);
        }
        let state = EqMenuState {
            editor: Some(editor),
            ..Default::default()
        };
        let out = Case {
            state,
            selected: 20,
            ..Case::default()
        }
        .grid();
        assert!(out.contains("save as: night bus█"), "{out}");
        // the header switches to the editor's own keys while it owns
        // them, instead of advertising keys that now type letters.
        assert!(out.contains("enter: save the name, esc: cancel"), "{out}");
        // and the saved group's key legend goes with it: those
        // letters type a name while the editor is up.
        assert!(!out.contains("s save · r rename"), "{out}");
    }

    #[test]
    fn a_rename_editor_starts_from_the_current_name() {
        let state = EqMenuState {
            editor: Some(NameEditor::rename(0, "night")),
            message: "press enter again to overwrite".to_string(),
            ..Default::default()
        };
        let out = Case {
            saved: store(&["night"]),
            state,
            ..Case::default()
        }
        .grid();
        assert!(out.contains("rename to: night█"), "{out}");
        assert!(
            out.contains("press enter again to overwrite"),
            "the confirm prompt sits on the editor's own row:\n{out}"
        );
    }

    #[test]
    fn a_message_shows_when_nothing_is_being_typed() {
        let state = EqMenuState {
            message: "press d again to delete 'night'".to_string(),
            ..Default::default()
        };
        let out = Case {
            saved: store(&["night"]),
            state,
            selected: PRESET_COUNT,
            ..Case::default()
        }
        .grid();
        assert!(out.contains("press d again to delete 'night'"), "{out}");
    }

    /// The body scrolls as one list, so a band row stays reachable
    /// however many presets are saved above it.
    #[test]
    fn the_body_scrolls_to_keep_the_selection_visible() {
        let saved = store(&["a", "b", "c", "d", "e", "f", "g", "h"]);
        let selected = selectable_count(saved.len()) - 1;
        let out = Case {
            saved,
            selected,
            gains: TUNED,
            ..Case::default()
        }
        .grid();
        assert!(out.contains("> 16000 Hz"), "last band off screen:\n{out}");
    }

    #[test]
    fn renders_at_degenerate_sizes_without_panicking() {
        let saved = store(&["night"]);
        let state = EqMenuState {
            editor: Some(NameEditor::rename(0, "a very long name indeed")),
            message: "a message long enough to overrun a narrow pane".to_string(),
            ..Default::default()
        };
        for (w, h) in [
            (1, 1),
            (0, 10),
            (10, 0),
            (3, 3),
            (8, 9),
            (W, 1),
            (W, 2),
            (W, 5),
            (200, 60),
        ] {
            for selected in [0, PRESET_COUNT, selectable_count(1) - 1, 999] {
                let area = Rect::new(0, 0, w, h);
                let mut buf = buffer(w.max(1), h.max(1));
                EqMenu {
                    gains: &TUNED,
                    saved: &saved,
                    source: Some(Source::Saved(0)),
                    modified: true,
                    selected,
                    state: &state,
                    elapsed: Duration::ZERO,
                }
                .render(area, &mut buf);
            }
        }
    }

    #[test]
    fn the_cursor_never_lands_between_the_groups() {
        assert_eq!(cursor(0, 2), Cursor::Builtin(0));
        assert_eq!(
            cursor(PRESET_COUNT - 1, 2),
            Cursor::Builtin(PRESET_COUNT - 1)
        );
        assert_eq!(cursor(PRESET_COUNT, 2), Cursor::Saved(0));
        assert_eq!(cursor(PRESET_COUNT + 1, 2), Cursor::Saved(1));
        assert_eq!(cursor(PRESET_COUNT + 2, 2), Cursor::Band(0));
        // no saved presets: the saved group is skipped entirely.
        assert_eq!(cursor(PRESET_COUNT, 0), Cursor::Band(0));
        // past the end clamps onto the last band rather than wrapping
        // round to a preset row, which would overwrite all ten gains.
        assert_eq!(cursor(999, 0), Cursor::Band(BAND_COUNT - 1));
    }

    #[test]
    fn opening_lands_on_whatever_is_applied() {
        assert_eq!(opening_selection(Some(Source::Builtin(3)), 2), 3);
        assert_eq!(
            opening_selection(Some(Source::Saved(1)), 2),
            PRESET_COUNT + 1
        );
        // a nameless curve has no row, so the bands get the cursor.
        assert_eq!(opening_selection(None, 2), first_band_row(2));
        // a stale index (its preset was deleted) must not point off
        // the end of the list.
        assert_eq!(
            opening_selection(Some(Source::Saved(9)), 2),
            first_band_row(2)
        );
    }

    /// The gate itself: one press arms, a second on the same target
    /// commits, and anything else at all disarms it.
    #[test]
    fn a_confirm_only_fires_on_a_second_press_on_the_same_target() {
        let mut confirm: Confirm<usize> = Confirm::default();
        assert!(!confirm.press(3), "the first press must never commit");
        assert_eq!(confirm.armed(), Some(&3));
        assert!(confirm.press(3));
        assert_eq!(confirm.armed(), None, "committing disarms");
        // a different target re-arms instead of committing, so moving
        // the cursor and pressing again cannot delete the wrong row.
        assert!(!confirm.press(3));
        assert!(!confirm.press(4));
        assert_eq!(confirm.armed(), Some(&4));
        // and any other key disarms.
        confirm.disarm();
        assert!(!confirm.press(4));
    }

    #[test]
    fn editing_the_name_disarms_a_pending_overwrite() {
        let mut editor = NameEditor::save();
        for ch in "night".chars() {
            editor.push(ch);
        }
        assert!(!editor.overwrite.press(0));
        editor.push('s');
        assert_eq!(
            editor.overwrite.armed(),
            None,
            "the name changed, so the confirm is about a different name"
        );
        assert!(!editor.overwrite.press(0));
        editor.pop();
        assert_eq!(editor.overwrite.armed(), None);
    }

    #[test]
    fn the_name_input_refuses_control_characters_and_stops_at_the_cap() {
        let mut editor = NameEditor::save();
        editor.push('\n');
        editor.push('\t');
        assert_eq!(editor.text, "");
        for _ in 0..MAX_NAME_LEN + 10 {
            editor.push('x');
        }
        assert_eq!(editor.text.chars().count(), MAX_NAME_LEN);
    }
}
