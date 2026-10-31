use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::config::NavidromeConfig;
use crate::theme::{shade, DIM, NEAR_BLACK, NEAR_WHITE};
use crate::ui::header::draw_header;
use crate::ui::results_list::scroll_cues;
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

        // the add row is the list's last entry, because that is what
        // j/k treats it as; ✓/✗ are plain text-presentation symbols
        // (not emoji variants), so they stay monochrome like
        // everything else.
        let mut entries: Vec<String> = self
            .servers
            .iter()
            .enumerate()
            .map(|(i, server)| {
                let status = match self.connection_status.get(i) {
                    Some(Some(true)) => "✓ ",
                    Some(Some(false)) => "✗ ",
                    _ => "  ",
                };
                format!("{status}{} ({})", server.url, server.username)
            })
            .collect();
        entries.push("[ add new profile ]".to_string());

        // the message row claims its row before the list does, then
        // the list scrolls inside what is left: it had no offset at
        // all, so enter reconnected to a profile that was selectable
        // and never drawn.
        let footer = 2;
        let list_top = y;
        let room = (area.y + area.height).saturating_sub(list_top);
        let rows = room.saturating_sub(footer).min(entries.len() as u16);
        let offset = crate::ui::scroll::offset_for(self.selected, rows as usize);
        // one column of gutter for the scroll cue, and only while
        // there is one: a list that fits keeps its full width.
        let scrolls = entries.len() > rows as usize;
        let text_width = width.saturating_sub(usize::from(scrolls));
        for (row, (i, entry)) in entries
            .iter()
            .enumerate()
            .skip(offset)
            .take(rows as usize)
            .enumerate()
        {
            let active = i == self.selected;
            let marker = if active { "> " } else { "  " };
            draw_line(
                buf,
                Rect {
                    width: text_width as u16,
                    ..area
                },
                list_top + row as u16,
                &truncate_with_ellipsis(&format!("{marker}{entry}"), text_width),
                if active {
                    shade(NEAR_WHITE)
                } else {
                    shade(DIM)
                },
            );
        }
        scroll_cues(
            buf,
            Rect {
                y: list_top,
                height: rows,
                ..area
            },
            offset,
            entries.len(),
        );

        y = list_top + rows + 1;
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
        // "d d: r…" is one cell of action (remove? rename?), so it
        // drops back to the whole hint before it (ui::header).
        "│ navidrome profiles │ enter: reconnect/add…         │",
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

    fn many(n: usize) -> Vec<NavidromeConfig> {
        (0..n)
            .map(|i| server(&format!("https://{i}.example"), &format!("user{i}")))
            .collect()
    }

    /// j/k walks every profile and the add row, so every one of them
    /// has to be reachable: enter on a row nobody can see reconnects
    /// to a profile the user never picked.
    #[test]
    fn the_selected_profile_is_on_screen_at_every_selection() {
        let servers = many(12);
        let status = vec![Some(true); servers.len()];
        for selected in 0..=servers.len() {
            let mut buf = buffer(54, 10);
            render(
                Rect::new(0, 0, 54, 10),
                &mut buf,
                &servers,
                &status,
                selected,
            );
            let g = grid(&buf);
            let wanted = if selected == servers.len() {
                "> [ add new profile ]".to_string()
            } else {
                format!(
                    "> ✓ {} ({})",
                    servers[selected].url, servers[selected].username
                )
            };
            assert!(
                g.contains(&wanted),
                "selection {selected} is off screen, wanted {wanted:?}:\n{g}"
            );
        }
    }

    /// Same `▴`/`▾` gutter cue `lyrics_view` uses; the add row counts
    /// as the list's last entry, since that is what j/k treats it as.
    #[test]
    fn a_scrolled_list_cues_what_is_off_each_edge() {
        let servers = many(12);
        let status = vec![None; servers.len()];
        let cue = |selected: usize, h: u16| {
            let mut buf = buffer(54, h);
            render(
                Rect::new(0, 0, 54, h),
                &mut buf,
                &servers,
                &status,
                selected,
            );
            let g = grid(&buf);
            (g.contains('▴'), g.contains('▾'))
        };
        assert_eq!(cue(0, 10), (false, true), "more below only");
        assert_eq!(cue(servers.len(), 10), (true, false), "above only");
        assert_eq!(cue(0, 24), (false, false), "every profile fits");
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

#[cfg(test)]
mod eyeball {
    use super::*;
    use crate::ui::testbuf::{buffer, grid};

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

    /// The list at the rows `main_content` actually gets (a 14-row
    /// terminal with the default 4-row bottom bar leaves 10), with the
    /// selection on the add row, which is where holding `j` parks it:
    /// `cargo test --lib ui::nav_manager::eyeball -- --nocapture`.
    /// Asserts the floor too, so it is a test and not just a printer.
    #[test]
    fn print_the_list_with_the_selection_at_the_end() {
        let servers: Vec<NavidromeConfig> = (0..12)
            .map(|i| server(&format!("https://{i}.example"), &format!("user{i}")))
            .collect();
        let status: Vec<Option<bool>> = (0..12).map(|i| Some(i % 2 == 0)).collect();
        for (w, h, selected) in [
            (54u16, 10u16, 12usize),
            (54, 10, 0),
            (54, 10, 6),
            (54, 24, 12),
        ] {
            let mut buf = buffer(w, h);
            NavManagerListView {
                servers: &servers,
                selected,
                message: "connected",
                connection_status: &status,
            }
            .render(Rect::new(0, 0, w, h), &mut buf);
            let text = grid(&buf);
            println!("--- {w}x{h} selected={selected}\n{text}");
            assert!(
                text.chars().any(|c| c != '.' && c != '\n'),
                "{w}x{h} drew nothing at all"
            );
        }
    }
}
