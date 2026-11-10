use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::media::MediaKind;
use crate::watch_workspace::{Page, Workspace};

pub fn render(frame: &mut Frame<'_>, area: Rect, workspace: &Workspace) {
    if area.width < 32 || area.height < 8 {
        return;
    }
    let width = area.width.min(100);
    let height = area.height.min(26);
    let panel = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, panel);
    let spoiler = if workspace.spoilers_revealed {
        "spoilers revealed"
    } else {
        "spoilers hidden · press r to reveal"
    };
    let title = match workspace.page {
        Page::Search => "watch · online search (separate from your music library)",
        Page::Detail => "watch · details",
        Page::Episodes => "watch · episodes",
        Page::Sources => "watch · sources",
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let content = block.inner(panel);
    frame.render_widget(block, panel);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(2),
        ])
        .split(content);

    let query = if workspace.page == Page::Search {
        Line::from(vec![
            Span::styled("search: ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(&workspace.query),
            Span::raw(if workspace.search_editing { "_" } else { "" }),
        ])
    } else {
        Line::from(
            workspace
                .detail
                .as_ref()
                .map_or("No title selected".to_owned(), |item| {
                    let year = item.year.map_or(String::new(), |year| format!(" ({year})"));
                    format!(
                        "{}{} · {}",
                        item.title,
                        year,
                        if item.kind == MediaKind::Series {
                            "series"
                        } else {
                            "movie"
                        }
                    )
                }),
        )
    };
    let privacy = Line::from(
        "Configured provider receives your query/media IDs and can see your IP address.",
    );
    frame.render_widget(Paragraph::new(vec![query, privacy]), chunks[0]);

    let rows: Vec<Line<'_>> = match workspace.page {
        Page::Search => workspace
            .results
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let cursor = if index == workspace.selected {
                    ">"
                } else {
                    " "
                };
                let kind = if item.kind == MediaKind::Series {
                    "Series"
                } else {
                    "Movie"
                };
                let year = item.year.map_or(String::new(), |year| format!(" · {year}"));
                Line::from(format!("{cursor} {kind} · {}{year}", item.title))
            })
            .collect(),
        Page::Detail => workspace
            .detail
            .as_ref()
            .map(|item| {
                let mut lines = vec![Line::from(format!(
                    "{} · {}",
                    item.title,
                    if item.kind == MediaKind::Series {
                        "series"
                    } else {
                        "movie"
                    }
                ))];
                if !item.genres.is_empty() {
                    lines.push(Line::from(item.genres.join(" · ")));
                }
                if let Some(description) = &item.description {
                    lines.push(Line::from(description.as_str()));
                }
                if let Some(poster) = &item.poster {
                    lines.push(Line::from(format!("poster: {poster}")));
                }
                lines
            })
            .unwrap_or_default(),
        Page::Episodes => {
            let mut lines = Vec::new();
            if let Some(season) = workspace.seasons.get(workspace.season_selected) {
                lines.push(Line::from(season.number.map_or_else(
                    || "Season · numbering unavailable".into(),
                    |number| format!("Season {number}"),
                )));
                for (index, episode) in season.episodes.iter().enumerate() {
                    lines.push(Line::from(format!(
                        "{} {}",
                        if index == workspace.selected {
                            ">"
                        } else {
                            " "
                        },
                        episode.display_label(workspace.spoiler_policy())
                    )));
                    if index == workspace.selected {
                        if let Some(overview) = episode.display_overview(workspace.spoiler_policy())
                        {
                            lines.push(Line::from(format!("  {overview}")));
                        }
                    }
                }
            }
            lines
        }
        Page::Sources => workspace
            .sources
            .iter()
            .enumerate()
            .map(|(index, stream)| {
                let cursor = if index == workspace.selected {
                    ">"
                } else {
                    " "
                };
                let transport = if stream
                    .playable_source("provider", &format!("candidate-{index}"))
                    .is_some()
                {
                    "direct HTTP(S)"
                } else if stream.info_hash.is_some() {
                    "torrent · not yet playable here"
                } else if stream.yt_id.is_some() {
                    "YouTube · not yet playable here"
                } else {
                    "unsupported transport"
                };
                Line::from(format!(
                    "{cursor} {} · {transport}",
                    workspace.source_label(index)
                ))
            })
            .collect(),
    };
    let body = if rows.is_empty() {
        Paragraph::new(
            workspace
                .message
                .as_deref()
                .unwrap_or(if workspace.loading {
                    "Loading…"
                } else {
                    "No results"
                })
                .to_owned(),
        )
    } else {
        Paragraph::new(rows)
    }
    .wrap(Wrap { trim: true });
    frame.render_widget(body, chunks[1]);

    let navigation = match workspace.page {
        Page::Search => "enter search/open · / edit · j/k move · esc close",
        Page::Detail
            if workspace
                .detail
                .as_ref()
                .is_some_and(|item| item.kind == MediaKind::Series) =>
        {
            "enter seasons · backspace results · r reveal/hide"
        }
        Page::Detail => "enter sources · backspace results · r reveal/hide",
        Page::Episodes => "enter sources · [/] season · backspace details · r reveal/hide",
        Page::Sources => "enter checks source, then plays · backspace episodes/details",
    };
    let message = workspace
        .message
        .as_deref()
        .unwrap_or(if workspace.loading { "Working…" } else { "" });
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(navigation),
            Line::from(format!("{spoiler} · {message}")),
        ])
        .wrap(Wrap { trim: true }),
        chunks[2],
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{Episode, MediaItem, Season};
    use crate::stremio::Stream;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[test]
    fn episode_and_source_titles_do_not_leak_until_revealed() {
        let mut workspace = Workspace::new();
        workspace.detail = Some(MediaItem {
            id: "tt1".into(),
            kind: MediaKind::Series,
            title: "Fixture".into(),
            year: None,
            genres: vec![],
            poster: None,
            background: None,
            description: None,
        });
        workspace.seasons = vec![Season {
            number: Some(1),
            episodes: vec![Episode {
                id: "tt1:1:1".into(),
                season: Some(1),
                number: Some(1),
                title: Some("UNIQUE SPOILER PHRASE".into()),
                overview: Some("ANOTHER SPOILER PHRASE".into()),
                thumbnail: Some("https://img.invalid/THUMBNAIL-SPOILER".into()),
                air_date: None,
            }],
        }];
        workspace.sources = vec![Stream {
            name: Some("UNIQUE SPOILER PHRASE source".into()),
            title: None,
            url: Some("https://cdn.invalid/signed".into()),
            info_hash: None,
            file_idx: None,
            yt_id: None,
        }];
        for page in [Page::Episodes, Page::Sources] {
            workspace.page = page;
            let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
            terminal
                .draw(|frame| render(frame, frame.area(), &workspace))
                .unwrap();
            let text = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(!text.contains("UNIQUE SPOILER PHRASE"), "{text}");
            assert!(!text.contains("ANOTHER SPOILER PHRASE"), "{text}");
            assert!(!text.contains("THUMBNAIL-SPOILER"), "{text}");
        }
        workspace.page = Page::Episodes;
        workspace.spoilers_revealed = true;
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| render(frame, frame.area(), &workspace))
            .unwrap();
        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("UNIQUE SPOILER PHRASE"), "{text}");
        assert!(text.contains("ANOTHER SPOILER PHRASE"), "{text}");
    }
}
