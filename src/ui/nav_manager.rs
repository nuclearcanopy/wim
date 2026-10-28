use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::config::NavidromeConfig;
use crate::theme::{shade, DIM, NEAR_BLACK, NEAR_WHITE};
use crate::ui::header::draw_header;
use crate::ui::textfit::truncate_with_ellipsis;

/// The profile list; not shown when adding a new one (that reuses
/// `LoginView` directly). Selecting an existing profile reconnects/
/// refreshes it; `d` removes it; the last row is always "add new".
pub struct NavManagerListView<'a> {
    pub servers: &'a [NavidromeConfig],
    pub selected: usize,
    pub message: &'a str,
    /// Same index as `servers`; `None` = not tested this session.
    pub connection_status: &'a [Option<bool>],
}

impl Widget for NavManagerListView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        // takes over a rect that already has other content drawn
        // into it, so it has to own every cell: without the clear,
        // whatever this menu did not write still showed through.
        let area = crate::ui::draw::overlay(buf, area);

        let width = area.width as usize;
        let mut y = draw_header(
            buf,
            area,
            area.y,
            "navidrome profiles",
            "enter: reconnect/add, d d: remove, esc: close",
            shade(DIM),
            shade(NEAR_BLACK),
        );
        y += 1;

        if self.servers.is_empty() {
            draw_line(buf, area, y, "(no profiles yet)", shade(DIM));
            y += 1;
        }

        for (i, server) in self.servers.iter().enumerate() {
            let active = i == self.selected;
            let marker = if active { "> " } else { "  " };
            // ✓/✗ are plain text-presentation symbols (not emoji
            // variants), so they stay monochrome like everything else.
            let status = match self.connection_status.get(i) {
                Some(Some(true)) => "✓ ",
                Some(Some(false)) => "✗ ",
                _ => "  ",
            };
            let line = format!("{marker}{status}{} ({})", server.url, server.username);
            draw_line(
                buf,
                area,
                y,
                &truncate_with_ellipsis(&line, width),
                if active {
                    shade(NEAR_WHITE)
                } else {
                    shade(DIM)
                },
            );
            y += 1;
        }

        let add_active = self.selected == self.servers.len();
        let marker = if add_active { "> " } else { "  " };
        draw_line(
            buf,
            area,
            y,
            &format!("{marker}[ add new profile ]"),
            if add_active {
                shade(NEAR_WHITE)
            } else {
                shade(DIM)
            },
        );
        y += 2;

        draw_line(
            buf,
            area,
            y,
            &truncate_with_ellipsis(self.message, width),
            shade(DIM),
        );
    }
}

fn draw_line(buf: &mut Buffer, area: Rect, y: u16, text: &str, color: ratatui::style::Color) {
    crate::ui::draw::line(buf, area, y, text, color);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::testbuf::{
        assert_no_bleed_through, assert_outside_untouched, buffer, grid, plated_buffer,
    };

    fn server(url: &str, username: &str) -> NavidromeConfig {
        NavidromeConfig {
            url: url.to_string(),
            username: username.to_string(),
            password: None,
            password_file: None,
            max_bitrate_kbps: None,
            transcode_format: None,
        }
    }

    fn render(
        area: Rect,
        buf: &mut Buffer,
        servers: &[NavidromeConfig],
        status: &[Option<bool>],
        selected: usize,
    ) {
        NavManagerListView {
            servers,
            selected,
            message: "",
            connection_status: status,
        }
        .render(area, buf);
    }

    #[test]
    fn an_empty_list_offers_only_the_add_row() {
        let area = Rect::new(0, 0, 54, 10);
        let mut buf = buffer(54, 10);
        render(area, &mut buf, &[], &[], 0);
        assert_eq!(grid(&buf), SNAPSHOT.join("\n"));
    }

    const SNAPSHOT: [&str; 10] = [
        "┌────────────────────────────────────────────────────┐",
        "│ navidrome profiles │ enter: reconnect/add, d d: r… │",
        "│ ───────────────────┴────────────────────────────── │",
        "│                                                    │",
        "│ (no profiles yet)                                  │",
        "│ > [ add new profile ]                              │",
        "│                                                    │",
        "│                                                    │",
        "│                                                    │",
        "└────────────────────────────────────────────────────┘",
    ];

    #[test]
    fn each_profile_row_carries_its_url_username_and_connection_mark() {
        let area = Rect::new(0, 0, 54, 10);
        let mut buf = buffer(54, 10);
        let servers = [
            server("https://a.example", "ana"),
            server("https://b.example", "bob"),
        ];
        render(area, &mut buf, &servers, &[Some(true), Some(false)], 0);
        let g = grid(&buf);
        assert!(g.contains("> ✓ https://a.example (ana)"), "{g}");
        assert!(g.contains("  ✗ https://b.example (bob)"), "{g}");
        assert!(g.contains("  [ add new profile ]"), "{g}");
    }

    /// A punycode-free non-ascii url is the real case here: two cells
    /// per glyph, so the "(username)" suffix after it has to land that
    /// much further right rather than a cell per wide char too few.
    #[test]
    fn a_wide_url_does_not_shift_the_username_after_it() {
        let area = Rect::new(0, 0, 54, 10);
        let mut buf = buffer(54, 10);
        let servers = [server("日本", "ana")];
        render(area, &mut buf, &servers, &[None], 0);
        let g = grid(&buf);
        let row: Vec<char> = g
            .lines()
            .find(|r| r.contains('日'))
            .unwrap()
            .chars()
            .collect();
        let col = row.iter().position(|c| *c == '日').expect("the wide url");
        assert_eq!(
            (
                row[col],
                row[col + 1],
                row[col + 2],
                row[col + 3],
                row[col + 4],
                row[col + 5]
            ),
            ('日', ' ', '本', ' ', ' ', '(')
        );
    }

    #[test]
    fn a_wide_url_stays_inside_the_rect() {
        let mut buf = buffer(40, 12);
        let area = Rect::new(2, 1, 20, 8);
        let servers = [server("日本語テスト曲日本語テスト曲", "日本語")];
        render(area, &mut buf, &servers, &[Some(true)], 0);
        assert_outside_untouched(&buf, area);
    }

    #[test]
    fn the_overlay_covers_every_cell_of_its_rect() {
        let mut buf = plated_buffer(44, 14);
        let area = Rect::new(2, 1, 40, 12);
        let servers = [server("https://a.example", "ana")];
        render(area, &mut buf, &servers, &[Some(true)], 0);
        assert_no_bleed_through(&buf, area);
    }

    #[test]
    fn the_frame_closes_all_four_corners() {
        let mut buf = buffer(44, 14);
        let area = Rect::new(2, 1, 40, 12);
        render(area, &mut buf, &[], &[], 0);
        let g = grid(&buf);
        let rows: Vec<Vec<char>> = g.lines().map(|r| r.chars().collect()).collect();
        assert_eq!(
            (rows[1][2], rows[1][41], rows[12][2], rows[12][41]),
            ('┌', '┐', '└', '┘')
        );
    }

    #[test]
    fn degenerate_sizes_do_not_panic() {
        let servers = [server("日本語", "日本"), server("b", "bob")];
        for (w, h) in [(1, 1), (1, 40), (2, 1), (40, 0), (0, 0), (3, 2)] {
            let mut buf = buffer(w.max(1), h.max(1));
            render(
                Rect::new(0, 0, w, h),
                &mut buf,
                &servers,
                &[Some(true), None],
                2,
            );
        }
    }
}
