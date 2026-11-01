use std::cell::RefCell;
use std::sync::LazyLock;
use std::time::Duration;

use crossterm::event::KeyCode;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::anim;
use crate::keymap::{key_label, KeyAction, Keymap};
use crate::theme::{lerp, shade, BRIGHT, DIM, NEAR_WHITE, WHITE};
use crate::ui::logos::LOGOS;
use crate::ui::textfit::width;

/// `figlet -f "ANSI Shadow" WIM`; the same font LazyVim's own
/// snacks.nvim dashboard header uses (confirmed against LazyVim's
/// actual `lua/lazyvim/plugins/ui.lua` source, not just recalled from
/// memory), baked in as static art rather than a runtime figlet call.
/// No figlet dependency at runtime; "WIM" never changes. This is
/// specifically index 0 of the logo picker (`<`/`>` while the
/// workspace has focus, see `logo_lines`/`logo_name`); kept as its
/// own const rather than folded into `logos::LOGOS` since it's the
/// permanent default a 'reset' always returns to, not just another
/// item in that generated list.
const LOGO: &[&str] = &[
    r"██╗    ██╗██╗███╗   ███╗",
    r"██║    ██║██║████╗ ████║",
    r"██║ █╗ ██║██║██╔████╔██║",
    r"██║███╗██║██║██║╚██╔╝██║",
    r"╚███╔███╔╝██║██║ ╚═╝ ██║",
    r" ╚══╝╚══╝ ╚═╝╚═╝     ╚═╝",
];

/// Codepoints figlet/toilet line art is built from: ascii, box
/// drawing, block elements, braille. Every one is a single cell and
/// none has an emoji presentation, which is what the logo grid below
/// assumes when it paints one char per cell in one shade.
fn is_line_art(ch: char) -> bool {
    ch.is_ascii() || matches!(ch as u32, 0x2500..=0x257F | 0x2580..=0x259F | 0x2800..=0x28FF)
}

/// The alternatives `<`/`>` can reach. Two of `LOGOS`' 178 entries are
/// not line art and are dropped here: `toilet: circle` draws circled
/// capitals, and U+24C2 Ⓜ has an emoji presentation, so an emoji font
/// paints it coloured and often double width against a monochrome ui
/// (exactly the hazard `controlsbar`'s glyph alphabet exists for, and
/// one `unicode-width` calls a single cell, so no width assertion
/// catches it); `toilet: wideterm` draws fullwidth forms, two cells
/// each, which the one-cell-per-char grid overlaps into each other.
/// Filtered rather than deleted from the generated table, which says
/// not to hand-edit it and would bring both back on the next
/// regeneration.
static ALTERNATIVES: LazyLock<Vec<&'static (&'static str, &'static [&'static str])>> =
    LazyLock::new(|| {
        LOGOS
            .iter()
            .filter(|(_, lines)| lines.iter().all(|l| l.chars().all(is_line_art)))
            .collect()
    });

/// 1 (the default) plus every alternative; `logo_index` cycles
/// `0..logo_count()`.
pub fn logo_count() -> usize {
    1 + ALTERNATIVES.len()
}

pub fn logo_lines(index: usize) -> &'static [&'static str] {
    if index == 0 {
        LOGO
    } else {
        ALTERNATIVES[index - 1].1
    }
}

/// `None` for the default (nothing to save, config just omits it);
/// `Some(name)` otherwise, the stable key persisted to config.toml
/// instead of the index itself (which would shift if this generated
/// list is ever regenerated with different fonts in a different
/// order).
pub fn logo_name(index: usize) -> Option<&'static str> {
    (index != 0).then(|| ALTERNATIVES[index - 1].0)
}

pub fn logo_index_for_name(name: Option<&str>) -> usize {
    match name {
        None => 0,
        Some(name) => ALTERNATIVES
            .iter()
            .position(|(n, _)| *n == name)
            .map(|i| i + 1)
            .unwrap_or(0),
    }
}

/// Action, description; same `snacks.dashboard.Item` idea LazyVim's own
/// dashboard menu uses below its header, minus the nerd-font icons
/// (same monochrome/plain-ascii reasoning as the result-row `[sk]`
/// codes: a patched-font glyph the user's terminal doesn't have would
/// just show as a broken box, not a silent icon). Actions rather than
/// key literals: every one of these is remappable, so the key column
/// is resolved from the live keymap at render time.
const MENU: &[(KeyAction, &str)] = &[
    (KeyAction::StartSearch, "search"),
    (KeyAction::OpenNavManager, "navidrome profiles"),
    (KeyAction::OpenStarredView, "starred songs"),
    (KeyAction::IndexBreakdown, "index breakdown"),
    (KeyAction::OpenSettingsMenu, "settings"),
    (KeyAction::OpenHelp, "help"),
    (KeyAction::Quit, "quit"),
];

/// Everything a banner frame derives from (logo index, keymap), and
/// nothing that moves with the clock.
///
/// The menu half: LazyVim's own menu is a left-aligned column (key,
/// two spaces, description) centred as one block; centring each row by
/// its own length is what produced the staircase this fixed, since
/// "n  navidrome profiles" and "q  quit" do not share a length.
struct Layout {
    /// What it was built from: the logo on screen and every `MENU`
    /// action's resolved key. A rebind or a `<`/`>` press rebuilds it
    /// and a frame reuses it.
    logo: usize,
    keys: [KeyCode; MENU.len()],
    /// Cells of the logo's widest line, which the art block and both
    /// arrows are centred off. Kept rather than tabled for every logo:
    /// measuring all 177 up front costs 185-220us on a first frame to
    /// save the 1.2-2.3us only the one on screen is worth.
    logo_width: u16,
    rows: Vec<String>,
    /// Cells of the widest menu row. Display width, not char count: a
    /// key bound to a wide char makes its row two cells wider than it
    /// counts, which would centre the block left by that overhang.
    menu_width: u16,
}

impl Layout {
    fn build(logo: usize, keys: [KeyCode; MENU.len()]) -> Self {
        #[cfg(test)]
        REBUILDS.with(|n| n.set(n.get() + 1));
        let logo_width = logo_lines(logo).iter().map(|l| width(l)).max().unwrap_or(0) as u16;
        let labels: Vec<String> = keys.iter().copied().map(key_label).collect();
        // padded by display width, by hand: `{key:<n$}` counts chars
        // and `key.len()` counts bytes, and a key bound to a wide or
        // multi-byte char is neither, which left the description
        // column jagged for exactly the reason f5 once did. the width
        // comes from the resolved labels, since a rebound key ("f5")
        // is a different length than the default it replaced.
        let key_width = labels.iter().map(|k| width(k)).max().unwrap_or(0);
        let rows: Vec<String> = labels
            .iter()
            .zip(MENU)
            .map(|(key, (_, desc))| {
                let pad = key_width - width(key);
                let mut row = String::with_capacity(key.len() + pad + 2 + desc.len());
                row.push_str(key);
                for _ in 0..pad {
                    row.push(' ');
                }
                row.push_str("  ");
                row.push_str(desc);
                row
            })
            .collect();
        let menu_width = rows.iter().map(|r| width(r)).max().unwrap_or(0) as u16;
        Layout {
            logo,
            keys,
            logo_width,
            rows,
            menu_width,
        }
    }
}

thread_local! {
    /// The derived half of a banner frame, kept across frames. ratatui
    /// draws on the thread that owns the terminal, so a thread-local
    /// is exactly as shared as the draw call is; the call
    /// `ui::raster::with_scratch` makes, for the same reason. One slot,
    /// because one banner is on screen at a time.
    static LAYOUT: RefCell<Option<Layout>> = const { RefCell::new(None) };
    /// How many times `Layout::build` has run on this thread, so a
    /// test can pin that a second frame does not rebuild it. Per
    /// thread because the cache is, and every `#[test]` gets its own.
    #[cfg(test)]
    static REBUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Runs `f` with this thread's layout for `logo` and `keymap`, rebuilt
/// only when the logo changed or one of the seven keys it shows moved.
fn with_layout<R>(logo: usize, keymap: &Keymap, f: impl FnOnce(&Layout) -> R) -> R {
    let mut keys = [KeyCode::Null; MENU.len()];
    for (slot, (action, _)) in keys.iter_mut().zip(MENU) {
        *slot = keymap.get(*action);
    }
    LAYOUT.with(|cache| {
        let mut cache = cache.borrow_mut();
        if !matches!(&*cache, Some(kept) if kept.logo == logo && kept.keys == keys) {
            *cache = Some(Layout::build(logo, keys));
        }
        f(cache.as_ref().expect("built just above"))
    })
}

/// Logo with a brightness sweep plus a slower overall pulse layered on
/// top (two independent motions read as more "alive" than a single
/// linear sweep) and a key/description menu below it (LazyVim's own
/// dashboard structure). No item count here: it used to sit as a
/// permanent footer line, which read as the app announcing a number
/// nobody asked for every time this screen was on; 'I' surfaces it on
/// demand instead. Both motions come off `elapsed` (time since app
/// start) via `anim`, so they run at the same speed on any display.
/// `logo_index` selects which of `logo_lines`' entries to draw.
pub struct Banner<'a> {
    pub elapsed: Duration,
    pub logo_index: usize,
    pub keymap: &'a Keymap,
}

impl Widget for Banner<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // everything here but the two motions is derived from the logo
        // index and the keymap, and is kept across frames (`Layout`).
        with_layout(self.logo_index, self.keymap, |layout| {
            self.paint(area, buf, layout);
        });
    }
}

impl Banner<'_> {
    fn paint(&self, area: Rect, buf: &mut Buffer, layout: &Layout) {
        let lines = logo_lines(self.logo_index);
        let glyph_height = lines.len() as u16;
        // display width, like every other measurement on this screen,
        // measured once per logo rather than once per frame;
        // `is_line_art` keeps the two measures equal for the art the
        // picker can reach, and this does not depend on that holding.
        let total_width = layout.logo_width;
        let start_x = area.x + area.width.saturating_sub(total_width) / 2;

        // logo, one gap row, the menu.
        let frame_height = glyph_height + 2 + MENU.len() as u16;
        let logo_start_y = area.y + area.height.saturating_sub(frame_height) / 2;

        let phase = anim::phase(self.elapsed, anim::BANNER_SWEEP_PERIOD);
        let pulse = anim::pulse(self.elapsed, anim::BANNER_PULSE_PERIOD);

        // a blank column of breathing room on each side; sitting
        // right up against the logo's own edge read as touching it.
        const ARROW_GAP: u16 = 2;
        // the row is the middle of a logo block centred from a frame
        // taller than the pane, so it can land below it; both guards
        // tested x only and the arrows punched holes in the bottom
        // bar's rule. same check the logo grid below already has.
        let arrow_y = logo_start_y + glyph_height / 2;
        if arrow_y < area.y + area.height {
            if start_x >= area.x + ARROW_GAP {
                put(buf, start_x - ARROW_GAP, arrow_y, '<', shade(NEAR_WHITE));
            }
            let right_x = start_x + total_width + ARROW_GAP - 1;
            if right_x < area.x + area.width {
                put(buf, right_x, arrow_y, '>', shade(NEAR_WHITE));
            }
        }

        for (row_idx, row) in lines.iter().enumerate() {
            let y = logo_start_y + row_idx as u16;
            if y >= area.y + area.height {
                continue;
            }
            for (col_idx, ch) in row.chars().enumerate() {
                if ch == ' ' {
                    continue;
                }
                let x = start_x + col_idx as u16;
                if x >= area.x + area.width {
                    continue;
                }
                let rel = (x - start_x) as f32 / total_width.max(1) as f32;
                let sweep = ((rel - phase).rem_euclid(1.0) - 0.5).abs() * 2.0;
                // all math stays in plain 0..1 levels until the one
                // conversion to a Color at the end.
                let sweep_level = lerp(WHITE, BRIGHT, sweep);
                let level = (sweep_level * (0.8 + 0.2 * pulse)).clamp(0.0, 1.0);
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.set_char(ch);
                    cell.set_fg(shade(level));
                }
            }
        }

        let menu_start_y = logo_start_y + glyph_height + 2;
        let block_start_x = area.x + area.width.saturating_sub(layout.menu_width) / 2;
        for (i, row) in layout.rows.iter().enumerate() {
            draw_left(
                buf,
                area,
                block_start_x,
                menu_start_y + i as u16,
                row,
                shade(DIM),
            );
        }
    }
}

fn put(buf: &mut Buffer, x: u16, y: u16, ch: char, color: ratatui::style::Color) {
    crate::ui::draw::put(buf, x, y, ch, color);
}

/// Menu rows only. The logo grid above keeps its own loop on purpose:
/// it is figlet art, one cell per char, which `is_line_art` enforces
/// rather than assumes.
fn draw_left(
    buf: &mut Buffer,
    area: Rect,
    start_x: u16,
    y: u16,
    text: &str,
    color: ratatui::style::Color,
) {
    if y < area.y || y >= area.y + area.height {
        return;
    }
    let right = area.x + area.width;
    crate::ui::draw::at(
        buf,
        start_x,
        y,
        text,
        color,
        right.saturating_sub(start_x) as usize,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render_to_string(keymap: &Keymap) -> String {
        let area = Rect::new(0, 0, 60, 24);
        let mut buf = Buffer::empty(area);
        Banner {
            elapsed: Duration::ZERO,
            logo_index: 0,
            keymap,
        }
        .render(area, &mut buf);
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf.cell((x, y)).unwrap().symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The grid paints one char per cell in one shade, so every logo
    /// the picker can reach has to be art that survives that: one cell
    /// per char, and no codepoint an emoji font would take over.
    #[test]
    fn every_logo_the_picker_can_reach_is_one_cell_per_char_line_art() {
        for index in 0..logo_count() {
            let name = logo_name(index).unwrap_or("default");
            for line in logo_lines(index) {
                assert_eq!(
                    width(line),
                    line.chars().count(),
                    "{name:?} has a wide glyph in {line:?}"
                );
                for ch in line.chars() {
                    assert!(is_line_art(ch), "{name:?} draws U+{:04X}", ch as u32);
                }
            }
        }
    }

    /// The test with teeth for the emoji half of that: U+24C2 Ⓜ is one
    /// cell by `unicode-width` and an emoji font still paints it
    /// coloured, so only naming the codepoints that are allowed catches
    /// it. Both entries stay in the generated table on purpose; what is
    /// asserted is that the picker cannot land on them.
    #[test]
    fn the_two_entries_that_are_not_line_art_are_unreachable() {
        for name in ["toilet: circle", "toilet: wideterm"] {
            assert!(
                LOGOS.iter().any(|(n, _)| *n == name),
                "{name:?} is gone from the generated table, so this test is stale"
            );
            assert!(
                !(0..logo_count()).any(|i| logo_name(i) == Some(name)),
                "{name:?} is reachable from the picker"
            );
            // a config still naming one falls back to the default
            // rather than to whichever logo took its index.
            assert_eq!(logo_index_for_name(Some(name)), 0);
        }
        // and nothing else was dropped along with them.
        assert_eq!(logo_count(), LOGOS.len() - 1);
    }

    /// A name persisted in config.toml has to come back as the same
    /// logo, which is why the index is not what is stored.
    #[test]
    fn every_reachable_name_round_trips_through_the_index() {
        for index in 1..logo_count() {
            let name = logo_name(index).expect("every alternative is named");
            assert_eq!(logo_index_for_name(Some(name)), index, "{name:?}");
        }
        assert_eq!(logo_index_for_name(None), 0);
        assert_eq!(logo_name(0), None);
    }

    #[test]
    fn the_menu_shows_rebound_keys_not_the_defaults() {
        let mut keymap = Keymap::default();
        keymap.set(KeyAction::StartSearch, KeyCode::Char('x'));
        keymap.set(KeyAction::OpenHelp, KeyCode::Char('y'));
        let out = render_to_string(&keymap);
        assert!(out.contains("x  search"), "got:\n{out}");
        assert!(out.contains("y  help"), "got:\n{out}");
        assert!(!out.contains("/  search"), "stale default key:\n{out}");
        assert!(!out.contains("?  help"), "stale default key:\n{out}");
    }

    #[test]
    fn the_key_column_is_padded_from_the_resolved_labels() {
        // "f5" is wider than every default, so every other row has to
        // widen with it or the description column goes jagged again.
        let mut keymap = Keymap::default();
        keymap.set(KeyAction::Quit, KeyCode::F(5));
        let out = render_to_string(&keymap);
        assert!(out.contains("f5  quit"), "got:\n{out}");
        assert!(out.contains("/   search"), "got:\n{out}");
    }

    use crate::ui::testbuf::{assert_outside_untouched, buffer, grid};

    fn render(area: Rect, buf: &mut Buffer, keymap: &Keymap) {
        Banner {
            elapsed: Duration::ZERO,
            logo_index: 0,
            keymap,
        }
        .render(area, buf);
    }

    /// Whole-screen snapshot, logo included: the figlet grid keeps its
    /// own one-cell-per-char loop on purpose, so this also pins that
    /// nothing about it moved.
    #[test]
    fn the_default_banner_renders_logo_arrows_and_menu() {
        let mut buf = buffer(44, 18);
        render(Rect::new(0, 0, 44, 18), &mut buf, &Keymap::default());
        assert_eq!(
            grid(&buf),
            [
                "............................................",
                "..........██╗....██╗██╗███╗...███╗..........",
                "..........██║....██║██║████╗.████║..........",
                "..........██║.█╗.██║██║██╔████╔██║..........",
                "........<.██║███╗██║██║██║╚██╔╝██║.>........",
                "..........╚███╔███╔╝██║██║.╚═╝.██║..........",
                "...........╚══╝╚══╝.╚═╝╚═╝.....╚═╝..........",
                "............................................",
                "............................................",
                ".........../  search........................",
                "...........n  navidrome profiles............",
                "...........S  starred songs.................",
                "...........I  index breakdown...............",
                "...........u  settings......................",
                "...........?  help..........................",
                "...........q  quit..........................",
                "............................................",
                "............................................",
            ]
            .join("\n")
        );
    }

    /// The menu block is centred and left-aligned as one block, with
    /// every row starting at the same column.
    #[test]
    fn the_menu_block_is_one_left_aligned_column() {
        let mut buf = buffer(60, 24);
        render(Rect::new(0, 0, 60, 24), &mut buf, &Keymap::default());
        let g = grid(&buf);
        let rows: Vec<&str> = g
            .lines()
            .filter(|r| MENU.iter().any(|(_, desc)| r.contains(desc)))
            .collect();
        assert_eq!(rows.len(), MENU.len(), "one row per menu item:\n{g}");
        let first = rows[0].find(|c: char| c != '.').unwrap();
        for row in &rows {
            assert_eq!(
                row.find(|c: char| c != '.'),
                Some(first),
                "menu rows are not aligned:\n{g}"
            );
        }
    }

    /// A key bound to a wide char is two cells; its row has to advance
    /// by two, and the block has to stay centred by cells.
    #[test]
    fn a_wide_key_label_does_not_shift_its_own_description() {
        let mut keymap = Keymap::default();
        keymap.set(KeyAction::StartSearch, KeyCode::Char('日'));
        let mut buf = buffer(60, 24);
        render(Rect::new(0, 0, 60, 24), &mut buf, &keymap);
        let g = grid(&buf);
        let row = g.lines().find(|r| r.contains("search")).unwrap();
        let col = row.find('日').expect("the wide key");
        // the wide glyph owns the cell after it, then the two-space
        // gutter, then the description.
        assert_eq!(row.chars().nth(col + 1), Some(' '), "{row}");
        assert_eq!(
            &row[row.char_indices().nth(col + 2).unwrap().0..][..8],
            "  search"
        );
    }

    #[test]
    fn the_menu_stays_inside_the_rect() {
        let mut buf = buffer(60, 30);
        let area = Rect::new(4, 2, 50, 26);
        let mut keymap = Keymap::default();
        keymap.set(KeyAction::StartSearch, KeyCode::Char('日'));
        render(area, &mut buf, &keymap);
        assert_outside_untouched(&buf, area);
    }

    /// The render as it was before the (logo index, keymap) half of it
    /// was kept: every frame re-walked the art's widths and rebuilt
    /// the menu. Both the reference
    /// `the_cached_derivation_paints_exactly_what_rebuilding_it_did`
    /// holds the cache against and the other side of `frame_budget`'s
    /// comparison, so it stays a function rather than a branch in a
    /// timing loop.
    #[inline(never)]
    pub(super) fn rebuilding_render(
        elapsed: Duration,
        logo_index: usize,
        keymap: &Keymap,
        area: Rect,
        buf: &mut Buffer,
    ) {
        let lines = logo_lines(logo_index);
        let glyph_height = lines.len() as u16;
        let total_width = lines.iter().map(|l| width(l)).max().unwrap_or(0) as u16;
        let start_x = area.x + area.width.saturating_sub(total_width) / 2;
        let frame_height = glyph_height + 2 + MENU.len() as u16;
        let logo_start_y = area.y + area.height.saturating_sub(frame_height) / 2;
        let phase = anim::phase(elapsed, anim::BANNER_SWEEP_PERIOD);
        let pulse = anim::pulse(elapsed, anim::BANNER_PULSE_PERIOD);
        const ARROW_GAP: u16 = 2;
        let arrow_y = logo_start_y + glyph_height / 2;
        if arrow_y < area.y + area.height {
            if start_x >= area.x + ARROW_GAP {
                put(buf, start_x - ARROW_GAP, arrow_y, '<', shade(NEAR_WHITE));
            }
            let right_x = start_x + total_width + ARROW_GAP - 1;
            if right_x < area.x + area.width {
                put(buf, right_x, arrow_y, '>', shade(NEAR_WHITE));
            }
        }
        for (row_idx, row) in lines.iter().enumerate() {
            let y = logo_start_y + row_idx as u16;
            if y >= area.y + area.height {
                continue;
            }
            for (col_idx, ch) in row.chars().enumerate() {
                if ch == ' ' {
                    continue;
                }
                let x = start_x + col_idx as u16;
                if x >= area.x + area.width {
                    continue;
                }
                let rel = (x - start_x) as f32 / total_width.max(1) as f32;
                let sweep = ((rel - phase).rem_euclid(1.0) - 0.5).abs() * 2.0;
                let sweep_level = lerp(WHITE, BRIGHT, sweep);
                let level = (sweep_level * (0.8 + 0.2 * pulse)).clamp(0.0, 1.0);
                if let Some(cell) = buf.cell_mut((x, y)) {
                    cell.set_char(ch);
                    cell.set_fg(shade(level));
                }
            }
        }
        let menu_start_y = logo_start_y + glyph_height + 2;
        let (rows, block_width) = rebuilt_menu(keymap);
        let block_start_x = area.x + area.width.saturating_sub(block_width) / 2;
        for (i, row) in rows.iter().enumerate() {
            draw_left(
                buf,
                area,
                block_start_x,
                menu_start_y + i as u16,
                row,
                shade(DIM),
            );
        }
    }

    /// The menu rows and their block width as a frame used to derive
    /// them: a `Vec<(String, &str)>`, a `" ".repeat()` and a
    /// `format!` per row, then a second `Vec` and `width()` over that.
    pub(super) fn rebuilt_menu(keymap: &Keymap) -> (Vec<String>, u16) {
        let menu: Vec<(String, &str)> = MENU
            .iter()
            .map(|(action, desc)| (key_label(keymap.get(*action)), *desc))
            .collect();
        let key_width = menu.iter().map(|(key, _)| width(key)).max().unwrap_or(0);
        let rows: Vec<String> = menu
            .iter()
            .map(|(key, desc)| {
                let pad = " ".repeat(key_width - width(key));
                format!("{key}{pad}  {desc}")
            })
            .collect();
        let block_width = rows.iter().map(|l| width(l)).max().unwrap_or(0) as u16;
        (rows, block_width)
    }

    /// A rebind is remappable text, so the keymap has to be in the
    /// cache key; these two move both the key column's width and one
    /// row's own width.
    fn rebound() -> Keymap {
        let mut keymap = Keymap::default();
        keymap.set(KeyAction::StartSearch, KeyCode::Char('日'));
        keymap.set(KeyAction::Quit, KeyCode::F(5));
        keymap
    }

    /// Pure performance, so not one cell may move: the kept
    /// derivation has to paint what rebuilding it per frame painted,
    /// styles included (`Buffer`'s own `PartialEq`, with a non-zero
    /// `elapsed` so the sweep gives every column its own shade). Every
    /// logo the picker can reach, at the sizes `main_content` gets
    /// including two too cramped for the art.
    #[test]
    fn the_cached_derivation_paints_exactly_what_rebuilding_it_did() {
        let elapsed = Duration::from_millis(1234);
        let rebound = rebound();
        // the keymap outermost on purpose: alternating it per logo
        // re-keyed the slot on every single iteration, which hid
        // whether the logo was part of that key at all.
        for keymap in [&Keymap::default(), &rebound] {
            for logo in 0..logo_count() {
                for (w, h) in [(44u16, 18u16), (90, 30), (60, 3), (12, 8), (26, 4)] {
                    let area = Rect::new(0, 0, w, h);
                    let mut cached = buffer(w, h);
                    let mut rebuilt = buffer(w, h);
                    // twice: the second is the frame that hit the
                    // slot, which is the one the cache can get wrong,
                    // and the paint is idempotent either way.
                    for _ in 0..2 {
                        Banner {
                            elapsed,
                            logo_index: logo,
                            keymap,
                        }
                        .render(area, &mut cached);
                    }
                    rebuilding_render(elapsed, logo, keymap, area, &mut rebuilt);
                    assert!(
                        cached == rebuilt,
                        "{:?} at {w}x{h}:\ncached:\n{}\nrebuilt:\n{}",
                        logo_name(logo).unwrap_or("default"),
                        grid(&cached),
                        grid(&rebuilt)
                    );
                }
            }
        }
    }

    /// The kept width stands in for a walk of the art, so it has to be
    /// that walk's answer for every logo, not just the default.
    #[test]
    fn the_kept_logo_width_is_what_the_art_measures() {
        let keymap = Keymap::default();
        for index in 0..logo_count() {
            let walked = logo_lines(index)
                .iter()
                .map(|l| width(l))
                .max()
                .unwrap_or(0);
            assert_eq!(
                with_layout(index, &keymap, |l| usize::from(l.logo_width)),
                walked,
                "{:?}",
                logo_name(index).unwrap_or("default")
            );
        }
    }

    fn rebuilds() -> usize {
        REBUILDS.with(|n| n.get())
    }

    /// The invariant the cache is: rebuilt when one of its two inputs
    /// moves, and not otherwise. The rebind and the logo cycle are the
    /// positive controls, so "it did not rebuild" cannot pass by never
    /// rebuilding at all.
    #[test]
    fn the_layout_is_rebuilt_only_when_the_logo_or_a_key_it_shows_moves() {
        let mut keymap = Keymap::default();
        let before = rebuilds();
        render_to_string(&keymap);
        assert_eq!(rebuilds(), before + 1, "the first frame builds it");
        render_to_string(&keymap);
        render_to_string(&keymap);
        assert_eq!(rebuilds(), before + 1, "later frames reuse it");

        keymap.set(KeyAction::Quit, KeyCode::F(5));
        render_to_string(&keymap);
        assert_eq!(rebuilds(), before + 2, "a rebind rebuilds it");
        render_to_string(&keymap);
        assert_eq!(rebuilds(), before + 2, "and then it is kept again");

        // an action the menu does not list is not one of its inputs.
        keymap.set(KeyAction::Enqueue, KeyCode::Char('Z'));
        render_to_string(&keymap);
        assert_eq!(rebuilds(), before + 2, "an unlisted action is not an input");

        // and the other input: `<`/`>` moves the logo, whose width the
        // same slot holds.
        let mut buf = buffer(60, 24);
        let doh = logo_index_for_name(Some("figlet: doh"));
        for _ in 0..2 {
            Banner {
                elapsed: Duration::ZERO,
                logo_index: doh,
                keymap: &keymap,
            }
            .render(Rect::new(0, 0, 60, 24), &mut buf);
        }
        assert_eq!(rebuilds(), before + 3, "a logo cycle rebuilds it once");
    }

    /// And what it rebuilds is what the per-frame derivation produced,
    /// asserted on the rows themselves rather than only through the
    /// buffer they are painted into.
    #[test]
    fn the_kept_rows_are_the_rows_the_old_derivation_built() {
        for keymap in [Keymap::default(), rebound()] {
            let (rows, block_width) = rebuilt_menu(&keymap);
            with_layout(0, &keymap, |layout| {
                assert_eq!(layout.rows, rows);
                assert_eq!(layout.menu_width, block_width);
            });
        }
    }

    #[test]
    fn degenerate_sizes_do_not_panic() {
        for (w, h) in [(1, 1), (1, 40), (2, 1), (40, 0), (0, 0), (3, 2), (26, 4)] {
            let mut buf = buffer(w.max(1), h.max(1));
            render(Rect::new(0, 0, w, h), &mut buf, &Keymap::default());
        }
    }

    /// The whole banner, arrows included, at the heights `main_content`
    /// actually gets on a short terminal; the rows under the pane are
    /// the bottom bar's, whose rule is drawn before this and never
    /// repainted.
    #[test]
    fn nothing_including_the_arrows_leaves_the_rect() {
        for logo in [0, logo_index_for_name(Some("figlet: doh"))] {
            for h in [0u16, 1, 2, 3, 4, 5, 6, 7, 25] {
                let area = Rect::new(2, 1, 60, h);
                let mut buf = buffer(66, 30);
                Banner {
                    elapsed: Duration::ZERO,
                    logo_index: logo,
                    keymap: &Keymap::default(),
                }
                .render(area, &mut buf);
                assert_outside_untouched(&buf, area);
            }
        }
    }
}

#[cfg(test)]
mod eyeball {
    use super::*;
    use crate::ui::testbuf::{buffer, grid};

    fn print(label: &str, logo_index: usize, w: u16, h: u16) {
        // three rows taller than the rect: those rows belong to the
        // bottom bar, which is where stray paint shows up.
        let mut buf = buffer(w, h + 3);
        Banner {
            elapsed: Duration::ZERO,
            logo_index,
            keymap: &Keymap::default(),
        }
        .render(Rect::new(0, 0, w, h), &mut buf);
        println!(
            "--- {label}: rect {w}x{h} in a {w}x{} buffer\n{}",
            h + 3,
            grid(&buf)
        );
    }

    /// A 60x7 terminal with the default 4-row bottom bar leaves the
    /// banner three rows; `figlet: doh` is 50 rows of art, so a 29-row
    /// terminal centres its block far below the pane:
    /// `cargo test --lib ui::banner::eyeball -- --nocapture`.
    /// Asserts the floor too, so it is a test and not just a printer.
    #[test]
    fn print_the_banner_at_the_squeezed_heights() {
        print("default logo", 0, 60, 3);
        print("default logo", 0, 60, 4);
        let doh = logo_index_for_name(Some("figlet: doh"));
        assert_ne!(doh, 0, "figlet: doh is gone, so this case is stale");
        print("figlet: doh", doh, 90, 25);

        let mut buf = buffer(44, 18);
        Banner {
            elapsed: Duration::ZERO,
            logo_index: 0,
            keymap: &Keymap::default(),
        }
        .render(Rect::new(0, 0, 44, 18), &mut buf);
        assert!(grid(&buf).contains('<'), "{}", grid(&buf));
    }

    /// The picture the kept derivation paints, beside the one
    /// rebuilding it per frame paints, at four widths including a
    /// cramped one and with a rebound key column. Printed so it is
    /// read, and asserted identical because this is a performance
    /// change and one moved cell would be the bug:
    /// `cargo test --lib ui::banner::eyeball -- --nocapture`.
    #[test]
    fn print_the_kept_and_the_rebuilt_banner_side_by_side() {
        let mut keymap = Keymap::default();
        keymap.set(KeyAction::StartSearch, KeyCode::Char('日'));
        keymap.set(KeyAction::Quit, KeyCode::F(5));
        let doh = logo_index_for_name(Some("figlet: doh"));
        let elapsed = Duration::from_millis(1234);
        for (label, logo, km) in [
            ("default logo", 0, &Keymap::default()),
            ("figlet: doh", doh, &Keymap::default()),
            (
                "toilet: pagga",
                logo_index_for_name(Some("toilet: pagga")),
                &Keymap::default(),
            ),
            ("default logo, 日 and f5 bound", 0, &keymap),
        ] {
            for (w, h) in [(44u16, 18u16), (90, 26), (60, 10), (22, 12)] {
                let area = Rect::new(0, 0, w, h);
                let mut kept = buffer(w, h);
                let mut rebuilt = buffer(w, h);
                Banner {
                    elapsed,
                    logo_index: logo,
                    keymap: km,
                }
                .render(area, &mut kept);
                super::tests::rebuilding_render(elapsed, logo, km, area, &mut rebuilt);
                println!("--- {label}, {w}x{h}, kept\n{}", grid(&kept));
                assert!(
                    kept == rebuilt,
                    "{label} at {w}x{h} moved:\nrebuilt\n{}",
                    grid(&rebuilt)
                );
            }
        }
    }
}

/// What the home screen costs to paint, against what it cost before
/// the (logo index, keymap) half of it was kept across frames. It is
/// what wim shows whenever nothing is playing, so it is on screen a
/// lot. Run with
/// `cargo test --release --lib ui::banner::frame_budget -- --ignored --nocapture`.
#[cfg(test)]
mod frame_budget {
    use super::tests::{rebuilding_render, rebuilt_menu};
    use super::*;
    use crate::ui::testbuf::buffer;
    use std::time::Instant;

    /// Which profile the numbers came out of, printed because
    /// `cargo test` defaults to the dev one: `opt-level = 0` measured
    /// one path in this crate 11.5x slower than release, which is an
    /// artefact and not a result.
    fn profile() -> &'static str {
        if cfg!(debug_assertions) {
            "debug, opt-level 0: an artefact, re-run with --release"
        } else {
            "release"
        }
    }

    /// Variants timed with their batches alternating, minimum each:
    /// back-to-back runs on this box come out 1.6-2.1x apart, which is
    /// larger than most splits being measured, so only a ratio taken
    /// inside one process is a measurement.
    fn best_each<const N: usize>(batches: u32, n: u32, mut f: impl FnMut(usize)) -> [Duration; N] {
        let mut best = [Duration::MAX; N];
        for _ in 0..batches {
            for (i, slot) in best.iter_mut().enumerate() {
                let t = Instant::now();
                for _ in 0..n {
                    f(i);
                }
                *slot = (*slot).min(t.elapsed() / n);
            }
        }
        best
    }

    /// Both sides of the comparison are ordinary calls that cannot be
    /// inlined into the timing loop. A branch there is specialized
    /// where the real function is not, and that way round a comparison
    /// in this repo reported the wrong sign twice.
    #[inline(never)]
    fn kept_frame(logo: usize, keymap: &Keymap, area: Rect, buf: &mut Buffer) {
        Banner {
            elapsed: Duration::from_millis(1234),
            logo_index: logo,
            keymap,
        }
        .render(area, buf);
    }

    #[inline(never)]
    fn rebuilt_frame(logo: usize, keymap: &Keymap, area: Rect, buf: &mut Buffer) {
        rebuilding_render(Duration::from_millis(1234), logo, keymap, area, buf);
    }

    #[test]
    #[ignore = "timing, run explicitly"]
    fn frame_cost() {
        let budget = anim::FrameBudget::default().interval();
        println!(
            "\n== banner frame cost ({}), budget {budget:.3?} at {}fps ==",
            profile(),
            anim::FALLBACK_HZ,
        );
        let keymap = Keymap::default();
        let doh = logo_index_for_name(Some("figlet: doh"));
        for (label, logo, w, h) in [
            ("default logo", 0, 60u16, 24u16),
            ("default logo", 0, 200, 50),
            ("figlet: doh", doh, 90, 30),
            ("figlet: doh", doh, 200, 50),
        ] {
            let area = Rect::new(0, 0, w, h);
            let mut buf = buffer(w, h);
            let [kept, rebuilt] = best_each::<2>(8, 2000, |which| match which {
                0 => kept_frame(logo, &keymap, area, &mut buf),
                _ => rebuilt_frame(logo, &keymap, area, &mut buf),
            });
            println!(
                "{label:<12} {w}x{h}: kept {kept:>10.3?}  rebuilt per frame {rebuilt:>10.3?}  \
                 {:.2}x",
                rebuilt.as_secs_f64() / kept.as_secs_f64(),
            );
        }
    }

    /// Where that difference is: the derived halves on their own, with
    /// the paint of the art outside the clock.
    #[test]
    #[ignore = "timing, run explicitly"]
    fn derivation_cost() {
        println!("\n== banner derivation ({}) ==", profile());
        let keymap = Keymap::default();

        // one slot hit against rebuilding both halves, per logo: the
        // slot's work is two comparisons either way, the derivation's
        // is the art's width plus seven rows.
        for (label, index) in [
            ("default logo (6 rows)", 0),
            (
                "figlet: doh (50 rows)",
                logo_index_for_name(Some("figlet: doh")),
            ),
        ] {
            #[inline(never)]
            fn kept(index: usize, keymap: &Keymap) -> u16 {
                with_layout(index, keymap, |l| l.logo_width + l.menu_width)
            }
            #[inline(never)]
            fn rebuilt(index: usize, keymap: &Keymap) -> u16 {
                let logo = logo_lines(index)
                    .iter()
                    .map(|l| width(l))
                    .max()
                    .unwrap_or(0) as u16;
                logo + rebuilt_menu(keymap).1
            }
            let [slot, derived] = best_each::<2>(8, 20_000, |which| {
                std::hint::black_box(match which {
                    0 => kept(index, &keymap),
                    _ => rebuilt(index, &keymap),
                });
            });
            println!(
                "{label}: slot {slot:>10.3?}  derived {derived:>10.3?}  {:.2}x",
                derived.as_secs_f64() / slot.as_secs_f64(),
            );
        }

        // the trade this slot exists to avoid: measuring every logo's
        // width up front instead of the one on screen. a first banner
        // frame would pay all of it.
        let at = Instant::now();
        let table: Vec<u16> = (0..logo_count())
            .map(|i| logo_lines(i).iter().map(|l| width(l)).max().unwrap_or(0) as u16)
            .collect();
        println!(
            "rejected, a width per logo up front ({} of them): {:?} on a first frame",
            table.len(),
            at.elapsed(),
        );
    }
}
