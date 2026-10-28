use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::theme::{shade, DIM, NEAR_WHITE};
use crate::ui::textfit::{tail_fit, truncate_with_ellipsis};

const CONTROLS: &[&str] = &[
    "/        start or edit the live search query",
    "j / k    move selection   (down/up also work)",
    "gg / G   jump to first / last result",
    "enter    play the selected result immediately",
    "a        add the selected result to the queue",
    "l        toggle loop on the current track",
    "s        toggle shuffle (reshuffles the queue)",
    "[ / ]    speed down / up",
    "e        cycle eq preset",
    "L        fetch and show lyrics for what's playing",
    "n        reopen navidrome setup",
    "q / esc  quit",
];

pub struct WelcomeView;

impl Widget for WelcomeView {
    fn render(self, area: Rect, buf: &mut Buffer) {
        draw_centered(buf, area, area.y, "welcome to wim", shade(NEAR_WHITE));
        draw_centered(
            buf,
            area,
            area.y + 2,
            "a terminal omniplayer for your music and video",
            shade(DIM),
        );
        draw_centered(
            buf,
            area,
            area.y + 4,
            "press any key to set things up",
            shade(DIM),
        );
    }
}

pub struct ControlsView;

impl Widget for ControlsView {
    fn render(self, area: Rect, buf: &mut Buffer) {
        draw_line(buf, area, area.y, "controls", shade(NEAR_WHITE));
        for (i, line) in CONTROLS.iter().enumerate() {
            draw_line(buf, area, area.y + 2 + i as u16, line, shade(DIM));
        }
        draw_line(
            buf,
            area,
            area.y + 3 + CONTROLS.len() as u16,
            "press any key to continue",
            shade(DIM),
        );
    }
}

pub struct ListenBrainzView<'a> {
    pub input: &'a str,
}

impl Widget for ListenBrainzView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        draw_line(
            buf,
            area,
            area.y,
            "scrobble to listenbrainz? (optional)",
            shade(NEAR_WHITE),
        );
        draw_line(
            buf,
            area,
            area.y + 2,
            "paste a user token from listenbrainz.org -> profile -> settings",
            shade(DIM),
        );
        draw_line(
            buf,
            area,
            area.y + 3,
            "(a token, not your account password)",
            shade(DIM),
        );

        let width = area.width as usize;
        let masked: String = "*".repeat(self.input.chars().count());
        let prefix = "token: ";
        let value_width = width.saturating_sub(prefix.chars().count());
        draw_line(
            buf,
            area,
            area.y + 5,
            &format!("{prefix}{}", tail_fit(&masked, value_width)),
            shade(NEAR_WHITE),
        );

        draw_line(
            buf,
            area,
            area.y + 7,
            "enter to save and continue, esc to skip (scrobbling just won't happen)",
            shade(DIM),
        );
    }
}

pub struct LibraryPathsView<'a> {
    pub paths: &'a [String],
    pub input: &'a str,
}

impl Widget for LibraryPathsView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let width = area.width as usize;
        draw_line(buf, area, area.y, "where's your media?", shade(NEAR_WHITE));
        let mut y = area.y + 2;
        if self.paths.is_empty() {
            draw_line(
                buf,
                area,
                y,
                "(no paths found: type one below, or skip to browse nothing yet)",
                shade(DIM),
            );
            y += 1;
        } else {
            for p in self.paths {
                draw_line(
                    buf,
                    area,
                    y,
                    &format!("  {}", truncate_with_ellipsis(p, width.saturating_sub(2))),
                    shade(DIM),
                );
                y += 1;
            }
        }
        y += 1;
        let prefix = "add path: ";
        let value_width = width.saturating_sub(prefix.chars().count());
        draw_line(
            buf,
            area,
            y,
            &format!("{prefix}{}", tail_fit(self.input, value_width)),
            shade(NEAR_WHITE),
        );
        y += 2;
        draw_line(
            buf,
            area,
            y,
            "enter to add, enter on empty to continue",
            shade(DIM),
        );
    }
}

pub struct DoneView;

impl Widget for DoneView {
    fn render(self, area: Rect, buf: &mut Buffer) {
        draw_centered(buf, area, area.y, "you're all set", shade(NEAR_WHITE));
        draw_centered(buf, area, area.y + 2, "press any key to start", shade(DIM));
    }
}

fn draw_line(buf: &mut Buffer, area: Rect, y: u16, text: &str, color: ratatui::style::Color) {
    let fitted = truncate_with_ellipsis(text, area.width as usize);
    crate::ui::draw::line(buf, area, y, &fitted, color);
}

/// `draw::centered` pads by display width; this centred by char count,
/// so a wide-char line sat right of centre by its own overhang.
fn draw_centered(buf: &mut Buffer, area: Rect, y: u16, text: &str, color: ratatui::style::Color) {
    let fitted = truncate_with_ellipsis(text, area.width as usize);
    crate::ui::draw::centered(buf, area, y, &fitted, color);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::testbuf::{assert_outside_untouched, buffer, char_at, grid};

    const SIZES: [(u16, u16); 6] = [(1, 1), (1, 40), (2, 1), (40, 0), (0, 0), (3, 2)];

    #[test]
    fn the_welcome_screen_centres_its_three_lines() {
        let mut buf = buffer(30, 5);
        WelcomeView.render(Rect::new(0, 0, 30, 5), &mut buf);
        assert_eq!(
            grid(&buf),
            [
                "........welcome to wim........",
                "..............................",
                "a terminal omniplayer for you…",
                "..............................",
                "press any key to set things up",
            ]
            .join("\n")
        );
    }

    #[test]
    fn the_done_screen_centres_both_of_its_lines() {
        let mut buf = buffer(30, 3);
        DoneView.render(Rect::new(0, 0, 30, 3), &mut buf);
        assert_eq!(
            grid(&buf),
            [
                "........you're all set........",
                "..............................",
                "....press any key to start....",
            ]
            .join("\n")
        );
    }

    #[test]
    fn the_controls_screen_lists_every_binding_and_the_continue_hint() {
        let h = CONTROLS.len() as u16 + 4;
        let mut buf = buffer(50, h);
        ControlsView.render(Rect::new(0, 0, 50, h), &mut buf);
        let g = grid(&buf);
        assert!(g.starts_with("controls"), "{g}");
        for line in CONTROLS {
            assert!(g.contains(line), "missing {line:?}:\n{g}");
        }
        assert!(g.contains("press any key to continue"), "{g}");
    }

    #[test]
    fn the_token_screen_masks_the_input_one_star_per_char() {
        let mut buf = buffer(72, 9);
        ListenBrainzView { input: "abc123" }.render(Rect::new(0, 0, 72, 9), &mut buf);
        let g = grid(&buf);
        assert!(g.contains("scrobble to listenbrainz? (optional)"), "{g}");
        assert!(g.contains("token: ******"), "{g}");
        assert!(
            !g.contains("abc123"),
            "the token must never be echoed:\n{g}"
        );
    }

    #[test]
    fn the_paths_screen_lists_what_was_found_and_the_add_line() {
        let mut buf = buffer(40, 8);
        LibraryPathsView {
            paths: &["/home/a/music".to_string()],
            input: "/mnt/b",
        }
        .render(Rect::new(0, 0, 40, 8), &mut buf);
        assert_eq!(
            grid(&buf),
            [
                "where's your media?.....................",
                "........................................",
                "  /home/a/music.........................",
                "........................................",
                "add path: /mnt/b........................",
                "........................................",
                "enter to add, enter on empty to continue",
                "........................................",
            ]
            .join("\n")
        );
    }

    #[test]
    fn an_empty_paths_list_says_so() {
        let mut buf = buffer(70, 8);
        LibraryPathsView {
            paths: &[],
            input: "",
        }
        .render(Rect::new(0, 0, 70, 8), &mut buf);
        assert!(grid(&buf).contains("(no paths found:"), "{}", grid(&buf));
    }

    /// A library path is typed, so it can be non-ascii: the glyphs
    /// after a wide one must not slide a cell left per wide char.
    #[test]
    fn a_wide_library_path_does_not_shift_the_glyphs_after_it() {
        let mut buf = buffer(40, 8);
        LibraryPathsView {
            paths: &["日本x".to_string()],
            input: "日本x",
        }
        .render(Rect::new(0, 0, 40, 8), &mut buf);
        let g = grid(&buf);
        let listed: Vec<char> = g.lines().nth(2).unwrap().chars().collect();
        assert_eq!((listed[2], listed[3], listed[4]), ('日', ' ', '本'));
        assert_eq!(listed[6], 'x');
        let typed: Vec<char> = g.lines().nth(4).unwrap().chars().collect();
        let start = 10; // "add path: "
        assert_eq!((typed[start], typed[start + 1]), ('日', ' '));
        assert_eq!(typed[start + 4], 'x');
    }

    /// `draw_centered` used to pad by char count, which put a wide-char
    /// line right of centre by its own overhang.
    #[test]
    fn a_centred_wide_line_sits_in_the_middle_by_cells() {
        // "日本" is 4 cells in a 10-wide rect, so x=3, not x=4.
        let mut buf = buffer(10, 1);
        draw_centered(
            &mut buf,
            Rect::new(0, 0, 10, 1),
            0,
            "日本",
            shade(NEAR_WHITE),
        );
        assert_eq!(char_at(&buf, 3, 0), '日');
    }

    #[test]
    fn nothing_is_drawn_outside_the_rect() {
        let rect = Rect::new(2, 1, 20, 10);
        let long = "日本語テスト曲日本語テスト曲日本語テスト曲";

        let mut buf = buffer(40, 14);
        WelcomeView.render(rect, &mut buf);
        assert_outside_untouched(&buf, rect);

        let mut buf = buffer(40, 14);
        DoneView.render(rect, &mut buf);
        assert_outside_untouched(&buf, rect);

        let mut buf = buffer(40, 14);
        ControlsView.render(rect, &mut buf);
        assert_outside_untouched(&buf, rect);

        let mut buf = buffer(40, 14);
        ListenBrainzView { input: long }.render(rect, &mut buf);
        assert_outside_untouched(&buf, rect);

        let mut buf = buffer(40, 14);
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
        for (w, h) in SIZES {
            let mut buf = buffer(w.max(1), h.max(1));
            let area = Rect::new(0, 0, w, h);
            WelcomeView.render(area, &mut buf);
            DoneView.render(area, &mut buf);
            ControlsView.render(area, &mut buf);
            ListenBrainzView { input: long }.render(area, &mut buf);
            LibraryPathsView {
                paths: &[long.to_string()],
                input: long,
            }
            .render(area, &mut buf);
        }
    }
}
