mod app;
mod config;
mod library;
mod mpv;
mod search;
mod subsonic;
mod term;
mod theme;
mod ui;

use std::time::Duration;

use crossterm::event::{self, Event};
use ratatui::layout::{Constraint, Layout};

use app::Action;
use search::SearchItem;
use ui::results_list::{label, ResultsList};
use ui::status::StatusBar;

/// Local items resolve straight to a file path mpv can load. Remote
/// (subsonic) items aren't wired into playback yet — the live loop only
/// has a local corpus this round; see ROADMAP.md.
fn playable_path(item: &SearchItem) -> Option<String> {
    match item {
        SearchItem::LocalMusic { path, .. } | SearchItem::LocalVideo { path, .. } => {
            Some(path.to_string_lossy().into_owned())
        }
        _ => None,
    }
}

fn main() -> anyhow::Result<()> {
    let cfg = config::load_or_init()?;

    let tracks = library::scan(&cfg.library_paths);
    let mut corpus = search::Corpus::new();
    corpus.add_local_tracks(tracks);

    let player = mpv::Mpv::spawn()?;
    let mut app = app::App::new(corpus);
    let mut status = format!(
        "{} local items indexed — / to search, j/k move, gg/G jump, enter play, q quit",
        app.corpus.len()
    );

    let mut guard = term::TerminalGuard::new()?;
    loop {
        guard.terminal.draw(|f| {
            let area = f.area();
            let chunks =
                Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);
            f.render_widget(
                ResultsList { items: &app.results, selected: app.selected },
                chunks[0],
            );
            f.render_widget(
                StatusBar { editing_query: app.editing_query, query: &app.query, message: &status },
                chunks[1],
            );
        })?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                match app.handle_key(key.code) {
                    Action::Quit => break,
                    Action::Play(item) => match playable_path(&item) {
                        Some(path) => match player.load_file(&path) {
                            Ok(()) => status = format!("playing: {}", label(&item)),
                            Err(e) => status = format!("error loading file: {e}"),
                        },
                        None => {
                            status =
                                "not playable yet — remote items aren't wired into playback this round"
                                    .to_string();
                        }
                    },
                    Action::None => {}
                }
            }
        }
    }
    drop(guard);

    Ok(())
}
