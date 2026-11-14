use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::keymap::{key_label, KeyAction, Keymap};
use crate::theme::{shade, DIM, MID, NEAR_BLACK, NEAR_WHITE};
use crate::ui::header::draw_header;
use crate::ui::textfit::{truncate_with_ellipsis, width};

/// Full keybind reference, opened with `?` (hinted by `HelpBox` in the
/// bottom-right corner); takes over the whole screen the same way
/// onboarding/the nav manager do, since it's a brief reference lookup
/// rather than something you'd want to see alongside anything else.
/// Looks up every remappable entry's *current* key from `keymap`
/// live, rather than a hardcoded default string that would silently
/// go stale the moment anything got rebound via 'K'.
pub struct HelpOverlay<'a> {
    pub keymap: &'a Keymap,
    /// Whether mouse capture is on (`config::Config::mouse`); the
    /// mouse section is dropped when it isn't, since a reference that
    /// lists input the app is not accepting is worse than one that
    /// says nothing.
    pub mouse: bool,
    /// First content row drawn, in the same units `content_rows`
    /// reports. The table outgrew one screen (64 rows against the 20
    /// a 24-row terminal leaves), and the old layout resolved that by
    /// letting the last column overflow and be clipped, which silently
    /// ate the section holding how to quit. Scrolling makes a new
    /// section cost a keypress instead of a disappearance.
    pub scroll: usize,
}

/// `Fixed` for universal navigation primitives and modal-internal
/// conventions that deliberately aren't part of the remappable
/// keymap (see keymap.rs); `Bound` for anything that is, so its
/// label always reflects whatever it's actually currently bound to.
/// Lives here rather than in keymap.rs because it is about *showing* a
/// binding, not resolving one; onboarding's controls screen shares it
/// (it had the same baked-in-literal bug this enum exists to prevent).
pub(crate) enum Key {
    Fixed(&'static str),
    Bound(KeyAction),
    /// Two remappable actions that read as one row ("speed down /
    /// speed up"). Spelling such a pair as `Fixed("[ / ]")` is how
    /// this table ended up lying about four rebindable keys: the text
    /// was baked in, so a remap left the help showing the old pair.
    Pair(KeyAction, KeyAction),
}

impl Key {
    /// The label to print, resolved against the live keymap so a
    /// rebound key is never shown as the default it replaced.
    pub(crate) fn label(&self, keymap: &Keymap) -> String {
        match self {
            Key::Fixed(s) => (*s).to_string(),
            Key::Bound(action) => key_label(keymap.get(*action)),
            Key::Pair(a, b) => {
                format!(
                    "{} / {}",
                    key_label(keymap.get(*a)),
                    key_label(keymap.get(*b))
                )
            }
        }
    }
}

const SECTIONS: &[(&str, &[(Key, &str)])] = &[
    (
        "navigation",
        &[
            (Key::Bound(KeyAction::StartSearch), "search"),
            (Key::Fixed("j / k"), "move selection"),
            (Key::Fixed("gg / G"), "jump to top / bottom"),
            (Key::Fixed("enter"), "play / open"),
            (Key::Fixed("esc"), "back / close"),
            (Key::Fixed("backspace"), "edit the query"),
            (Key::Fixed("tab"), "enter move mode"),
            (Key::Fixed("wasd / arrows"), "move: focus pane"),
            (Key::Fixed("enter / esc"), "move: back to interact"),
            (Key::Fixed("left/right"), "cycle home screen logo"),
            (Key::Fixed("0"), "reset logo to default"),
        ],
    ),
    (
        "playback",
        &[
            (Key::Bound(KeyAction::TogglePause), "pause / resume"),
            (Key::Bound(KeyAction::Enqueue), "queue"),
            (Key::Bound(KeyAction::SkipNext), "skip to next"),
            (Key::Bound(KeyAction::SkipBack), "back to previous"),
            (Key::Bound(KeyAction::CycleLoop), "cycle loop"),
            (Key::Bound(KeyAction::ToggleShuffle), "shuffle"),
            (
                Key::Pair(KeyAction::SpeedDown, KeyAction::SpeedUp),
                "speed down / up",
            ),
            (Key::Bound(KeyAction::CycleEq), "cycle eq preset"),
            (Key::Bound(KeyAction::ToggleCrossfade), "toggle crossfade"),
            (
                Key::Pair(KeyAction::SubtitleDelayDown, KeyAction::SubtitleDelayUp),
                "subtitle delay -/+ 0.5s",
            ),
            (Key::Bound(KeyAction::OpenSubtitleMenu), "subtitle settings"),
        ],
    ),
    (
        "library",
        &[
            (Key::Bound(KeyAction::OpenNavManager), "navidrome profiles"),
            (Key::Bound(KeyAction::OpenStarredView), "starred songs"),
            (Key::Bound(KeyAction::OpenQueueView), "the queue"),
            (Key::Bound(KeyAction::OpenSuggestions), "suggestions"),
            (Key::Bound(KeyAction::IndexBreakdown), "index breakdown"),
            (Key::Bound(KeyAction::ToggleStar), "star / unstar"),
            (
                Key::Bound(KeyAction::DownloadSelected),
                "download for offline",
            ),
            (
                Key::Bound(KeyAction::OpenLyrics),
                "lyrics for current track",
            ),
            (Key::Fixed("left/right"), "artist: switch column"),
            (Key::Bound(KeyAction::StartSearch), "artist: filter songs"),
            (Key::Fixed("W"), "Watch workspace"),
            (Key::Fixed("T"), "TorBox workspace"),
            (Key::Fixed("J / K"), "queue: move track down / up"),
        ],
    ),
    (
        "view",
        &[
            (Key::Bound(KeyAction::ToggleMinimize), "minimize player"),
            (
                Key::Bound(KeyAction::ToggleFullscreenVideo),
                "fullscreen video in mpv",
            ),
            (
                Key::Bound(KeyAction::OpenVideoPlayer),
                "open / focus mpv video",
            ),
            (Key::Fixed("F / f11"), "fullscreen video"),
            (Key::Bound(KeyAction::CycleArtMode), "cycle album art"),
            (Key::Bound(KeyAction::ToggleResizeMode), "resize panes"),
        ],
    ),
    (
        "mouse",
        &[
            (Key::Fixed("click"), "focus pane / row"),
            (Key::Fixed("click"), "press a bottom-bar control"),
            (Key::Fixed("drag"), "scrub the progress bar"),
            (Key::Fixed("wheel"), "scroll the list under it"),
            (Key::Fixed("shift+drag"), "select text (bypass capture)"),
        ],
    ),
    (
        // the app has quietly supported nucleo's anchors and exact
        // match all along and never said so anywhere; `!` now works
        // too (see search.rs's split_exclusions), so the whole syntax
        // is worth one block rather than being folklore.
        "search syntax",
        &[
            (Key::Fixed("foo bar"), "fuzzy, typos forgiven"),
            (Key::Fixed("'foo"), "exact substring"),
            (Key::Fixed("^foo"), "starts with"),
            (Key::Fixed("foo$"), "ends with"),
            (Key::Fixed("!foo"), "exclude matches"),
        ],
    ),
    (
        // `:` opens an entire command language (docs/shellsong.md) and
        // this reference never mentioned it once, so the only way to
        // find the console was to guess at a key. It is in
        // `reserved_key_purpose`, which means no remap can ever reach
        // it and this table is the only place it can be learned.
        "console",
        &[
            (Key::Fixed(":"), "open the command console"),
            (Key::Fixed("help()"), "list every command"),
            (Key::Fixed("play(x)"), "play or queue by name"),
            (Key::Fixed("view(queue)"), "open a screen by name"),
            (Key::Fixed("tab"), "complete what you typed"),
            (Key::Fixed("up / down"), "recall a past command"),
        ],
    ),
    (
        "other",
        &[
            (Key::Bound(KeyAction::OpenSettingsMenu), "settings"),
            (Key::Fixed("K"), "remap keybinds"),
            (Key::Bound(KeyAction::Quit), "quit"),
            (Key::Fixed("ctrl+c"), "quit from anywhere"),
            (Key::Bound(KeyAction::OpenHelp), "close this"),
        ],
    ),
];

impl Widget for HelpOverlay<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // takes over a rect that already has other content drawn
        // into it, so it has to own every cell: without the clear,
        // whatever this menu did not write still showed through.
        let area = crate::ui::draw::overlay(buf, area);

        let top = draw_header(
            buf,
            area,
            area.y,
            "keybinds",
            &format!(
                "{} or esc to close",
                key_label(self.keymap.get(KeyAction::OpenHelp))
            ),
            shade(DIM),
            shade(NEAR_BLACK),
        ) + 1;

        debug_assert_eq!(
            top,
            area.y + HEADER_ROWS,
            "max_scroll derives `top` arithmetically; keep it equal to the header"
        );

        // laid out in columns rather than one tall list: the full
        // reference is ~50 rows, so on a normal 24-row terminal the
        // last sections (including how to quit) used to just silently
        // fall off the bottom. columns are balanced by row count, and
        // a narrow terminal falls back to a single column. taller than
        // the screen now scrolls rather than being clipped.
        let Layout {
            rows,
            tallest,
            scroll_max,
            width: body_width,
        } = self.layout(area);
        let blocks = self.rendered_blocks();
        let columns = split_into_columns(&blocks, rows, body_width);

        let col_width = column_width(body_width, columns.len());
        // clamped here as well as by the caller: `area` is what decides
        // how much fits, and only the render sees the real one.
        let scroll = self.scroll.min(scroll_max);

        for (i, column) in columns.iter().enumerate() {
            let left = i * (col_width + COLUMN_GUTTER);
            // the integer division's remainder goes to the last column,
            // so its heading rule stops exactly where the body does
            // rather than a ragged cell or two short of it.
            let w = if i + 1 == columns.len() {
                body_width.saturating_sub(left)
            } else {
                col_width
            };
            let x = area.x + left as u16;
            for (y, line) in (top..top + rows as u16).zip(column.iter().skip(scroll)) {
                if y >= area.y + area.height {
                    break;
                }
                draw_line(buf, area, x, w, y, line);
            }
        }

        // only drawn when there is something off-screen, so a table that
        // fits says nothing and loses no row to the hint.
        if scroll_max == 0 {
            return;
        }
        let left = tallest.saturating_sub(rows + scroll);
        // the same `▴`/`▾` edge cues every windowed list in the ui
        // uses, in the one column `layout` held back for them: the
        // count below says how much is left, these say which way.
        if rows > 0 {
            let cue_x = area.x + area.width - 1;
            if scroll > 0 {
                crate::ui::draw::put(buf, cue_x, top, '▴', shade(DIM));
            }
            if left > 0 {
                crate::ui::draw::put(buf, cue_x, top + rows as u16 - 1, '▾', shade(DIM));
            }
        }
        let hint = if left > 0 {
            format!("{left} more below · j/k, wheel")
        } else {
            "end of the table · k, wheel up".to_string()
        };
        let hint = truncate_with_ellipsis(&hint, area.width as usize);
        let w = width(&hint);
        let y = top + rows as u16;
        let x = area.x + area.width.saturating_sub(w as u16);
        draw_line(buf, area, x, w, y, &Line::Hint(hint));
    }
}

/// Rows the header owns before the table starts, counted from `area.y`.
/// `draw_header` returns `y + 2` and the overlay adds one, so this is
/// that sum written down where `max_scroll` can reach it without a
/// `Buffer`; the render `debug_assert`s the two agree.
const HEADER_ROWS: u16 = 3;

/// What the render resolved for one `area`, so `max_scroll` and the
/// render cannot disagree about how much fits.
struct Layout {
    /// Content rows drawn, after the hint row is reserved.
    rows: usize,
    /// Rows the tallest column needs.
    tallest: usize,
    /// Largest useful `scroll`; 0 when everything fits.
    scroll_max: usize,
    /// Cells the columns get, after the `▴`/`▾` gutter is reserved.
    width: usize,
}

impl HelpOverlay<'_> {
    fn layout(&self, area: Rect) -> Layout {
        let full = area.height.saturating_sub(HEADER_ROWS) as usize;
        let blocks = self.rendered_blocks();
        let tallest = |rows: usize, width: usize| {
            split_into_columns(&blocks, rows, width)
                .iter()
                .map(Vec::len)
                .max()
                .unwrap_or(0)
        };
        // two passes on purpose: the hint row and the cue gutter are
        // only owed when it scrolls, and whether it scrolls depends on
        // a packing that depends on both. one direction only, so it
        // cannot oscillate: taking a row and a column away can only
        // make the table taller, never short enough to fit.
        let full_width = area.width as usize;
        if tallest(full, full_width) <= full {
            return Layout {
                rows: full,
                tallest: tallest(full, full_width),
                scroll_max: 0,
                width: full_width,
            };
        }
        let width = full_width.saturating_sub(1);
        let rows = full.saturating_sub(1);
        let t = tallest(rows, width);
        Layout {
            rows,
            tallest: t,
            scroll_max: t.saturating_sub(rows),
            width,
        }
    }

    /// Largest `scroll` worth passing for this `area`, so the caller can
    /// clamp a key or wheel step without a `Buffer`. Takes the *outer*
    /// rect and insets it exactly as the render's `draw::overlay` does.
    pub fn max_scroll(&self, area: Rect) -> usize {
        self.layout(crate::ui::draw::framed_inner(area)).scroll_max
    }
}

/// One rendered line. Three kinds rather than one string plus a
/// brightness, because two of them are two-tone: a heading is its name
/// over a rule, and a bind row is a key over a description. The row
/// cannot be built without the column width, so it is built at the
/// draw rather than here.
enum Line {
    /// Section name followed by a rule to the column's edge, so the
    /// eight groups read as groups instead of one long list.
    Heading(&'static str),
    /// Key and what it does. Painted a shade apart: the key column is
    /// what the eye scans down, and both at one level gave it nothing
    /// to hook onto.
    Bind { key: String, desc: &'static str },
    /// Air under a section.
    Gap,
    /// The scroll footer, right-aligned by the render.
    Hint(String),
}

impl Line {
    /// The heading's own name, for the tests that check which sections
    /// are in the table.
    #[cfg(test)]
    fn heading(&self) -> Option<&'static str> {
        match self {
            Line::Heading(name) => Some(name),
            _ => None,
        }
    }
}

impl HelpOverlay<'_> {
    /// `SECTIONS` flattened into drawable lines, resolving every
    /// `Key::Bound` against the live keymap so a rebound key is never
    /// shown as its default. Grouped per section so `split_into_columns`
    /// can avoid breaking a section across a column boundary.
    fn rendered_blocks(&self) -> Vec<Vec<Line>> {
        SECTIONS
            .iter()
            .filter(|(section, _)| self.mouse || *section != MOUSE_SECTION)
            .map(|(section, binds)| {
                let mut block = vec![Line::Heading(section)];
                for (key, desc) in *binds {
                    block.push(Line::Bind {
                        key: key.label(self.keymap),
                        desc,
                    });
                }
                block.push(Line::Gap);
                block
            })
            .collect()
    }
}

/// The one section that is conditional, named rather than matched by
/// index so reordering `SECTIONS` cannot drop the wrong one.
const MOUSE_SECTION: &str = "mouse";

/// Cells reserved for the key column, so every description in every
/// section starts at the same offset. Fixed rather than derived from
/// the widest resolved label on purpose: a single long rebind ("f12")
/// would otherwise shift every description in the table, and
/// `MIN_COLUMN_WIDTH` below is computed from this number.
const KEY_COLUMN: usize = 13;

/// Cells between the key column and the descriptions. Two, not one:
/// "wasd / arrows" is exactly `KEY_COLUMN` wide, so one cell of gutter
/// was none at all and that row read "wasd / arrowsmove: focus pane".
/// Same two-cell gutter `keybinds_menu`'s key column uses.
const KEY_GUTTER: usize = 2;

/// Cells between two columns of the table. At a width that fit two
/// minimum columns exactly, the widest row of the left one ran into
/// the heading of the right one with nothing between them.
const COLUMN_GUTTER: usize = 2;

/// Minimum usable width for one column: the widest line this table
/// actually produces, i.e. `"  "` + `KEY_COLUMN` + `KEY_GUTTER` + the
/// longest description. Keep it equal to that, not below it. At 38 a
/// 120-cell terminal took three 40-cell columns and silently truncated
/// the three widest rows mid-word ("press a bottom-bar con", "select
/// text (bypass ca"); the overlay scrolls now, so trading a column for
/// readable lines costs a keypress rather than the content.
/// `no_row_is_wider_than_a_column` keeps this honest against the table.
const MIN_COLUMN_WIDTH: usize = 45;

/// Cells one of `columns` columns gets out of `body_width`, with
/// `COLUMN_GUTTER` between each neighbouring pair. Shared by the render
/// and the tests, since "where does column 1 start" is the thing a
/// gutter has to get right.
fn column_width(body_width: usize, columns: usize) -> usize {
    if columns <= 1 {
        return body_width;
    }
    body_width.saturating_sub(COLUMN_GUTTER * (columns - 1)) / columns
}

/// Packs whole sections into columns, never splitting a section across
/// a column boundary. First-fit rather than "fill one column then move
/// on": filling greedily left a tall section stranded with nowhere to
/// go and overflowed the last column off the bottom, which is the
/// exact failure this layout exists to prevent. Source order within a
/// column is preserved, so reading is column-major but no section is
/// ever interleaved with another.
fn split_into_columns(
    blocks: &[Vec<Line>],
    rows_available: usize,
    width: usize,
) -> Vec<Vec<&Line>> {
    // the gutters count against the width too: without them a width
    // that held n minimum columns edge to edge claimed n and then had
    // nothing left to separate them with.
    let max_columns = ((width + COLUMN_GUTTER) / (MIN_COLUMN_WIDTH + COLUMN_GUTTER)).max(1);
    if max_columns == 1 || rows_available == 0 {
        return vec![blocks.iter().flatten().collect()];
    }
    let mut columns: Vec<Vec<&Line>> = vec![Vec::new()];
    for block in blocks {
        let target = columns
            .iter()
            .position(|c| c.len() + block.len() <= rows_available);
        let idx = match target {
            Some(i) => i,
            None if columns.len() < max_columns => {
                columns.push(Vec::new());
                columns.len() - 1
            }
            // out of columns and out of rows: put it in the emptiest
            // one and let it overflow, rather than dropping it.
            None => columns
                .iter()
                .enumerate()
                .min_by_key(|(_, c)| c.len())
                .map(|(i, _)| i)
                .expect("seeded with one column"),
        };
        columns[idx].extend(block.iter());
    }
    columns
}

/// One column's row: `x0`/`max_width` pick the column out of `area`.
/// This copy was the one missing the vertical guard the other eight
/// had, so it could draw outside its own rect.
///
/// Two of the four kinds are painted in two passes, which is what
/// gives the table a hierarchy: `draw::at` hands back the x it reached,
/// so the second pass starts where the first stopped rather than at a
/// recomputed offset that a wide glyph would put a cell out.
fn draw_line(buf: &mut Buffer, area: Rect, x0: u16, max_width: usize, y: u16, line: &Line) {
    if y < area.y || y >= area.y + area.height {
        return;
    }
    let budget = max_width.min((area.x + area.width).saturating_sub(x0) as usize);
    let paint = |buf: &mut Buffer, x: u16, text: &str, level: f32, room: usize| -> u16 {
        crate::ui::draw::at(buf, x, y, text, shade(level), room)
    };
    match line {
        Line::Gap => {}
        Line::Hint(text) => {
            paint(buf, x0, text, DIM, budget);
        }
        Line::Heading(name) => {
            let end = paint(buf, x0, name, NEAR_WHITE, budget);
            let used = end.saturating_sub(x0) as usize;
            // `NEAR_BLACK`, the same level `draw::frame` uses: a rule
            // at the brightness of its own content reads as noise
            // rather than as structure.
            // a cell of air at each end: the `▴`/`▾` gutter is the
            // last column of the body, and a cue butted straight onto
            // a rule read as part of the rule.
            let left = budget.saturating_sub(used);
            let rule = format!(" {}", "─".repeat(left.saturating_sub(2)));
            paint(buf, end, &rule, NEAR_BLACK, left);
        }
        Line::Bind { key, desc } => {
            // padded by display width, by hand: `{key:<13}` counts
            // chars, so a key bound to a wide char was one cell wider
            // than it counted and pushed its own description a cell
            // right of the column. same fix banner.rs records.
            let pad = " ".repeat(KEY_COLUMN.saturating_sub(width(key)) + KEY_GUTTER);
            let end = paint(buf, x0, &format!("  {key}{pad}"), MID, budget);
            let used = end.saturating_sub(x0) as usize;
            paint(buf, end, desc, DIM, budget.saturating_sub(used));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::Keymap;
    use crate::ui::testbuf::{
        assert_no_bleed_through, assert_outside_untouched, buffer, grid, plated_buffer,
    };

    /// Every row the overlay can show at this size, across all reachable
    /// scroll offsets. The table is taller than a 24-row terminal, so
    /// "is it on screen" is the wrong question; "can the user get to it"
    /// is the one the layout actually has to answer.
    fn reachable(width: u16, height: u16) -> String {
        let keymap = Keymap::default();
        let max = HelpOverlay {
            keymap: &keymap,
            mouse: true,
            scroll: 0,
        }
        .max_scroll(Rect::new(0, 0, width, height));
        (0..=max)
            .map(|s| render_at(width, height, s).join("\n"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn render_at(width: u16, height: u16, scroll: usize) -> Vec<String> {
        let keymap = Keymap::default();
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        HelpOverlay {
            keymap: &keymap,
            mouse: true,
            scroll,
        }
        .render(area, &mut buf);
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn the_overlay_covers_every_cell_of_its_rect() {
        let keymap = Keymap::default();
        let mut buf = plated_buffer(80, 24);
        let area = Rect::new(2, 1, 76, 22);
        HelpOverlay {
            keymap: &keymap,
            mouse: true,
            scroll: 0,
        }
        .render(area, &mut buf);
        assert_no_bleed_through(&buf, area);
    }

    /// And nothing outside it: the heading rule and the cue gutter are
    /// both drawn from a width rather than from a string, which is the
    /// shape of writer that runs off the right edge.
    #[test]
    fn nothing_is_painted_outside_the_rect() {
        use crossterm::event::KeyCode;
        let mut keymap = Keymap::default();
        // two cells per glyph on one key and one emoji cluster on
        // another, so neither the pad nor the rule can be measured by
        // char count and stay inside.
        keymap.set(KeyAction::CycleEq, KeyCode::Char('\u{65e5}'));
        keymap.set(KeyAction::ToggleStar, KeyCode::Char('\u{1f3b5}'));
        for (w, h) in [(60u16, 20u16), (100, 30), (14, 8), (49, 12)] {
            for scroll in [0usize, 3, 400] {
                let mut buf = buffer(w + 6, h + 4);
                let area = Rect::new(3, 2, w, h);
                HelpOverlay {
                    keymap: &keymap,
                    mouse: true,
                    scroll,
                }
                .render(area, &mut buf);
                assert_outside_untouched(&buf, area);
            }
        }
    }

    #[test]
    fn the_frame_closes_all_four_corners() {
        let keymap = Keymap::default();
        let mut buf = buffer(80, 24);
        let area = Rect::new(2, 1, 76, 22);
        HelpOverlay {
            keymap: &keymap,
            mouse: true,
            scroll: 0,
        }
        .render(area, &mut buf);
        let g = grid(&buf);
        let rows: Vec<Vec<char>> = g.lines().map(|r| r.chars().collect()).collect();
        assert_eq!(
            (rows[1][2], rows[1][77], rows[22][2], rows[22][77]),
            ('┌', '┐', '└', '┘')
        );
    }

    #[test]
    fn every_section_heading_is_reachable_on_a_standard_terminal() {
        // the single-column layout used to drop the last sections
        // (quit included) off the bottom with no indication they
        // existed, and the column layout then did the same thing the
        // moment the table outgrew three columns. scrolling is what
        // makes this assertion hold for any number of sections.
        let rendered = reachable(120, 24);
        for (section, _) in SECTIONS {
            assert!(
                rendered.contains(section),
                "section {section:?} cannot be reached at 120x24:\n{rendered}"
            );
        }
    }

    /// The rows themselves, not just the headings: a section whose
    /// heading is on the last visible row is not actually readable.
    #[test]
    fn every_bind_row_is_reachable_on_a_standard_terminal() {
        let keymap = Keymap::default();
        let rendered = reachable(120, 24);
        for (section, binds) in SECTIONS {
            for (key, desc) in *binds {
                assert!(
                    rendered.contains(desc),
                    "{section:?} / {desc:?} (key {:?}) cannot be reached at 120x24",
                    key.label(&keymap)
                );
            }
        }
    }

    /// The reference must not advertise input the app is not
    /// accepting, and must advertise it when it is: a feature nobody
    /// can find out about is only half shipped.
    #[test]
    fn the_mouse_section_follows_whether_capture_is_on() {
        let keymap = Keymap::default();
        let headings = |mouse| -> Vec<String> {
            HelpOverlay {
                keymap: &keymap,
                mouse,
                scroll: 0,
            }
            .rendered_blocks()
            .iter()
            .filter_map(|b| b.first().and_then(Line::heading))
            .map(str::to_string)
            .collect()
        };
        assert!(headings(true).contains(&MOUSE_SECTION.to_string()));
        assert!(!headings(false).contains(&MOUSE_SECTION.to_string()));
        // and nothing else moved: only that one section is
        // conditional.
        assert_eq!(headings(true).len(), headings(false).len() + 1);
        assert_eq!(headings(true).len(), SECTIONS.len());
    }

    #[test]
    fn how_to_quit_is_always_reachable() {
        assert!(reachable(120, 24).contains("quit from anywhere"));
    }

    /// A reserved key is one no remap can reach, which makes this
    /// table the only place its purpose is written down for a user.
    /// `:` sat in `RESERVED_KEYS` naming "the shellsong console", a
    /// whole command language with its own completion, and appeared in
    /// no section, no onboarding row and no hint: the subsystem was
    /// reachable only by guessing the key.
    #[test]
    fn every_reserved_key_is_named_somewhere_in_the_table() {
        let keymap = Keymap::default();
        let listed = SECTIONS
            .iter()
            .flat_map(|(_, binds)| binds.iter())
            .map(|(key, _)| key.label(&keymap))
            .collect::<Vec<_>>()
            .join(" | ");
        for (key, purpose) in crate::keymap::RESERVED_KEYS {
            let label = key_label(*key);
            assert!(
                listed.contains(&label),
                "{label:?} is reserved for {purpose:?} and no row mentions it:\n{listed}"
            );
        }
    }

    /// The column width is derived from the table, so a description
    /// longer than `MIN_COLUMN_WIDTH` allows for silently truncates
    /// mid-word. That is how "press a bottom-bar con" and "select text
    /// (bypass ca" shipped: the mouse section added the two widest
    /// rows and nothing recomputed the constant.
    #[test]
    fn no_row_is_wider_than_a_column() {
        let keymap = Keymap::default();
        let widest = SECTIONS
            .iter()
            .flat_map(|(_, binds)| binds.iter())
            .map(|(key, desc)| {
                2 + KEY_COLUMN.max(width(&key.label(&keymap))) + KEY_GUTTER + width(desc)
            })
            .max()
            .unwrap_or(0);
        assert!(
            widest <= MIN_COLUMN_WIDTH,
            "widest row is {widest} cells but MIN_COLUMN_WIDTH is {MIN_COLUMN_WIDTH}; \
             a column that narrow truncates it"
        );
    }

    #[test]
    fn a_narrow_terminal_falls_back_to_one_column() {
        let blocks_owner = {
            let keymap = Keymap::default();
            HelpOverlay {
                keymap: &keymap,
                mouse: true,
                scroll: 0,
            }
            .rendered_blocks()
        };
        let cols = split_into_columns(&blocks_owner, 24, 50);
        assert_eq!(cols.len(), 1, "50 cols can't hold two readable columns");
        // and a width that holds two of them edge to edge still can't,
        // because they would have nothing between them.
        assert_eq!(
            split_into_columns(&blocks_owner, 24, MIN_COLUMN_WIDTH * 2).len(),
            1,
            "two columns need a gutter as well as their own width"
        );
        assert_eq!(
            split_into_columns(&blocks_owner, 24, MIN_COLUMN_WIDTH * 2 + COLUMN_GUTTER).len(),
            2,
            "and that is exactly enough for two"
        );
    }

    #[test]
    fn a_section_is_never_split_across_columns() {
        let keymap = Keymap::default();
        let blocks = HelpOverlay {
            keymap: &keymap,
            mouse: true,
            scroll: 0,
        }
        .rendered_blocks();
        let cols = split_into_columns(&blocks, 12, 160);
        // each section's heading must be followed by its own binds in
        // the same column, so count headings per column and make sure
        // no column starts with a bind row (an orphaned continuation).
        for col in &cols {
            if let Some(first) = col.first() {
                assert!(
                    first.heading().is_some(),
                    "column starts mid-section, not on a heading"
                );
            }
        }
    }

    #[test]
    fn the_header_hint_follows_the_live_keymap() {
        use crossterm::event::KeyCode;
        let mut keymap = Keymap::default();
        keymap.set(KeyAction::OpenHelp, KeyCode::Char('y'));
        let area = Rect::new(0, 0, 120, 24);
        let mut buf = Buffer::empty(area);
        HelpOverlay {
            keymap: &keymap,
            mouse: true,
            scroll: 0,
        }
        .render(area, &mut buf);
        // row 0 is the frame's top edge now; the title is one inside.
        let top: String = (0..area.width)
            .map(|x| buf.cell((x, 1)).unwrap().symbol().to_string())
            .collect();
        assert!(top.contains("y or esc to close"), "got:\n{top}");
        assert!(!top.contains("? or esc to close"), "stale default:\n{top}");
    }

    #[test]
    fn renders_without_panicking_at_degenerate_sizes() {
        for (w, h) in [(1, 1), (1, 0), (200, 1), (3, 3), (2, 3), (6, 4), (0, 10)] {
            // every scroll a caller could hand over, including one
            // past the end: the cue rows are derived from `rows`, which
            // a pane this small leaves at zero.
            for scroll in [0usize, 1, usize::MAX] {
                let area = Rect::new(0, 0, w, h);
                let mut buf = Buffer::empty(area);
                let keymap = Keymap::default();
                HelpOverlay {
                    keymap: &keymap,
                    mouse: true,
                    scroll,
                }
                .render(area, &mut buf);
            }
        }
    }

    #[test]
    fn a_paired_row_follows_both_of_its_rebound_keys() {
        use crossterm::event::KeyCode;
        let mut keymap = Keymap::default();
        keymap.set(KeyAction::SpeedDown, KeyCode::Char('x'));
        keymap.set(KeyAction::SpeedUp, KeyCode::Char('y'));
        let area = Rect::new(0, 0, 120, 30);
        let mut buf = Buffer::empty(area);
        HelpOverlay {
            keymap: &keymap,
            mouse: true,
            scroll: 0,
        }
        .render(area, &mut buf);
        let rendered: String = (0..30)
            .map(|y| {
                (0..120)
                    .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            rendered.contains("x / y"),
            "the speed row must read the live keymap:\n{rendered}"
        );
        assert!(
            !rendered.contains("[ / ]"),
            "and must not still show the baked-in default"
        );
    }

    /// Every remappable action is reachable from this table, which is
    /// where a user finds out an action exists at all.
    #[test]
    fn every_remappable_action_has_a_row_in_the_table() {
        let listed: Vec<KeyAction> = SECTIONS
            .iter()
            .flat_map(|(_, binds)| binds.iter())
            .flat_map(|(key, _)| match key {
                Key::Bound(a) => vec![*a],
                Key::Pair(a, b) => vec![*a, *b],
                Key::Fixed(_) => Vec::new(),
            })
            .collect();
        for action in crate::keymap::ALL_ACTIONS {
            assert!(
                listed.contains(action),
                "{:?} ({:?}) is remappable and no row names it",
                action.name(),
                action.label()
            );
        }
    }

    /// Where the row's two halves meet. `"  "` of indent, the key
    /// column, then `KEY_GUTTER` cells of air before the description.
    const DESC_COLUMN: usize = 2 + KEY_COLUMN + KEY_GUTTER;

    /// The row the eyeball caught: "wasd / arrows" is exactly
    /// `KEY_COLUMN` cells, so one cell of gutter was no gutter at all
    /// and that row printed "wasd / arrowsmove: focus pane". Checked
    /// for every row at once, since the fix is a column and not a row.
    #[test]
    fn every_bind_row_has_a_clear_gutter_between_its_key_and_its_description() {
        let keymap = Keymap::default();
        // one column, tall enough that nothing scrolls, so every row
        // of the table is on screen at a known x.
        let (w, h) = (56u16, 90u16);
        let mut buf = buffer(w, h);
        HelpOverlay {
            keymap: &keymap,
            mouse: true,
            scroll: 0,
        }
        .render(Rect::new(0, 0, w, h), &mut buf);
        let inner = crate::ui::draw::framed_inner(Rect::new(0, 0, w, h));
        let g = grid(&buf);
        let mut checked = 0;
        for (row, line) in g.lines().enumerate().skip((inner.y + HEADER_ROWS) as usize) {
            let cells: Vec<char> = line.chars().collect();
            let at = |col: usize| cells[inner.x as usize + col];
            // a bind row is the indented kind; headings start at col 0
            // of the column and gaps are blank.
            if at(0) != ' ' || at(2) == ' ' {
                continue;
            }
            for col in KEY_COLUMN + 2..DESC_COLUMN {
                assert_eq!(
                    at(col),
                    ' ',
                    "row {row} has no gutter before its description:\n{line}"
                );
            }
            assert_ne!(
                at(DESC_COLUMN),
                ' ',
                "row {row}'s description does not start in the column:\n{line}"
            );
            checked += 1;
        }
        let rows: usize = SECTIONS.iter().map(|(_, binds)| binds.len()).sum();
        assert_eq!(checked, rows, "not every bind row was on screen");
    }

    /// Two columns of the table never touch. At a width that held two
    /// minimum columns edge to edge, the widest row of the left one ran
    /// straight into the heading of the right one; the heading rule
    /// made it unmissable, which is how it was found.
    #[test]
    fn no_column_runs_into_the_one_beside_it() {
        let keymap = Keymap::default();
        // a heading's rule runs to its own column's edge, so whatever
        // follows one is the gutter; a second column's heading begins
        // with a letter, and the `▴`/`▾` cues never do.
        let mut multi_column_rows = 0;
        for w in 80..=200u16 {
            for h in [29u16, 40, 80] {
                let mut buf = buffer(w, h);
                HelpOverlay {
                    keymap: &keymap,
                    mouse: true,
                    scroll: 0,
                }
                .render(Rect::new(0, 0, w, h), &mut buf);
                let g = grid(&buf);
                let inner = crate::ui::draw::framed_inner(Rect::new(0, 0, w, h));
                let body = (inner.y + HEADER_ROWS) as usize;
                for line in g.lines().take((inner.y + inner.height) as usize).skip(body) {
                    let cells: Vec<char> = line.chars().collect();
                    let mut runs = 0;
                    for (i, c) in cells.iter().enumerate() {
                        if *c != '─' || cells.get(i + 1) == Some(&'─') {
                            continue;
                        }
                        runs += 1;
                        let after: String = cells[i + 1..].iter().take(COLUMN_GUTTER).collect();
                        assert!(
                            !after.chars().any(char::is_alphanumeric),
                            "{w}x{h}: a rule runs into the next column:\n{line}"
                        );
                    }
                    if runs > 1 {
                        multi_column_rows += 1;
                    }
                }
            }
        }
        // the sweep has to have drawn columns side by side, or it is
        // asserting about a layout it never produced: at a tall enough
        // pane first-fit keeps the whole table in column one.
        assert!(
            multi_column_rows > 0,
            "no size in the sweep put two columns on one row"
        );
    }

    /// Three levels down the row, which is the whole of the hierarchy
    /// on a monochrome surface: the heading brightest, then the key,
    /// then what it does, over a rule dark enough to read as structure.
    /// Every row at one level is what made the table a wall of text.
    #[test]
    fn a_row_paints_its_key_a_shade_above_its_description() {
        let keymap = Keymap::default();
        let (w, h) = (56u16, 90u16);
        let mut buf = buffer(w, h);
        HelpOverlay {
            keymap: &keymap,
            mouse: true,
            scroll: 0,
        }
        .render(Rect::new(0, 0, w, h), &mut buf);
        let inner = crate::ui::draw::framed_inner(Rect::new(0, 0, w, h));
        let heading = inner.y + HEADER_ROWS;
        let fg = |x: u16, y: u16| buf.cell((x, y)).unwrap().fg;
        // the first body row is "navigation", followed by its rule
        assert_eq!(fg(inner.x, heading), shade(NEAR_WHITE), "heading");
        assert_eq!(
            fg(inner.x + 12, heading),
            shade(NEAR_BLACK),
            "the rule must be darker than its own name"
        );
        // and the row under it: "  /" then "search"
        let row = heading + 1;
        assert_eq!(fg(inner.x + 2, row), shade(MID), "the key");
        assert_eq!(
            fg(inner.x + DESC_COLUMN as u16, row),
            shade(DIM),
            "the description is as bright as the key"
        );
    }

    /// A window onto a taller table says which way the rest of it is
    /// and how much of it there is. The count alone left "am I at the
    /// top" unanswered; the cues alone left "how much more" unanswered.
    #[test]
    fn a_scrolled_table_cues_both_edges_and_counts_what_is_left() {
        let keymap = Keymap::default();
        let (w, h) = (120u16, 24u16);
        let at = |scroll: usize| -> String {
            let mut buf = buffer(w, h);
            HelpOverlay {
                keymap: &keymap,
                mouse: true,
                scroll,
            }
            .render(Rect::new(0, 0, w, h), &mut buf);
            grid(&buf)
        };
        let max = HelpOverlay {
            keymap: &keymap,
            mouse: true,
            scroll: 0,
        }
        .max_scroll(Rect::new(0, 0, w, h));
        assert!(max > 2, "this size has to scroll for the test to mean much");

        let top = at(0);
        assert!(!top.contains('▴'), "nothing is above the first row:\n{top}");
        assert!(top.contains('▾'), "{top}");
        assert!(top.contains(&format!("{max} more below")), "{top}");

        let mid = at(1);
        assert!(mid.contains('▴') && mid.contains('▾'), "{mid}");
        assert!(mid.contains(&format!("{} more below", max - 1)), "{mid}");

        let end = at(max);
        assert!(end.contains('▴'), "{end}");
        assert!(!end.contains('▾'), "that is the last row:\n{end}");
        assert!(end.contains("end of the table"), "{end}");
        // and a table that fits says none of it, losing no row to a
        // hint it does not need.
        let fits = {
            let mut buf = buffer(120, 80);
            HelpOverlay {
                keymap: &keymap,
                mouse: true,
                scroll: 0,
            }
            .render(Rect::new(0, 0, 120, 80), &mut buf);
            grid(&buf)
        };
        assert!(
            !fits.contains('▴') && !fits.contains('▾') && !fits.contains("more below"),
            "{fits}"
        );
    }

    /// The footer is the one row that can be wider than the pane, and
    /// it used to stop mid-word wherever the budget ran out.
    #[test]
    fn a_footer_too_wide_for_the_pane_is_ellipsised_not_clipped() {
        let keymap = Keymap::default();
        for w in 8..=36u16 {
            let mut buf = buffer(w, 10);
            HelpOverlay {
                keymap: &keymap,
                mouse: true,
                scroll: 0,
            }
            .render(Rect::new(0, 0, w, 10), &mut buf);
            let g = grid(&buf);
            let Some(line) = g.lines().find(|l| l.contains("more")) else {
                continue;
            };
            // either the whole hint survived, naming the keys, or the
            // cut is marked; never a sentence that just stops.
            assert!(
                line.contains("j/k, wheel") || line.contains('…'),
                "{w} cols cut the footer with no sign of it:\n{line}"
            );
        }
    }

    /// A key can be rebound to a wide char, which owns two cells while
    /// `{:<13}` counted it as one; that pushed its own description a
    /// cell right of every other row's.
    #[test]
    fn a_wide_key_label_does_not_shift_its_own_description() {
        use crossterm::event::KeyCode;
        let mut keymap = Keymap::default();
        keymap.set(KeyAction::CycleEq, KeyCode::Char('\u{65e5}'));
        let mut buf = buffer(120, 40);
        HelpOverlay {
            keymap: &keymap,
            mouse: true,
            scroll: 0,
        }
        .render(Rect::new(0, 0, 120, 40), &mut buf);
        let g = grid(&buf);
        let eq = g.lines().find(|l| l.contains("cycle eq preset")).unwrap();
        let shuffle = g.lines().find(|l| l.contains("shuffle")).unwrap();
        // cell columns, not byte offsets: the wide key is three bytes
        // and two cells, so `find` alone would compare the wrong thing
        // (which is how this test first "failed" against a correct fix).
        let col = |line: &str, needle: &str| {
            let byte = line.find(needle).unwrap();
            line[..byte].chars().count()
        };
        assert_eq!(
            col(eq, "cycle eq preset"),
            col(shuffle, "shuffle"),
            "the wide key pushed its description out of the column:\n{eq}\n{shuffle}"
        );
    }
}

#[cfg(test)]
mod eyeball {
    use super::*;
    use crate::keymap::Keymap;
    use crate::ui::testbuf::{buffer, grid};

    /// Every cell's brightness as one digit of `shade`'s level, 0 to 9,
    /// blank where nothing was painted. The char grid cannot show
    /// hierarchy in a strictly monochrome ui, which is the half of
    /// this surface that needed looking at: the heading, the key and
    /// the description have to come out three different digits.
    pub(super) fn levels(buf: &Buffer) -> String {
        let area = buf.area;
        (area.y..area.y + area.height)
            .map(|y| {
                (area.x..area.x + area.width)
                    .map(|x| match buf.cell((x, y)).map(|c| c.fg) {
                        Some(ratatui::style::Color::Rgb(v, _, _)) => {
                            char::from_digit(u32::from(v) * 10 / 256, 10).unwrap_or('?')
                        }
                        _ => ' ',
                    })
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The reference at the sizes it is actually read at, including
    /// the scroll offsets, since the table is taller than a terminal
    /// and the console section was added to the end of it:
    /// `cargo test --lib ui::help::eyeball -- --nocapture`.
    /// Asserts the floor too, so it is a test and not just a printer.
    #[test]
    fn print_the_reference_at_every_width_it_is_read_at() {
        let keymap = Keymap::default();
        for (w, h) in [
            (120u16, 24u16),
            (80, 24),
            (160, 40),
            (100, 20),
            (50, 16),
            // cramped: one column, barely any rows.
            (30, 9),
            // and absurdly wide, where the column count caps out.
            (240, 30),
        ] {
            let scroll_max = HelpOverlay {
                keymap: &keymap,
                mouse: true,
                scroll: 0,
            }
            .max_scroll(Rect::new(0, 0, w, h));
            for scroll in [0, scroll_max / 2, scroll_max] {
                let mut buf = buffer(w, h);
                HelpOverlay {
                    keymap: &keymap,
                    mouse: true,
                    scroll,
                }
                .render(Rect::new(0, 0, w, h), &mut buf);
                let text = grid(&buf);
                println!("--- {w}x{h} scroll={scroll}/{scroll_max}\n{text}");
                assert!(
                    text.contains("keybinds"),
                    "{w}x{h} lost its own header:\n{text}"
                );
            }
        }
    }

    /// The console block as the user meets it, next to the key that
    /// opens it; the whole section was missing, so this is the one
    /// that wants looking at rather than asserting.
    #[test]
    fn print_the_console_section() {
        let keymap = Keymap::default();
        let area = Rect::new(0, 0, 50, 24);
        // the console block is at the end of a one-column table, so
        // it is only on screen at the bottom of the scroll.
        let scroll = HelpOverlay {
            keymap: &keymap,
            mouse: false,
            scroll: 0,
        }
        .max_scroll(area);
        let mut buf = buffer(50, 24);
        HelpOverlay {
            keymap: &keymap,
            mouse: false,
            scroll,
        }
        .render(area, &mut buf);
        let text = grid(&buf);
        println!("--- 50x24 scroll={scroll} (one column, mouse off)\n{text}");
        assert!(text.contains("open the command console"), "{text}");
    }

    /// The same table as a brightness map, which is the only way to
    /// read hierarchy off a monochrome surface: heading, key and
    /// description have to be three distinct levels, and the rule has
    /// to be darker than all three.
    #[test]
    fn print_the_brightness_of_a_section() {
        let keymap = Keymap::default();
        let (w, h) = (60u16, 14u16);
        let mut buf = buffer(w, h);
        HelpOverlay {
            keymap: &keymap,
            mouse: true,
            scroll: 0,
        }
        .render(Rect::new(0, 0, w, h), &mut buf);
        println!("--- {w}x{h}\n{}\n{}", grid(&buf), levels(&buf));
        let digits: std::collections::BTreeSet<char> =
            levels(&buf).lines().skip(4).flat_map(str::chars).collect();
        assert!(
            digits.len() >= 3,
            "the rows are all one brightness:\n{}",
            levels(&buf)
        );
    }

    /// Both overlay rows, and a key rebound to a wide glyph, at the
    /// sizes that have nowhere to put anything.
    #[test]
    fn print_the_degenerate_and_wide_glyph_cases() {
        use crossterm::event::KeyCode;
        let mut keymap = Keymap::default();
        keymap.set(KeyAction::CycleEq, KeyCode::Char('\u{65e5}'));
        keymap.set(KeyAction::ToggleStar, KeyCode::Char('\u{1f3b5}'));
        // the last two scroll the wide keys into view; they are in
        // playback and library, which a short pane starts above.
        for (w, h, scroll) in [
            (1u16, 1u16, 0usize),
            (2, 3, 0),
            (6, 4, 0),
            (14, 6, 0),
            (52, 20, 13),
            (100, 18, 10),
        ] {
            let mut buf = buffer(w, h);
            HelpOverlay {
                keymap: &keymap,
                mouse: true,
                scroll,
            }
            .render(Rect::new(0, 0, w, h), &mut buf);
            println!(
                "--- {w}x{h} scroll={scroll} (日 on cycle eq, 🎵 on star)\n{}",
                grid(&buf)
            );
        }
    }
}
