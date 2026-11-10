use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::torbox::AccountItem;
use crate::torbox_workspace::{Page, Workspace};

pub struct SetupView<'a> {
    pub path: &'a str,
    pub message: &'a str,
    pub busy: bool,
}

impl Widget for SetupView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let area = crate::ui::onboarding::chrome(buf, area, crate::ui::onboarding::TORBOX_STEP);
        if area.width == 0 || area.height == 0 {
            return;
        }
        crate::ui::header::draw_header(
            buf,
            area,
            area.y,
            "connect TorBox",
            "optional · enter to verify and continue · esc to skip",
            crate::theme::shade(crate::theme::DIM),
            crate::theme::shade(crate::theme::NEAR_BLACK),
        );
        let rows = [
            "TorBox uses an API token, not your account password.",
            "Keep it in a protected file outside the Nix store (agenix recommended).",
            "Wim saves only the file path; the token is never shown or stored here.",
            "Connection tests are read-only. Mullvad VPN may be rejected; see docs/torbox.md.",
        ];
        for (i, line) in rows.iter().enumerate() {
            crate::ui::draw::line(
                buf,
                area,
                area.y + 3 + i as u16,
                line,
                crate::theme::shade(crate::theme::DIM),
            );
        }
        let path = format!(
            "token file: {}_",
            crate::ui::textfit::tail_fit(self.path, area.width.saturating_sub(14) as usize)
        );
        crate::ui::draw::line(
            buf,
            area,
            area.y + 9,
            &path,
            crate::theme::shade(crate::theme::NEAR_WHITE),
        );
        let message = if self.busy {
            "checking token file and TorBox connection…"
        } else {
            self.message
        };
        crate::ui::draw::line(
            buf,
            area,
            area.y + 11,
            message,
            crate::theme::shade(crate::theme::MID),
        );
    }
}

/// Background level of the full-width plate behind the selected row.
const SELECT_BG: f32 = 0.22;

pub fn render(frame: &mut Frame<'_>, area: Rect, workspace: &Workspace) {
    if area.width < 36 || area.height < 10 {
        return;
    }
    let width = area.width.min(96);
    let height = area.height.min(28);
    let panel = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, panel);
    let title = match workspace.page {
        Page::Setup => "connect TorBox · API token file",
        Page::Search => "your TorBox library · enter searches this account",
        Page::Account => "torbox account · 1 torrents · 2 usenet · 3 web downloads",
        Page::Files => "files · enter play · q queue",
    };
    let inner = Block::default().borders(Borders::ALL).title(title);
    let content = inner.inner(panel);
    frame.render_widget(inner, panel);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(content);

    render_input(frame, chunks[0], workspace);
    render_list(frame, chunks[1], workspace);
    render_status(frame, chunks[2], workspace);
    render_footer(frame, chunks[3], workspace);
}

fn render_input(frame: &mut Frame<'_>, area: Rect, workspace: &Workspace) {
    let label_style = Style::default()
        .fg(crate::theme::shade(crate::theme::DIM))
        .add_modifier(Modifier::BOLD);
    let value_style = Style::default().fg(crate::theme::shade(crate::theme::NEAR_WHITE));
    let line = if workspace.link_input.is_some() {
        Line::from(vec![
            Span::styled("add link: ", label_style),
            Span::styled(
                workspace
                    .link_input
                    .as_deref()
                    .unwrap_or_default()
                    .to_string(),
                value_style,
            ),
            Span::styled("_", value_style),
        ])
    } else {
        match workspace.page {
            Page::Setup => Line::from(vec![
                Span::styled("token file: ", label_style),
                Span::styled(workspace.token_path_input.clone(), value_style),
                Span::styled("_", value_style),
            ]),
            Page::Search => {
                let mut spans = vec![
                    Span::styled("query: ", label_style),
                    Span::styled(workspace.query.clone(), value_style),
                ];
                if workspace.search_editing {
                    spans.push(Span::styled("_", value_style));
                }
                Line::from(spans)
            }
            Page::Account => Line::from(vec![
                Span::styled("service: ", label_style),
                Span::styled(workspace.service.as_str().to_string(), value_style),
            ]),
            Page::Files => {
                let selected = workspace.account_selected.as_ref();
                Line::from(vec![
                    Span::styled(
                        selected.map_or("item".to_string(), |item| item.name.clone()),
                        value_style,
                    ),
                    Span::styled(
                        format!(
                            " · {}",
                            selected.map_or("unknown", |item| item.status.as_str())
                        ),
                        label_style,
                    ),
                ])
            }
        }
    };
    frame.render_widget(Paragraph::new(line), area);
}

fn render_list(frame: &mut Frame<'_>, area: Rect, workspace: &Workspace) {
    if area.height == 0 {
        return;
    }
    let visible = area.height as usize;
    let (lines, selected) = match workspace.page {
        Page::Setup => (
            vec![
                "TorBox uses an API token, not your account password.",
                "Keep the token in a protected file (agenix is recommended).",
                "Wim stores only the file path in its config; never the token.",
                "Enter tests the connection with a read-only request.",
                "A failure behind Mullvad does not prove the token is bad.",
            ]
            .into_iter()
            .map(|text| {
                Line::from(Span::styled(
                    text,
                    Style::default().fg(crate::theme::shade(crate::theme::MID)),
                ))
            })
            .collect::<Vec<_>>(),
            usize::MAX,
        ),
        Page::Search => rows_for_items(&workspace.hits, workspace.selected, area.width),
        Page::Account => rows_for_items(&workspace.account, workspace.selected, area.width),
        Page::Files => match workspace.account_selected.as_ref() {
            Some(item) => (
                item.playable_files_for_display()
                    .iter()
                    .map(|(name, size, video)| {
                        let details = format!(
                            "[{}] · {}",
                            if *video { "video" } else { "audio" },
                            format_size(*size)
                        );
                        fit_row("  ", name, &details, area.width, false)
                    })
                    .collect(),
                usize::MAX,
            ),
            None => (Vec::new(), usize::MAX),
        },
    };
    let list: Vec<Line<'static>> = if lines.is_empty() {
        vec![Line::from(Span::styled(
            workspace
                .message
                .as_deref()
                .unwrap_or("nothing here yet")
                .to_string(),
            Style::default().fg(crate::theme::shade(crate::theme::DIM)),
        ))]
    } else {
        let offset =
            crate::ui::scroll::offset_for(selected.min(lines.len().saturating_sub(1)), visible);
        let _ = selected;
        lines.into_iter().skip(offset).take(visible).collect()
    };
    frame.render_widget(Paragraph::new(list), area);
}

/// One bounded row per account item: full-width plate on the selected row,
/// a truncated name, and dim secondary details. Rows never wrap.
fn rows_for_items(
    items: &[AccountItem],
    selected: usize,
    width: u16,
) -> (Vec<Line<'static>>, usize) {
    let rows = items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let marker = if i == selected { "> " } else { "  " };
            let progress = item
                .progress
                .map_or(String::new(), |p| format!(" {:.0}%", p * 100.0));
            let details = format!(
                "{}{} · {} · {} files",
                item.status,
                progress,
                format_size(item.total_size()),
                item.files.len()
            );
            fit_row(marker, &item.name, &details, width, i == selected)
        })
        .collect();
    (rows, selected)
}

/// Build one non-wrapping row: `marker`, the name truncated to what is left
/// after `details`, then `details`. When selected, the whole row — including
/// padding to the pane edge — carries the selection background.
fn fit_row(marker: &str, name: &str, details: &str, width: u16, selected: bool) -> Line<'static> {
    let budget = width as usize;
    let marker_width = crate::ui::textfit::width(marker);
    let details_width = crate::ui::textfit::width(details);
    let plate = |style: Style| {
        if selected {
            style.bg(crate::theme::shade(SELECT_BG))
        } else {
            style
        }
    };
    let marker_style = plate(Style::default().fg(crate::theme::shade(crate::theme::MID)));
    let name_style = plate(
        Style::default()
            .fg(crate::theme::shade(crate::theme::NEAR_WHITE))
            .add_modifier(if selected {
                Modifier::BOLD
            } else {
                Modifier::empty()
            }),
    );
    let details_style = plate(Style::default().fg(crate::theme::shade(crate::theme::DIM)));
    let (name, shown_details) = if marker_width + details_width + 1 >= budget {
        (
            crate::ui::textfit::truncate_to_width(name, budget.saturating_sub(marker_width)),
            "",
        )
    } else {
        (
            crate::ui::textfit::truncate_to_width(name, budget - marker_width - details_width - 1),
            details,
        )
    };
    let mut spans = vec![
        Span::styled(marker.to_string(), marker_style),
        Span::styled(name.clone(), name_style),
    ];
    let mut used = marker_width + crate::ui::textfit::width(&name);
    if !shown_details.is_empty() {
        spans.push(Span::styled(" ".to_string(), plate(Style::default())));
        spans.push(Span::styled(shown_details.to_string(), details_style));
        used += 1 + details_width;
    }
    if selected {
        let pad = budget.saturating_sub(used);
        if pad > 0 {
            spans.push(Span::styled(" ".repeat(pad), name_style));
        }
    }
    Line::from(spans)
}

fn render_status(frame: &mut Frame<'_>, area: Rect, workspace: &Workspace) {
    let text = workspace
        .message
        .clone()
        .unwrap_or_else(|| match workspace.page {
            Page::Search if !workspace.search_editing && workspace.hits.is_empty() => {
                "press / to type a query; results come from your own library".to_string()
            }
            _ => String::new(),
        });
    let style = if workspace.loading {
        Style::default().fg(crate::theme::shade(crate::theme::MID))
    } else {
        Style::default().fg(crate::theme::shade(crate::theme::DIM))
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            crate::ui::textfit::truncate_to_width(&text, area.width as usize),
            style,
        ))),
        area,
    );
}

fn render_footer(frame: &mut Frame<'_>, area: Rect, workspace: &Workspace) {
    let mut footer = match workspace.page {
        Page::Setup => "type path · enter verify and save · esc cancel".to_string(),
        Page::Search => {
            if workspace.search_editing {
                "type · enter search · esc stop editing".to_string()
            } else {
                "j/k select · enter files · / edit query · b account · c connect · esc close"
                    .to_string()
            }
        }
        Page::Account => "1/2/3 service · r refresh · n next page · s search · enter files · + add link · p pause/resume · R reannounce · d delete · esc close".to_string(),
        Page::Files => "j/k select · enter play · q queue · backspace back · esc close".to_string(),
    };
    if workspace.loading {
        footer.push_str(" · working…");
    }
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            crate::ui::textfit::truncate_to_width(&footer, area.width as usize),
            Style::default().fg(crate::theme::shade(crate::theme::DIM)),
        ))),
        area,
    );
}

fn format_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit + 1 < UNITS.len() {
        size /= 1024.0;
        unit += 1;
    }
    format!("{size:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::torbox::AccountFile;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn item(id: u64, name: &str, files: Vec<AccountFile>) -> AccountItem {
        AccountItem {
            service: crate::torbox::Service::Torrent,
            id,
            name: name.to_string(),
            status: "cached".to_string(),
            progress: Some(1.0),
            files,
        }
    }

    fn rendered(width: u16, height: u16, workspace: &Workspace) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), workspace))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn row_text(buf: &Buffer, y: u16) -> String {
        (0..buf.area.width)
            .map(|x| buf[(x, y)].symbol().to_string())
            .collect()
    }

    fn bg_at(buf: &Buffer, x: u16, y: u16) -> Option<ratatui::style::Color> {
        buf[(x, y)].style().bg
    }

    #[test]
    fn overlay_renders_at_normal_size_and_declines_tiny_areas() {
        let workspace = Workspace::new(Err("not configured".into()));
        for (width, height) in [(80, 24), (36, 10), (80, 9), (1, 1)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| render(frame, frame.area(), &workspace))
                .unwrap();
        }
    }

    #[test]
    fn the_selected_row_gets_a_full_width_plate() {
        let mut workspace = Workspace::new(Err("not configured".into()));
        workspace.hits = vec![
            item(1, "first", vec![]),
            item(2, "second", vec![]),
            item(3, "third", vec![]),
        ];
        workspace.selected = 1;
        let buf = rendered(60, 20, &workspace);
        // The panel is centred; find the list rows by locating the markers.
        let mut list_top = None;
        for y in 0..buf.area.height {
            if row_text(&buf, y).contains("  first") {
                list_top = Some(y);
                break;
            }
        }
        let list_top = list_top.expect("list rows rendered");
        // Selected row is plated edge to edge; unselected rows are not.
        let plate = crate::theme::shade(SELECT_BG);
        assert_eq!(bg_at(&buf, 2, list_top + 1), Some(plate));
        assert_eq!(bg_at(&buf, 55, list_top + 1), Some(plate));
        assert_ne!(bg_at(&buf, 2, list_top), Some(plate));
        assert_ne!(bg_at(&buf, 55, list_top + 2), Some(plate));
    }

    #[test]
    fn long_names_are_truncated_within_their_row_not_wrapped() {
        let mut workspace = Workspace::new(Err("not configured".into()));
        let long = "Silo.S03.2160p.ATVP.WEB-DL.DV.HDR10+.DDP5.1.Atmos.H265.BTM".repeat(3);
        workspace.hits = vec![item(1, &long, vec![]), item(2, "second", vec![])];
        let buf = rendered(60, 20, &workspace);
        let mut first_row = None;
        for y in 0..buf.area.height {
            if row_text(&buf, y).contains("Silo.S03") {
                first_row = Some(y);
                break;
            }
        }
        let first_row = first_row.expect("long row rendered");
        let second = row_text(&buf, first_row + 1);
        assert!(
            second.contains("second"),
            "next item on its own row: {second:?}"
        );
    }

    #[test]
    fn the_list_scrolls_to_keep_the_selection_visible() {
        let mut workspace = Workspace::new(Err("not configured".into()));
        workspace.hits = (0..30)
            .map(|i| item(i, &format!("title-{i}"), vec![]))
            .collect();
        workspace.selected = 29;
        let buf = rendered(60, 20, &workspace);
        let text: String = (0..buf.area.height).map(|y| row_text(&buf, y)).collect();
        assert!(text.contains("title-29"), "selection drawn: {text:?}");
        assert!(!text.contains("title-0 "), "off-screen rows are not drawn");
    }

    #[test]
    fn empty_results_show_the_message_not_a_blank_wall() {
        let mut workspace = Workspace::new(Err("not configured".into()));
        workspace.message = Some("no library matches".into());
        let buf = rendered(60, 20, &workspace);
        let text: String = (0..buf.area.height).map(|y| row_text(&buf, y)).collect();
        assert!(text.contains("no library matches"), "{text:?}");
    }
}
