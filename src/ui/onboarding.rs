//! The five onboarding screens. Rendering only; `crate::onboarding`
//! owns the step state.
//!
//! Every screen shares one chrome (frame, vertically centred content,
//! a progress rail at the bottom) so setup reads as one guided flow
//! rather than five unrelated prompts dumped at the top-left corner.
//! See `Screen` for the layout contract.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::keymap::{KeyAction, Keymap};
use crate::theme::{shade, BRIGHT, DIM, MID, NEAR_WHITE};
use crate::ui::help::Key;
use crate::ui::textfit::{tail_fit, truncate_with_ellipsis, width};
use crate::ui::{banner, draw};

/// Keys worth knowing before the first screenful of music, which is
/// not the whole keymap; `?` opens that. Remappable actions go through
/// `Key::Bound`/`Key::Pair` so a rebound key is shown as what it is
/// actually bound to: this table used to bake in the literals ("l",
/// "[ / ]"), which meant onboarding confidently taught the wrong keys
/// to anyone with a custom keymap. `Key::Fixed` is for the navigation
/// primitives that genuinely are not remappable.
const CONTROLS: &[(Key, &str)] = &[
    (
        Key::Bound(KeyAction::StartSearch),
        "start or edit the live search query",
    ),
    (Key::Fixed("j / k"), "move selection (arrows work too)"),
    (Key::Fixed("gg / G"), "jump to first / last result"),
    (Key::Fixed("enter"), "play the selected result immediately"),
    (
        Key::Bound(KeyAction::Enqueue),
        "add the selected result to the queue",
    ),
    (
        Key::Bound(KeyAction::CycleLoop),
        "toggle loop on the current track",
    ),
    (
        Key::Bound(KeyAction::ToggleShuffle),
        "toggle shuffle (reshuffles the queue)",
    ),
    (
        Key::Pair(KeyAction::SpeedDown, KeyAction::SpeedUp),
        "speed down / up",
    ),
    (Key::Bound(KeyAction::CycleEq), "cycle eq preset"),
    (
        Key::Bound(KeyAction::OpenLyrics),
        "fetch and show lyrics for what's playing",
    ),
    (
        Key::Bound(KeyAction::OpenNavManager),
        "reopen navidrome setup",
    ),
    (Key::Bound(KeyAction::Quit), "quit"),
];

/// Welcome, controls, navidrome, listenbrainz, paths, done. The rail
/// counts all six including the bookends, since "step 1 of 6" on the
/// welcome screen is what tells the user the flow is short and finite
/// before they commit to it.
pub const TOTAL_STEPS: usize = 6;

/// One content row. `level` is a `theme` brightness, never a hue.
struct Row {
    text: String,
    level: f32,
    centred: bool,
}

impl Row {
    fn new(text: impl Into<String>, level: f32, centred: bool) -> Row {
        Row {
            text: text.into(),
            level,
            centred,
        }
    }
}

/// Shared chrome for every onboarding screen.
///
/// Layout: a frame around the whole area, then the art (if any), a
/// blank row, the content rows, all as one block centred vertically in
/// the framed interior; the rail pins to the interior's bottom row. The
/// block is centred as a unit rather than each row independently, so a
/// short row and a long one do not stagger (the same mistake
/// `banner.rs` records fixing in its menu).
struct Screen {
    /// 1-based, for the rail.
    step: usize,
    /// Figlet art drawn centred above the rows; one cell per char by
    /// construction, like `banner.rs`'s logo.
    art: &'static [&'static str],
    rows: Vec<Row>,
}

impl Screen {
    fn new(step: usize) -> Screen {
        Screen {
            step,
            art: &[],
            rows: Vec::new(),
        }
    }

    fn art(mut self, art: &'static [&'static str]) -> Screen {
        self.art = art;
        self
    }

    fn row(mut self, text: impl Into<String>, level: f32) -> Screen {
        self.rows.push(Row::new(text, level, false));
        self
    }

    fn centred(mut self, text: impl Into<String>, level: f32) -> Screen {
        self.rows.push(Row::new(text, level, true));
        self
    }

    fn gap(mut self) -> Screen {
        self.rows.push(Row::new("", DIM, false));
        self
    }

    fn render(self, area: Rect, buf: &mut Buffer) {
        let body = chrome(buf, area, self.step);
        if body.width == 0 || body.height == 0 {
            return;
        }

        let art_rows = if self.art.is_empty() {
            0
        } else {
            self.art.len() as u16 + 1
        };
        let total = art_rows + self.rows.len() as u16;
        let mut y = body.y + body.height.saturating_sub(total) / 2;

        if !self.art.is_empty() {
            draw_art(buf, body, y, self.art);
            y += art_rows;
        }
        for row in &self.rows {
            if y >= body.y + body.height {
                break;
            }
            if row.centred {
                draw_centred(buf, body, y, &row.text, row.level);
            } else {
                draw_row(buf, body, y, &row.text, row.level);
            }
            y += 1;
        }
    }
}

/// `LoginView`'s place in the flow. It lives in login.rs, which cannot
/// know what it is embedded in.
pub const NAVIDROME_STEP: usize = 3;

/// Wraps `area` in the flow's chrome (frame plus the step rail) and
/// returns the interior left over for content. Shared with
/// `ui::login`, whose screen is step 3 here but is also reopened
/// standalone, where there is no flow for it to be a step of.
pub(crate) fn chrome(buf: &mut Buffer, area: Rect, step: usize) -> Rect {
    draw::frame(buf, area, shade(DIM));
    let inner = draw::framed_inner(area);
    if inner.width == 0 || inner.height == 0 {
        return inner;
    }
    // the rail owns the bottom row plus a blank one above it, so
    // content is never flush against it.
    let rail_rows = if inner.height >= 3 { 2 } else { 0 };
    if rail_rows > 0 {
        draw_rail(buf, inner, step);
    }
    Rect {
        height: inner.height.saturating_sub(rail_rows),
        ..inner
    }
}

/// "● ● ○ ○ ○ ○   step 2 of 6". Position, not just a count: the dots
/// make the remaining distance visible at a glance, the words make it
/// unambiguous. Current step is brightest, completed ones mid, the rest
/// dim, so the eye lands on where it is now.
fn draw_rail(buf: &mut Buffer, inner: Rect, step: usize) {
    let y = inner.y + inner.height - 1;
    let label = format!("step {step} of {TOTAL_STEPS}");
    // two cells per dot (glyph plus its space) bar the last, three
    // spaces, then the words.
    let dots_width = TOTAL_STEPS * 2 - 1;
    let total = dots_width + 3 + width(&label);
    if total > inner.width as usize {
        // no room for both; the words alone still answer "where am I".
        draw_centred(buf, inner, y, &label, DIM);
        return;
    }
    let mut x = inner.x + (inner.width - total as u16) / 2;
    for i in 1..=TOTAL_STEPS {
        let (glyph, level) = match i.cmp(&step) {
            std::cmp::Ordering::Less => ('●', MID),
            std::cmp::Ordering::Equal => ('●', NEAR_WHITE),
            std::cmp::Ordering::Greater => ('○', DIM),
        };
        draw::put(buf, x, y, glyph, shade(level));
        x += 2;
    }
    x += 2;
    draw::at(
        buf,
        x,
        y,
        &label,
        shade(DIM),
        (inner.x + inner.width).saturating_sub(x) as usize,
    );
}

/// Figlet art, centred by char count because it is one cell per char by
/// construction (same assumption `banner.rs` draws its logo under).
/// Blanks are skipped so whatever is behind shows through rather than
/// being punched out.
fn draw_art(buf: &mut Buffer, area: Rect, top: u16, art: &[&str]) {
    let art_width = art.iter().map(|l| l.chars().count()).max().unwrap_or(0) as u16;
    let start_x = area.x + area.width.saturating_sub(art_width) / 2;
    for (row, line) in art.iter().enumerate() {
        let y = top + row as u16;
        if y >= area.y + area.height {
            return;
        }
        for (col, ch) in line.chars().enumerate() {
            let x = start_x + col as u16;
            if ch == ' ' || x >= area.x + area.width {
                continue;
            }
            draw::put(buf, x, y, ch, shade(BRIGHT));
        }
    }
}

pub struct WelcomeView;

impl Widget for WelcomeView {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // the real wordmark, not the words "welcome to wim": this is
        // the first thing anyone sees and a plain text line read as a
        // thin title card. index 0 is the permanent default logo.
        Screen::new(1)
            .art(banner::logo_lines(0))
            .centred("a terminal omniplayer for your music and video", DIM)
            .gap()
            .centred("press any key to set things up", NEAR_WHITE)
            .render(area, buf);
    }
}

pub struct ControlsView<'a> {
    pub keymap: &'a Keymap,
}

impl Widget for ControlsView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // pad the key column by display width, from the *resolved*
        // labels: a key rebound to "f5" is a different length than the
        // default it replaced, and a wide char is two cells while
        // `{:<n}` would count it as one.
        let rows: Vec<(String, &str)> = CONTROLS
            .iter()
            .map(|(key, desc)| (key.label(self.keymap), *desc))
            .collect();
        let key_width = rows.iter().map(|(k, _)| width(k)).max().unwrap_or(0);

        let mut screen = Screen::new(2).centred("controls", NEAR_WHITE).gap();
        for (key, desc) in &rows {
            let pad = " ".repeat(key_width - width(key));
            screen = screen.row(format!("{key}{pad}   {desc}"), DIM);
        }
        screen
            .gap()
            .centred(
                format!(
                    "{} opens the full list any time",
                    Key::Bound(KeyAction::OpenHelp).label(self.keymap)
                ),
                MID,
            )
            .centred("press any key to continue", NEAR_WHITE)
            .render(area, buf);
    }
}

pub struct ListenBrainzView<'a> {
    pub input: &'a str,
}

impl Widget for ListenBrainzView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let prefix = "token: ";
        let value_width = (area.width as usize).saturating_sub(prefix.len() + 6);
        let masked = "*".repeat(self.input.chars().count());

        Screen::new(4)
            .centred("scrobble to listenbrainz?", NEAR_WHITE)
            .centred("optional; skip it and nothing breaks", DIM)
            .gap()
            .row("paste a user token from listenbrainz.org, under", DIM)
            .row("profile -> settings. a token, not your password.", DIM)
            .gap()
            // caret so an empty field reads as "type here" rather than
            // as a dead label.
            .row(
                format!("{prefix}{}_", tail_fit(&masked, value_width)),
                NEAR_WHITE,
            )
            .gap()
            .centred("enter to save, esc to skip", MID)
            .render(area, buf);
    }
}

pub struct LibraryPathsView<'a> {
    pub paths: &'a [String],
    pub input: &'a str,
}

impl Widget for LibraryPathsView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let width_budget = (area.width as usize).saturating_sub(8);
        let mut screen = Screen::new(5)
            .centred("where's your media?", NEAR_WHITE)
            .gap();

        if self.paths.is_empty() {
            screen = screen.row("nothing found yet; add a folder below", DIM);
        } else {
            // counted, so a long list does not just trail off with no
            // sense of how much was found.
            let found = format!(
                "{} folder{} found",
                self.paths.len(),
                if self.paths.len() == 1 { "" } else { "s" }
            );
            screen = screen.row(found, MID);
            for p in self.paths {
                let fitted = truncate_with_ellipsis(p, width_budget);
                screen = screen.row(format!("  {fitted}"), DIM);
            }
        }

        screen
            .gap()
            .row(
                format!(
                    "add path: {}_",
                    tail_fit(self.input, width_budget.saturating_sub(10))
                ),
                NEAR_WHITE,
            )
            .gap()
            .centred("enter to add, enter on empty to continue", MID)
            .render(area, buf);
    }
}

pub struct DoneView;

impl Widget for DoneView {
    fn render(self, area: Rect, buf: &mut Buffer) {
        Screen::new(6)
            .centred("you're all set", NEAR_WHITE)
            .gap()
            .centred("press any key to start", DIM)
            .render(area, buf);
    }
}

fn draw_row(buf: &mut Buffer, area: Rect, y: u16, text: &str, level: f32) {
    let fitted = truncate_with_ellipsis(text, area.width as usize);
    draw::line(buf, area, y, &fitted, shade(level));
}

/// `draw::centered` pads by display width; this centred by char count,
/// so a wide-char line sat right of centre by its own overhang.
fn draw_centred(buf: &mut Buffer, area: Rect, y: u16, text: &str, level: f32) {
    let fitted = truncate_with_ellipsis(text, area.width as usize);
    draw::centered(buf, area, y, &fitted, shade(level));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::testbuf::{assert_outside_untouched, buffer, char_at, grid};

    const SIZES: [(u16, u16); 8] = [
        (1, 1),
        (1, 40),
        (2, 1),
        (40, 0),
        (0, 0),
        (3, 2),
        (5, 3),
        (6, 4),
    ];

    fn keymap() -> Keymap {
        Keymap::default()
    }

    fn render_welcome(w: u16, h: u16) -> String {
        let mut buf = buffer(w, h);
        WelcomeView.render(Rect::new(0, 0, w, h), &mut buf);
        grid(&buf)
    }

    fn render_controls(w: u16, h: u16, km: &Keymap) -> String {
        let mut buf = buffer(w, h);
        ControlsView { keymap: km }.render(Rect::new(0, 0, w, h), &mut buf);
        grid(&buf)
    }

    #[test]
    fn the_welcome_screen_shows_the_wordmark_not_the_words() {
        let g = render_welcome(60, 20);
        // a row of the ANSI-Shadow "WIM" art, which is what index 0 is.
        assert!(g.contains('█'), "the wordmark should be drawn:\n{g}");
        assert!(g.contains("a terminal omniplayer"), "tagline missing:\n{g}");
    }

    #[test]
    fn every_screen_is_framed() {
        let rect = Rect::new(0, 0, 60, 20);
        for (name, g) in [
            ("welcome", render_welcome(60, 20)),
            ("controls", render_controls(60, 20, &keymap())),
        ] {
            let first = g.lines().next().unwrap();
            assert!(
                first.starts_with('┌') && first.ends_with('┐'),
                "{name} has no top frame:\n{g}"
            );
        }
        let mut buf = buffer(60, 20);
        DoneView.render(rect, &mut buf);
        let g = grid(&buf);
        let last = g.lines().last().unwrap();
        assert!(
            last.starts_with('└') && last.ends_with('┘'),
            "no bottom frame:\n{g}"
        );
    }

    /// The rail is the whole point of the chrome: it answers "how much
    /// of this is left" on every screen.
    #[test]
    fn the_rail_marks_the_current_step_on_each_screen() {
        let cases: [(&str, u16); 4] = [("welcome", 1), ("controls", 2), ("paths", 5), ("done", 6)];
        for (name, step) in cases {
            let mut buf = buffer(60, 24);
            let area = Rect::new(0, 0, 60, 24);
            match name {
                "welcome" => WelcomeView.render(area, &mut buf),
                "controls" => ControlsView { keymap: &keymap() }.render(area, &mut buf),
                "paths" => LibraryPathsView {
                    paths: &[],
                    input: "",
                }
                .render(area, &mut buf),
                _ => DoneView.render(area, &mut buf),
            }
            let g = grid(&buf);
            assert!(
                g.contains(&format!("step {step} of 6")),
                "{name} should say step {step}:\n{g}"
            );
        }
    }

    #[test]
    fn the_rail_falls_back_to_words_when_the_dots_do_not_fit() {
        // 20 cells of interior cannot hold 11 dot cells plus the label.
        let mut buf = buffer(24, 10);
        DoneView.render(Rect::new(0, 0, 24, 10), &mut buf);
        let g = grid(&buf);
        assert!(g.contains("step 6 of 6"), "{g}");
        assert!(
            !g.contains('●'),
            "dots should be dropped, not clipped:\n{g}"
        );
    }

    /// The bug this table's rewrite exists to fix: a rebound key used
    /// to still be taught as its default.
    #[test]
    fn the_controls_screen_shows_rebound_keys_not_the_defaults() {
        use crossterm::event::KeyCode;
        let mut km = keymap();
        km.set(KeyAction::Enqueue, KeyCode::F(5));
        let g = render_controls(70, 24, &km);
        assert!(g.contains("f5"), "the rebound key should be shown:\n{g}");
        let default_row = g
            .lines()
            .find(|l| l.contains("add the selected result"))
            .unwrap();
        assert!(
            !default_row
                .trim_start_matches('│')
                .trim_start()
                .starts_with('a'),
            "still teaching the default 'a':\n{default_row}"
        );
    }

    #[test]
    fn the_controls_key_column_is_padded_from_resolved_labels() {
        use crossterm::event::KeyCode;
        let mut km = keymap();
        km.set(KeyAction::Quit, KeyCode::F(12));
        let g = render_controls(70, 24, &km);
        // every description starts at the same column, whatever the
        // keys resolved to.
        let cols: Vec<usize> = g
            .lines()
            .filter_map(|l| l.find("cycle eq preset").or_else(|| l.find("quit")))
            .collect();
        assert!(cols.len() >= 2, "{g}");
        assert!(
            cols.windows(2).all(|w| w[0] == w[1]),
            "description column is jagged at {cols:?}:\n{g}"
        );
    }

    #[test]
    fn the_controls_screen_points_at_the_full_help() {
        let g = render_controls(70, 24, &keymap());
        assert!(g.contains("opens the full list"), "{g}");
    }

    #[test]
    fn the_token_screen_masks_the_input_one_star_per_char() {
        let mut buf = buffer(72, 16);
        ListenBrainzView { input: "abc123" }.render(Rect::new(0, 0, 72, 16), &mut buf);
        let g = grid(&buf);
        assert!(g.contains("scrobble to listenbrainz?"), "{g}");
        assert!(g.contains("token: ******"), "{g}");
        assert!(
            !g.contains("abc123"),
            "the token must never be echoed:\n{g}"
        );
    }

    #[test]
    fn the_paths_screen_counts_what_it_found() {
        let mut buf = buffer(50, 16);
        LibraryPathsView {
            paths: &["/home/a/music".to_string(), "/mnt/b".to_string()],
            input: "/mnt/c",
        }
        .render(Rect::new(0, 0, 50, 16), &mut buf);
        let g = grid(&buf);
        assert!(g.contains("2 folders found"), "{g}");
        assert!(g.contains("/home/a/music"), "{g}");
        assert!(g.contains("add path: /mnt/c_"), "{g}");
    }

    #[test]
    fn one_path_is_singular() {
        let mut buf = buffer(50, 16);
        LibraryPathsView {
            paths: &["/home/a/music".to_string()],
            input: "",
        }
        .render(Rect::new(0, 0, 50, 16), &mut buf);
        assert!(grid(&buf).contains("1 folder found"), "{}", grid(&buf));
    }

    #[test]
    fn an_empty_paths_list_says_so() {
        let mut buf = buffer(70, 16);
        LibraryPathsView {
            paths: &[],
            input: "",
        }
        .render(Rect::new(0, 0, 70, 16), &mut buf);
        assert!(grid(&buf).contains("nothing found yet"), "{}", grid(&buf));
    }

    /// A library path is typed, so it can be non-ascii: the glyphs
    /// after a wide one must not slide a cell left per wide char.
    #[test]
    fn a_wide_library_path_does_not_shift_the_glyphs_after_it() {
        let mut buf = buffer(50, 16);
        LibraryPathsView {
            paths: &["日本x".to_string()],
            input: "",
        }
        .render(Rect::new(0, 0, 50, 16), &mut buf);
        let g = grid(&buf);
        let listed: Vec<char> = g
            .lines()
            .find(|l| l.contains('日'))
            .unwrap()
            .chars()
            .collect();
        let start = listed.iter().position(|c| *c == '日').unwrap();
        // wide char owns two cells, so its second reads as a blank.
        assert_eq!(
            (listed[start], listed[start + 1], listed[start + 2]),
            ('日', ' ', '本')
        );
        assert_eq!(listed[start + 4], 'x');
    }

    /// `draw_centred` used to pad by char count, which put a wide-char
    /// line right of centre by its own overhang.
    #[test]
    fn a_centred_wide_line_sits_in_the_middle_by_cells() {
        // "日本" is 4 cells in a 10-wide rect, so x=3, not x=4.
        let mut buf = buffer(10, 1);
        draw_centred(&mut buf, Rect::new(0, 0, 10, 1), 0, "日本", NEAR_WHITE);
        assert_eq!(char_at(&buf, 3, 0), '日');
    }

    #[test]
    fn nothing_is_drawn_outside_the_rect() {
        let rect = Rect::new(2, 1, 30, 12);
        let long = "日本語テスト曲日本語テスト曲日本語テスト曲";

        let mut buf = buffer(40, 16);
        WelcomeView.render(rect, &mut buf);
        assert_outside_untouched(&buf, rect);

        let mut buf = buffer(40, 16);
        DoneView.render(rect, &mut buf);
        assert_outside_untouched(&buf, rect);

        let mut buf = buffer(40, 16);
        ControlsView { keymap: &keymap() }.render(rect, &mut buf);
        assert_outside_untouched(&buf, rect);

        let mut buf = buffer(40, 16);
        ListenBrainzView { input: long }.render(rect, &mut buf);
        assert_outside_untouched(&buf, rect);

        let mut buf = buffer(40, 16);
        LibraryPathsView {
            paths: &[long.to_string(), long.to_string()],
            input: long,
        }
        .render(rect, &mut buf);
        assert_outside_untouched(&buf, rect);
    }

    #[test]
    fn degenerate_sizes_do_not_panic() {
        let long = "日本語テスト曲";
        let km = keymap();
        for (w, h) in SIZES {
            let mut buf = buffer(w.max(1), h.max(1));
            let area = Rect::new(0, 0, w, h);
            WelcomeView.render(area, &mut buf);
            DoneView.render(area, &mut buf);
            ControlsView { keymap: &km }.render(area, &mut buf);
            ListenBrainzView { input: long }.render(area, &mut buf);
            LibraryPathsView {
                paths: &[long.to_string()],
                input: long,
            }
            .render(area, &mut buf);
        }
    }

    /// A tall-but-narrow terminal is the case where the content block
    /// and the rail can collide; the rail must still be the last row.
    #[test]
    fn the_rail_stays_on_the_bottom_row_when_content_overflows() {
        let mut buf = buffer(70, 12);
        ControlsView { keymap: &keymap() }.render(Rect::new(0, 0, 70, 12), &mut buf);
        let g = grid(&buf);
        let rail = g.lines().nth(10).unwrap();
        assert!(
            rail.contains("step 2 of 6"),
            "rail should be the interior's last row:\n{g}"
        );
    }
}
