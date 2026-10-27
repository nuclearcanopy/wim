mod app;
mod config;
mod eq;
mod library;
mod lyrics;
mod mpv;
mod persistence;
mod search;
mod subsonic;
mod term;
mod theme;
mod ui;

use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode};
use ratatui::layout::{Constraint, Layout};

use app::Action;
use search::SearchItem;
use ui::banner::Banner;
use ui::login::{LoginForm, LoginView};
use ui::lyrics_view::LyricsView;
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

/// (artist, title, album) for a lyrics lookup — only meaningful for
/// local music; `SearchItem` doesn't carry duration, so lookups always
/// fall through to lrclib's fuzzy `/search` rather than its exact
/// `/get`. See ROADMAP.md.
fn lyrics_query(item: &SearchItem) -> Option<(String, String, String)> {
    match item {
        SearchItem::LocalMusic { title, artist, album, .. } => Some((
            artist.clone().unwrap_or_default(),
            title.clone(),
            album.clone().unwrap_or_default(),
        )),
        _ => None,
    }
}

/// Replaces mpv's whole playlist and plays `item` immediately. Returns
/// the status line and whether it actually started playing, so the
/// caller can update `now_playing` only on success.
fn try_play(player: &mpv::Mpv, item: &SearchItem) -> (String, bool) {
    match playable_path(item) {
        Some(path) => match player.load_file(&path) {
            Ok(()) => (format!("playing: {}", label(item)), true),
            Err(e) => (format!("error loading file: {e}"), false),
        },
        None => (
            "not playable yet — remote items aren't wired into playback this round".to_string(),
            false,
        ),
    }
}

/// Appends `item` to mpv's own playlist (plays immediately if nothing
/// was already loaded, otherwise queues gaplessly — mpv auto-advances
/// its own playlist on end-of-file with none of the round-trip gap that
/// reactively reloading on our end would introduce).
fn try_enqueue(player: &mpv::Mpv, item: &SearchItem) -> String {
    match playable_path(item) {
        Some(path) => match player.playlist_append(&path) {
            Ok(()) => format!("queued: {}", label(item)),
            Err(e) => format!("error queuing file: {e}"),
        },
        None => "not playable yet — remote items aren't wired into playback this round".to_string(),
    }
}

/// Resyncs mpv's upcoming playlist to `items` (e.g. after a shuffle)
/// without disturbing whatever's currently playing — mpv's own
/// `playlist-clear` semantics already preserve the current file.
fn resync_queue(player: &mpv::Mpv, items: &[SearchItem]) -> String {
    if let Err(e) = player.playlist_clear() {
        return format!("error resyncing queue: {e}");
    }
    for item in items {
        if let Some(path) = playable_path(item) {
            if let Err(e) = player.playlist_append(&path) {
                return format!("error resyncing queue: {e}");
            }
        }
    }
    "shuffled".to_string()
}

/// Snapshots now-playing/queue/position/volume for next launch to
/// resume. Best-effort: a failed save just means nothing to resume
/// next time, not worth surfacing since we're already quitting.
fn save_playback_state(player: &mpv::Mpv, now_playing: &Option<SearchItem>, queue: &[SearchItem]) {
    let state = persistence::PlaybackState {
        now_playing_path: now_playing.as_ref().and_then(playable_path),
        queue_paths: queue.iter().filter_map(playable_path).collect(),
        position_secs: player.get_property("time-pos").ok().and_then(|v| v.as_f64()),
        volume: player.get_property("volume").ok().and_then(|v| v.as_f64()),
    };
    let _ = persistence::save(&state);
}

/// What the results list should say instead of rendering nothing when
/// there's nothing to show — a blank screen with no explanation is a
/// dead end, never the right default.
fn empty_results_message(app: &app::App) -> String {
    if app.corpus.is_empty() {
        "no local music/video found — check library_paths in config, or set up navidrome (n)"
            .to_string()
    } else if app.editing_query && !app.query.is_empty() {
        format!("no matches for \"{}\"", app.query)
    } else {
        format!("{} items indexed — press / to search", app.corpus.len())
    }
}

fn main() -> anyhow::Result<()> {
    let mut cfg = config::load_or_init()?;

    let tracks = library::scan(&cfg.library_paths);
    let mut corpus = search::Corpus::new();
    corpus.add_local_tracks(tracks);

    let player = mpv::Mpv::spawn()?;
    // per-track normalization is the sensible default; mpv reads the
    // gain from each file's own embedded tags as it plays.
    player.set_replaygain("track")?;
    let mut app = app::App::new(corpus);
    let mut status = format!(
        "{} local items indexed — / search, j/k move, gg/G jump, enter play, a queue, L lyrics, n navidrome, q quit",
        app.corpus.len()
    );

    let mut now_playing: Option<SearchItem> = None;
    let lyrics_client = lyrics::Client::new();
    let mut lyrics_lines: Option<Vec<String>> = None;
    let mut lyrics_scroll: usize = 0;

    // shown automatically on first run instead of requiring a
    // hand-edited config.toml before the app is even usable; `n`
    // reopens it later too.
    let mut login_form: Option<LoginForm> = if cfg.navidrome.is_none() { Some(LoginForm::new()) } else { None };
    let mut login_message = String::new();

    // resuming across restarts is standard in any serious media player;
    // a missing/corrupt state file just means nothing to resume, not an
    // error — see persistence.rs.
    if let Some(saved) = persistence::load() {
        if let Some(item) = saved.now_playing_path.as_deref().and_then(persistence::item_from_path) {
            if try_play(&player, &item).1 {
                if let Some(pos) = saved.position_secs {
                    let _ = player.seek(pos, false);
                }
                now_playing = Some(item);
            }
        }
        for queued_path in &saved.queue_paths {
            if let Some(item) = persistence::item_from_path(queued_path) {
                let _ = try_enqueue(&player, &item);
                app.queue.push(item);
            }
        }
        if let Some(vol) = saved.volume {
            let _ = player.set_volume(vol);
        }
    }

    let start = Instant::now();
    let mut guard = term::TerminalGuard::new()?;
    loop {
        guard.terminal.draw(|f| {
            let area = f.area();
            let chunks = Layout::vertical([
                Constraint::Min(0),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(area);
            f.render_widget(ui::divider::HorizontalRule, chunks[1]);

            if let Some(form) = &login_form {
                f.render_widget(LoginView { form, message: &login_message }, chunks[0]);
            } else {
                match &lyrics_lines {
                    Some(lines) => {
                        let borrowed: Vec<&str> = lines.iter().map(String::as_str).collect();
                        f.render_widget(LyricsView { lines: &borrowed, scroll: lyrics_scroll }, chunks[0]);
                    }
                    None if app.results.is_empty() => {
                        // nothing to show — the animated title card, not
                        // a blank void. splitting leaves one line at the
                        // bottom of the content area for the contextual
                        // hint (why it's empty / what to do next).
                        let idle_chunks =
                            Layout::vertical([Constraint::Min(0), Constraint::Length(1)])
                                .split(chunks[0]);
                        let elapsed_secs = start.elapsed().as_secs_f32();
                        let phase = elapsed_secs * 0.25;
                        f.render_widget(Banner { phase, elapsed_secs }, idle_chunks[0]);
                        f.render_widget(
                            StatusBar {
                                editing_query: app.editing_query,
                                query: &app.query,
                                message: &empty_results_message(&app),
                            },
                            idle_chunks[1],
                        );
                    }
                    None => {
                        f.render_widget(
                            ResultsList {
                                items: &app.results,
                                selected: app.selected,
                                empty_message: &empty_results_message(&app),
                            },
                            chunks[0],
                        );
                    }
                }
            }

            let status_line = format!(
                "[queue:{} loop:{} shuffle:{} speed:{:.2}x eq:{}] {status}",
                app.queue.len(),
                if app.loop_file { "on" } else { "off" },
                if app.shuffle { "on" } else { "off" },
                app.speed,
                eq::PRESETS[app.eq_preset].name,
            );
            f.render_widget(
                StatusBar { editing_query: app.editing_query, query: &app.query, message: &status_line },
                chunks[2],
            );
        })?;

        // non-blocking: mpv auto-advances its own playlist on
        // end-of-file (gaplessly) — this just keeps our queue *view* in
        // sync with what mpv already moved past, nothing to load here.
        if let Ok(mpv::Event::EndFile) = player.events().try_recv() {
            status = match app.pop_queue() {
                Some(next) => {
                    let s = format!("now playing: {}", label(&next));
                    now_playing = Some(next);
                    s
                }
                None => {
                    now_playing = None;
                    "queue empty — idle".to_string()
                }
            };
        }

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if let Some(form) = &mut login_form {
                    match key.code {
                        KeyCode::Esc => {
                            login_form = None;
                            status = "navidrome setup skipped — local files only".to_string();
                        }
                        KeyCode::Char(c) => form.current_mut().push(c),
                        KeyCode::Backspace => {
                            form.current_mut().pop();
                        }
                        KeyCode::Tab => form.next_field(),
                        KeyCode::Enter if !form.is_last_field() => form.next_field(),
                        KeyCode::Enter => {
                            let nav = config::NavidromeConfig {
                                url: form.url.clone(),
                                username: form.username.clone(),
                                password: Some(form.password.clone()),
                                password_file: None,
                            };
                            match subsonic::Client::new(&nav).and_then(|c| c.ping().map(|_| c)) {
                                Ok(client) => {
                                    cfg.navidrome = Some(nav);
                                    let _ = config::save(&cfg);
                                    if let Ok(playlists) = client.playlists() {
                                        app.corpus.add_playlists(playlists);
                                    }
                                    status = "navidrome connected and saved".to_string();
                                    login_form = None;
                                }
                                Err(e) => {
                                    login_message = format!("login failed: {e} — esc to skip");
                                }
                            }
                        }
                        _ => {}
                    }
                    continue;
                }

                // lyrics view intercepts input directly rather than going
                // through App, since scrolling it shouldn't move the
                // underlying results selection.
                if lyrics_lines.is_some() {
                    match key.code {
                        KeyCode::Char('j') | KeyCode::Down => lyrics_scroll = lyrics_scroll.saturating_add(1),
                        KeyCode::Char('k') | KeyCode::Up => lyrics_scroll = lyrics_scroll.saturating_sub(1),
                        KeyCode::Char('L') | KeyCode::Esc | KeyCode::Char('q') => {
                            lyrics_lines = None;
                            lyrics_scroll = 0;
                        }
                        _ => {}
                    }
                    continue;
                }

                if key.code == KeyCode::Char('n') {
                    login_form = Some(LoginForm::new());
                    login_message.clear();
                    continue;
                }

                if key.code == KeyCode::Char('L') {
                    status = match &now_playing {
                        Some(item) => match lyrics_query(item) {
                            Some((artist, title, album)) => {
                                match lyrics_client.lookup(&artist, &title, &album, None) {
                                    Ok(Some(l)) => {
                                        let text = l
                                            .plain
                                            .or(l.synced)
                                            .unwrap_or_else(|| "(instrumental — no lyrics)".to_string());
                                        lyrics_lines =
                                            Some(text.lines().map(str::to_string).collect());
                                        "lyrics loaded (j/k scroll, L/esc close)".to_string()
                                    }
                                    Ok(None) => "no lyrics found".to_string(),
                                    Err(e) => format!("lyrics lookup failed: {e}"),
                                }
                            }
                            None => "no lyrics for this item".to_string(),
                        },
                        None => "nothing playing".to_string(),
                    };
                    continue;
                }

                match app.handle_key(key.code) {
                    Action::Quit => {
                        save_playback_state(&player, &now_playing, &app.queue);
                        break;
                    }
                    Action::Play(item) => {
                        let (s, played) = try_play(&player, &item);
                        if played {
                            now_playing = Some(item);
                        }
                        status = s;
                    }
                    Action::Queued(item) => status = try_enqueue(&player, &item),
                    Action::QueueReordered(items) => status = resync_queue(&player, &items),
                    Action::SetLoop(on) => {
                        status = match player.set_loop_file(if on { "inf" } else { "no" }) {
                            Ok(()) => format!("loop: {}", if on { "on" } else { "off" }),
                            Err(e) => format!("error setting loop: {e}"),
                        };
                    }
                    Action::SetSpeed(speed) => {
                        status = match player.set_speed(speed) {
                            Ok(()) => format!("speed: {speed:.2}x"),
                            Err(e) => format!("error setting speed: {e}"),
                        };
                    }
                    Action::SetEq(name, gains) => {
                        let filter = eq::build_af_filter(&gains);
                        status = match player.set_af(&filter) {
                            Ok(()) => format!("eq: {name}"),
                            Err(e) => format!("error setting eq: {e}"),
                        };
                    }
                    Action::None => {}
                }
            }
        }
    }
    drop(guard);

    Ok(())
}
