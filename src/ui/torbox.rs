use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Widget;
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

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

pub fn render(frame: &mut Frame<'_>, area: Rect, workspace: &Workspace) {
    if area.width < 36 || area.height < 10 {
        return;
    }
    let width = area.width.min(96);
    let height = area.height.min(24);
    let panel = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, panel);
    let title = match workspace.page {
        Page::Setup => "connect TorBox · API token file",
        Page::Search => "torbox search · online query is sent to TorBox",
        Page::Releases => "torrent sources · enter = sources, a = add selected",
        Page::Account => "torbox account · 1 torrents · 2 usenet · 3 web downloads",
        Page::Files => "account files · enter = play · q = queue · backspace = account",
    };
    let inner = Block::default().borders(Borders::ALL).title(title);
    let content = inner.inner(panel);
    frame.render_widget(inner, panel);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(2),
        ])
        .split(content);

    let query_text = match workspace.page {
        Page::Search => Line::from(vec![
            Span::styled("query: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(&workspace.query),
        ]),
        Page::Setup => Line::from(vec![
            Span::styled(
                "token file: ",
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(&workspace.token_path_input),
        ]),
        _ if workspace.link_input.is_some() => Line::from(format!(
            "add link: {}",
            workspace.link_input.as_deref().unwrap_or_default()
        )),
        Page::Account => Line::from(format!("service: {}", workspace.service.as_str())),
        Page::Files => Line::from(format!(
            "{} · {}",
            workspace
                .account_selected
                .as_ref()
                .map_or("item", |item| item.name.as_str()),
            workspace
                .account_selected
                .as_ref()
                .map_or("unknown", |item| item.status.as_str())
        )),
        Page::Releases => Line::from(format!("query: {}", workspace.query)),
    };
    let query = Paragraph::new(query_text);
    frame.render_widget(query, chunks[0]);

    let lines: Vec<Line<'_>> = match workspace.page {
        Page::Setup => vec![
            Line::from("TorBox uses an API token, not your account password."),
            Line::from("Keep the token in a protected file (agenix is recommended)."),
            Line::from("Wim stores only this file path in its config; never the token."),
            Line::from("Enter tests the connection before saving the path."),
            Line::from("If connection fails behind Mullvad, see docs/torbox.md."),
        ],
        Page::Search => workspace
            .hits
            .iter()
            .enumerate()
            .map(|(i, hit)| {
                Line::from(format!(
                    "{} {} [{}:{}] {}",
                    if i == workspace.selected { ">" } else { " " },
                    hit.title,
                    hit.source,
                    hit.id,
                    hit.media_type.as_deref().unwrap_or("metadata")
                ))
            })
            .collect(),
        Page::Releases => workspace
            .releases
            .iter()
            .enumerate()
            .map(|(i, release)| {
                let size = if release.size == 0 {
                    "size unknown".into()
                } else {
                    format_size(release.size)
                };
                let seeders = release
                    .seeders
                    .map_or("seeders ?".into(), |n| format!("{n} seeders"));
                let cached = match release.cached {
                    Some(true) => "cached",
                    Some(false) => "not cached",
                    None => "cache unknown",
                };
                Line::from(format!(
                    "{} {} · {} · {} · {}",
                    if i == workspace.selected { ">" } else { " " },
                    release.name,
                    size,
                    seeders,
                    cached
                ))
            })
            .collect(),
        Page::Account => workspace
            .account
            .iter()
            .enumerate()
            .map(|(i, item)| {
                let progress = item
                    .progress
                    .map_or(String::new(), |p| format!(" · {:.0}%", p * 100.0));
                Line::from(format!(
                    "{} {} · {}{} · {} files",
                    if i == workspace.selected { ">" } else { " " },
                    item.name,
                    item.status,
                    progress,
                    item.files.len()
                ))
            })
            .collect(),
        Page::Files => workspace
            .account_selected
            .as_ref()
            .map(|item| item.playable_files_for_display())
            .unwrap_or_default()
            .into_iter()
            .enumerate()
            .map(|(i, (name, size, video))| {
                Line::from(format!(
                    "{} [{}] {} · {}",
                    if i == workspace.selected { ">" } else { " " },
                    if video { "video" } else { "audio" },
                    name,
                    format_size(size)
                ))
            })
            .collect(),
    };
    let list = if lines.is_empty() {
        Paragraph::new(
            workspace
                .message
                .as_deref()
                .unwrap_or("no results")
                .to_string(),
        )
    } else {
        Paragraph::new(lines)
    }
    .wrap(Wrap { trim: true });
    frame.render_widget(list, chunks[1]);

    let mut footer = workspace
        .message
        .clone()
        .unwrap_or_else(|| match workspace.page {
            Page::Setup => "type or paste token-file path · enter verify/save · esc cancel".into(),
            Page::Search => if workspace.search_editing { "type query · enter search · esc stop editing · c connect · b account".into() } else { "j/k select · enter sources · / edit query · b account · c connect · esc close".into() },
            Page::Releases => "j/k select · enter back to search · a add magnet · esc close".into(),
            Page::Account => "1/2/3 service · r refresh · n next page · s search · enter files · + add · p pause/resume · R reannounce · d delete · esc close".into(),
            Page::Files => "j/k select · enter play · q queue · backspace list · esc close".into(),
        });
    if workspace.loading {
        footer = format!("{footer} · working…");
    }
    frame.render_widget(Paragraph::new(footer).wrap(Wrap { trim: true }), chunks[2]);
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
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[test]
    fn overlay_renders_at_normal_size_and_declines_tiny_areas() {
        let workspace = Workspace::new(Err("not configured".into()));
        for (width, height) in [(80, 24), (35, 24), (80, 9), (1, 1)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| render(frame, frame.area(), &workspace))
                .unwrap();
        }
    }
}
