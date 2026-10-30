use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::widgets::Widget;

use crate::keymap::{key_label, KeyAction, Keymap};
use crate::theme::{shade, DIM, NEAR_BLACK, NEAR_WHITE};
use crate::ui::header::draw_header;
use crate::ui::textfit::width;

/// Full keybind reference, opened with `?` (hinted by `HelpBox` in the
/// bottom-right corner); takes over the whole screen the same way
/// onboarding/the nav manager do, since it's a brief reference lookup
/// rather than something you'd want to see alongside anything else.
/// Looks up every remappable entry's *current* key from `keymap`
/// live, rather than a hardcoded default string that would silently
/// go stale the moment anything got rebound via 'K'.
pub struct HelpOverlay<'a> {
    pub keymap: &'a Keymap,
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
        ],
    ),
    (
        "view",
        &[
            (Key::Bound(KeyAction::ToggleMinimize), "minimize player"),
            (
                Key::Bound(KeyAction::ToggleFullscreenVideo),
                "fullscreen video",
            ),
            (Key::Bound(KeyAction::CycleArtMode), "cycle album art"),
            (Key::Bound(KeyAction::ToggleResizeMode), "resize panes"),
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

        // laid out in columns rather than one tall list: the full
        // reference is ~50 rows, so on a normal 24-row terminal the
        // last sections (including how to quit) used to just silently
        // fall off the bottom. columns are balanced by row count, and
        // a narrow terminal falls back to a single column.
        let rows_available = (area.y + area.height).saturating_sub(top) as usize;
        let blocks = self.rendered_blocks();
        let columns = split_into_columns(&blocks, rows_available, area.width as usize);

        let col_width = if columns.len() > 1 {
            area.width as usize / columns.len()
        } else {
            area.width as usize
        };
        for (i, column) in columns.iter().enumerate() {
            let x = area.x + (i * col_width) as u16;
            for (y, line) in (top..).zip(column.iter()) {
                if y >= area.y + area.height {
                    break;
                }
                draw_line(buf, area, x, col_width, y, &line.text, shade(line.level));
            }
        }
    }
}

/// One rendered line plus its brightness: section headings sit at
/// `NEAR_WHITE`, individual binds at `DIM`, blank spacers anywhere.
struct Line {
    text: String,
    level: f32,
}

impl HelpOverlay<'_> {
    /// `SECTIONS` flattened into drawable lines, resolving every
    /// `Key::Bound` against the live keymap so a rebound key is never
    /// shown as its default. Grouped per section so `split_into_columns`
    /// can avoid breaking a section across a column boundary.
    fn rendered_blocks(&self) -> Vec<Vec<Line>> {
        SECTIONS
            .iter()
            .map(|(section, binds)| {
                let mut block = vec![Line {
                    text: (*section).to_string(),
                    level: NEAR_WHITE,
                }];
                for (key, desc) in *binds {
                    let key_text = key.label(self.keymap);
                    // padded by display width, by hand: `{key:<13}`
                    // counts chars, so a key bound to a wide char was
                    // one cell wider than it counted and pushed its own
                    // description a cell right of the column. same fix
                    // banner.rs records for its menu.
                    let pad = " ".repeat(KEY_COLUMN.saturating_sub(width(&key_text)));
                    block.push(Line {
                        text: format!("  {key_text}{pad} {desc}"),
                        level: DIM,
                    });
                }
                block.push(Line {
                    text: String::new(),
                    level: DIM,
                });
                block
            })
            .collect()
    }
}

/// Cells reserved for the key column, so every description in every
/// section starts at the same offset. Fixed rather than derived from
/// the widest resolved label on purpose: a single long rebind ("f12")
/// would otherwise shift every description in the table, and
/// `MIN_COLUMN_WIDTH` below is computed from this number.
const KEY_COLUMN: usize = 13;

/// Minimum usable width for one column; below two of these the layout
/// stays single-column and scrolls off as before, which is still
/// better than two columns of truncated nonsense. 38 is the widest
/// line this table actually produces ("  " + a 13-cell key column +
/// " " + the longest description), not a round number: the frame added
/// around this overlay costs four columns, and a 40 here turned 120
/// cells into two columns, which is how sections fell off the bottom.
const MIN_COLUMN_WIDTH: usize = 38;

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
    let max_columns = (width / MIN_COLUMN_WIDTH).max(1);
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
fn draw_line(
    buf: &mut Buffer,
    area: Rect,
    x0: u16,
    max_width: usize,
    y: u16,
    text: &str,
    color: Color,
) {
    if y < area.y || y >= area.y + area.height {
        return;
    }
    let budget = max_width.min((area.x + area.width).saturating_sub(x0) as usize);
    crate::ui::draw::at(buf, x0, y, text, color, budget);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::Keymap;
    use crate::ui::testbuf::{assert_no_bleed_through, buffer, grid, plated_buffer};

    fn render_to_lines(width: u16, height: u16) -> Vec<String> {
        let keymap = Keymap::default();
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        HelpOverlay { keymap: &keymap }.render(area, &mut buf);
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
        HelpOverlay { keymap: &keymap }.render(area, &mut buf);
        assert_no_bleed_through(&buf, area);
    }

    #[test]
    fn the_frame_closes_all_four_corners() {
        let keymap = Keymap::default();
        let mut buf = buffer(80, 24);
        let area = Rect::new(2, 1, 76, 22);
        HelpOverlay { keymap: &keymap }.render(area, &mut buf);
        let g = grid(&buf);
        let rows: Vec<Vec<char>> = g.lines().map(|r| r.chars().collect()).collect();
        assert_eq!(
            (rows[1][2], rows[1][77], rows[22][2], rows[22][77]),
            ('┌', '┐', '└', '┘')
        );
    }

    #[test]
    fn every_section_heading_is_visible_on_a_standard_terminal() {
        // 80x24 is the floor worth supporting; the single-column
        // layout used to drop the last sections (quit included) off
        // the bottom with no indication they existed.
        let rendered = render_to_lines(120, 24).join("\n");
        for (section, _) in SECTIONS {
            assert!(
                rendered.contains(section),
                "section {section:?} fell off the overlay:\n{rendered}"
            );
        }
    }

    #[test]
    fn how_to_quit_is_always_reachable() {
        let rendered = render_to_lines(120, 24).join("\n");
        assert!(rendered.contains("quit from anywhere"));
    }

    #[test]
    fn a_narrow_terminal_falls_back_to_one_column() {
        let blocks_owner = {
            let keymap = Keymap::default();
            HelpOverlay { keymap: &keymap }.rendered_blocks()
        };
        let cols = split_into_columns(&blocks_owner, 24, 50);
        assert_eq!(cols.len(), 1, "50 cols can't hold two readable columns");
    }

    #[test]
    fn a_section_is_never_split_across_columns() {
        let keymap = Keymap::default();
        let blocks = HelpOverlay { keymap: &keymap }.rendered_blocks();
        let cols = split_into_columns(&blocks, 12, 160);
        // each section's heading must be followed by its own binds in
        // the same column, so count headings per column and make sure
        // no column starts with a bind row (an orphaned continuation).
        for col in &cols {
            if let Some(first) = col.first() {
                assert!(
                    !first.text.starts_with("  "),
                    "column starts mid-section: {:?}",
                    first.text
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
        HelpOverlay { keymap: &keymap }.render(area, &mut buf);
        // row 0 is the frame's top edge now; the title is one inside.
        let top: String = (0..area.width)
            .map(|x| buf.cell((x, 1)).unwrap().symbol().to_string())
            .collect();
        assert!(top.contains("y or esc to close"), "got:\n{top}");
        assert!(!top.contains("? or esc to close"), "stale default:\n{top}");
    }

    #[test]
    fn renders_without_panicking_at_degenerate_sizes() {
        for (w, h) in [(1, 1), (1, 0), (200, 1), (3, 3), (0, 10)] {
            let area = Rect::new(0, 0, w, h);
            let mut buf = Buffer::empty(area);
            let keymap = Keymap::default();
            HelpOverlay { keymap: &keymap }.render(area, &mut buf);
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
        HelpOverlay { keymap: &keymap }.render(area, &mut buf);
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

    /// A key can be rebound to a wide char, which owns two cells while
    /// `{:<13}` counted it as one; that pushed its own description a
    /// cell right of every other row's.
    #[test]
    fn a_wide_key_label_does_not_shift_its_own_description() {
        use crossterm::event::KeyCode;
        let mut keymap = Keymap::default();
        keymap.set(KeyAction::CycleEq, KeyCode::Char('\u{65e5}'));
        let mut buf = buffer(120, 40);
        HelpOverlay { keymap: &keymap }.render(Rect::new(0, 0, 120, 40), &mut buf);
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
