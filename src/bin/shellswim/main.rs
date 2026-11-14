//! `shellswim`: wim's player with nothing but the prompt. A
//! full-screen shellsong console, the playing track and its scrubber;
//! no panes, no sidebar, no results list. Everything is a command.
//!
//! It is a second front-end over the same `wim` library, which is the
//! point: the interpreter, the op table and the player are shared, and
//! all that lives here is presentation plus a `Surface` that declines
//! what a shell cannot show. See docs/library-seam.md and
//! docs/shellswim.md.

use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Layout};
use ratatui::style::Style;
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use wim::core::{self, Drive};
use wim::eq_menu_keys::{adjust_eq_band, eq_menu_editor_key, eq_saved_row_hint, persist_eq};
use wim::keymap::{key_label, KeyAction, Keymap, ALL_ACTIONS};
use wim::playqueue::PlayQueue;
use wim::search::SearchItem;
use wim::shellsong::{Console, ConsoleAction, LineKind};
use wim::swimconsole::{SwimBar, SwimConsole};
use wim::theme::{shade, DIM, NEAR_WHITE};
#[cfg(feature = "dev-telemetry")]
use wim::trace;
use wim::ui::divider::HorizontalRule;
use wim::ui::eq_menu as eqm;
use wim::ui::lyrics_view::LyricsView;
use wim::ui::results_list::label;
use wim::{anim, config, mpv, term};

use wim::cli;
mod host;
mod surface;

use cli::{Cli, Run};
use host::Host;

const BUILD: &str = env!("WIM_BUILD");

fn main() -> anyhow::Result<()> {
    // answered before anything else, so both work with no terminal, no
    // config and no server; `--version` exists to settle "am I running
    // the build I think I am".
    let cli = match Cli::parse(std::env::args().skip(1)) {
        Ok(cli) => cli,
        Err(why) => {
            eprintln!("shellswim: {why}\n{}", cli::USAGE);
            std::process::exit(2);
        }
    };
    let _playback_files = wim::cache::PlaybackFiles::new();
    match &cli.run {
        Run::Version => {
            println!("shellswim {} ({BUILD})", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Run::Help => {
            println!("{}", cli::help());
            Ok(())
        }
        // a one-shot never touches raw mode or the alternate screen:
        // it is a command, so its answer goes to stdout where a pipe
        // or a keybind can read it. drawing one frame and then leaving
        // the alternate screen wiped the line it had just written.
        Run::Command(_) | Run::Script(_) => {
            #[cfg(feature = "dev-telemetry")]
            trace::init();
            oneshot(&cli)
        }
        Run::Prompt => {
            #[cfg(feature = "dev-telemetry")]
            trace::init();
            prompt()
        }
    }
}

/// `-c` and a script: run the lines, print what each one reported,
/// exit non-zero if any failed. No terminal is touched, which is also
/// what makes this path testable with no pty at all.
fn oneshot(cli: &Cli) -> anyhow::Result<()> {
    let lines = match &cli.run {
        Run::Command(line) => vec![line.clone()],
        Run::Script(path) => {
            let text = std::fs::read_to_string(path)
                .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", path.display()))?;
            cli::script_lines(&text)
        }
        _ => Vec::new(),
    };
    let mut host = Host::start()?;
    wim::trace_mark!("host ready (no terminal)");
    let failed = host.run_lines(&lines);
    for line in &host.console.lines {
        // the command is echoed by the console; a pipe wants the
        // answers, not the question.
        match line.kind {
            LineKind::Command => {}
            LineKind::Error => eprintln!("{}", line.text),
            LineKind::Note => println!("{}", line.text),
        }
    }
    core::save_playback_state(&host.player, &host.play_queue);
    if failed {
        wim::cache::cleanup_playback_files();
        std::process::exit(1);
    }
    Ok(())
}

fn prompt() -> anyhow::Result<()> {
    let mut host = Host::start()?;
    host.interactive_prompt = true;
    let mut lyrics_setup = host.first_run.then(|| LyricsSetup {
        providers: host.cfg.lyrics.providers.clone(),
        key_file: String::new(),
    });

    // the display's refresh rate decides how often the scrubber is
    // sampled, never how fast anything runs; one subprocess on a
    // thread, 60hz until it answers, so the first frame never waits.
    let frame_rx = anim::spawn_probe();
    let mut frame = anim::FrameBudget::default();

    // no graphics probe at all: shellswim draws no images, and the
    // probe reads stdin for up to a second on a terminal that will
    // never answer, racing the event loop for input. not running it is
    // both faster and what makes this loop the only reader of stdin.
    let mut guard = term::TerminalGuard::new(false)?;
    wim::trace_mark!("terminal ready");

    let mut keymap = Keymap::from_overrides(&host.cfg.keybinds);
    banner(&mut host.console, &host.cfg, &keymap);
    let mut first_frame = true;
    let mut queue_open = false;
    let mut queue_selected = 0usize;
    let mut subtitle_menu_open = false;
    let mut subtitle_menu_selected = 0usize;
    let mut keybinds_open = false;
    let mut keybinds_selected = 0usize;
    let mut keybinds_capturing = false;
    let mut keybinds_refused: Option<String> = None;
    let mut eq_menu = eqm::EqMenuPane::default();

    loop {
        host.pump();
        if let Ok(budget) = frame_rx.try_recv() {
            frame = budget;
        }

        // never blocks: a command that searches suspends on
        // `AwaitResults` and resumes on a later frame, once the
        // results for its query have settled.
        if host.step() == Drive::Quit {
            break;
        }
        if std::mem::take(&mut host.swim.keybinds_requested) && host.interactive_prompt {
            keybinds_open = true;
            keybinds_selected = 0;
            keybinds_refused = None;
        }
        if std::mem::take(&mut host.swim.eq_requested) && host.interactive_prompt {
            eq_menu.open(host.app.eq_source, host.cfg.eq_custom_presets.len());
        }

        let now_playing = host.now_playing();
        draw(
            &mut guard,
            &Screen {
                console: &host.console,
                playback: &host.playback,
                queue: &host.play_queue,
                now_playing: &now_playing,
                now: host.now(),
                gap_remaining: host
                    .between_tracks
                    .remaining(Instant::now())
                    .map(|d| d.as_secs_f64()),
                lyrics: host.lyrics_pane.is_open().then(|| {
                    let (width, height) = crossterm::terminal::size().unwrap_or((80, 24));
                    host.lyrics_pane.saw_size(
                        ((u32::from(width) * 45) / 100) as u16,
                        height.saturating_sub(4),
                    );
                    let active = host
                        .lyrics_pane
                        .follow_playback(host.playback.elapsed_secs, Instant::now());
                    (&host.lyrics_pane, active)
                }),
                setup: lyrics_setup.as_ref(),
                subtitles: &host.cfg.subtitles,
                queue_open,
                queue_selected,
                keymap: &keymap,
                subtitle_menu_open,
                subtitle_menu_selected,
                keybinds_open,
                keybinds_selected,
                keybinds_capturing,
                keybinds_refused: keybinds_refused.as_deref(),
                eq: eq_menu.is_open().then(|| EqScreen {
                    gains: &host.app.eq_gains,
                    saved: &host.cfg.eq_custom_presets,
                    source: host.app.eq_source,
                    modified: host.app.eq_custom,
                    selected: eq_menu.selected(),
                    state: eq_menu.state(),
                }),
            },
        )?;
        if first_frame {
            wim::trace_mark!("first frame drawn");
            first_frame = false;
            // the first thing that needs the socket, so it is also
            // what pays whatever is left of mpv's startup. deliberately
            // after the draw: the prompt is on screen first, and a
            // human cannot type inside the ~70ms this waits.
            if let Err(e) = host.player.observe_playback() {
                host.console.error = Some(format!("mpv will not report playback position: {e}"));
            }
            wim::trace_mark!("player observed");
        }

        // the scrubber moves while something plays and costs nothing
        // when nothing does.
        let animating = lyrics_setup.is_some()
            || queue_open
            || subtitle_menu_open
            || keybinds_open
            || eq_menu.is_open()
            || host.lyrics_pane.is_open()
            || host.has_program_work()
            || host.console.cycle().is_some()
            || (now_playing.is_some() && !host.playback.paused);
        let timeout = if animating {
            frame.interval()
        } else {
            anim::IDLE_POLL
        };
        if !event::poll(timeout)? {
            continue;
        }
        match event::read()? {
            Event::Paste(text) => {
                if let Some(setup) = &mut lyrics_setup {
                    setup.key_file.push_str(&text);
                    continue;
                }
                // `paste` recomputes the suggestion, so it takes the
                // candidates too; a ghost-less paste was a real bug
                // (docs/shellsong.md).
                let mut console = std::mem::take(&mut host.console);
                console.paste(&text, &host.candidates());
                host.console = console;
            }
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                // ctrl+c and ctrl+d are a shell's own exits; `quit()`
                // is the language's, and both land here.
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('d'))
                {
                    break;
                }
                if let Some(setup) = &mut lyrics_setup {
                    match key.code {
                        KeyCode::Esc => lyrics_setup = None,
                        KeyCode::F(1..=6) => {
                            let provider = setup.provider_for_key(key.code);
                            if let Some(index) = setup
                                .providers
                                .iter()
                                .position(|selected| selected == provider)
                            {
                                setup.providers.remove(index);
                            } else {
                                setup.providers.push(provider.to_owned());
                            }
                        }
                        KeyCode::Backspace => {
                            setup.key_file.pop();
                        }
                        KeyCode::Enter => {
                            host.cfg.lyrics.providers = setup.providers.clone();
                            if !setup.key_file.trim().is_empty() {
                                host.cfg
                                    .lyrics
                                    .api_key_files
                                    .insert("musixmatch".into(), setup.key_file.trim().to_owned());
                            }
                            match config::save(&host.cfg) {
                                Ok(()) => host.console.note("lyrics setup saved".to_owned()),
                                Err(error) => host.console.fail(format!("saving config: {error}")),
                            }
                            host.lyrics_client = wim::lyrics::Client::with_providers(
                                host.cfg.lyrics.providers.clone(),
                                host.cfg.resolve_lyrics_api_keys(),
                            );
                            host.first_run = false;
                            lyrics_setup = None;
                        }
                        KeyCode::Char(c) => setup.key_file.push(c),
                        _ => {}
                    }
                    continue;
                }
                if host.lyrics_pane.is_open() {
                    match key.code {
                        KeyCode::Esc => {
                            host.lyrics_pane.close();
                            host.swim.lyrics_requested = false;
                        }
                        KeyCode::Char('j') | KeyCode::Down => host.lyrics_pane.scroll_by(1),
                        KeyCode::Char('k') | KeyCode::Up => host.lyrics_pane.scroll_by(-1),
                        _ => {}
                    }
                    continue;
                }
                if subtitle_menu_open {
                    if key.code == KeyCode::Esc || key.code == subtitle_key(&keymap) {
                        subtitle_menu_open = false;
                        if let Err(error) = config::save(&host.cfg) {
                            host.console
                                .fail(format!("saving subtitle settings: {error}"));
                        }
                    } else {
                        match key.code {
                            KeyCode::Char('j') | KeyCode::Down => {
                                subtitle_menu_selected = (subtitle_menu_selected + 1)
                                    % wim::ui::subtitle_menu::FIELD_COUNT;
                            }
                            KeyCode::Char('k') | KeyCode::Up => {
                                subtitle_menu_selected = (subtitle_menu_selected
                                    + wim::ui::subtitle_menu::FIELD_COUNT
                                    - 1)
                                    % wim::ui::subtitle_menu::FIELD_COUNT;
                            }
                            KeyCode::Char('n') => match host.player.cycle_subtitle_track() {
                                Ok(Some(message)) => host.console.note(message),
                                Ok(None) => host.console.note("no subtitle tracks in this video"),
                                Err(error) => host.console.fail(format!("subtitle track: {error}")),
                            },
                            KeyCode::Char('x') => match host.player.toggle_subtitle_visibility() {
                                Ok(visible) => host.console.note(if visible {
                                    "subtitles visible"
                                } else {
                                    "subtitles hidden"
                                }),
                                Err(error) => {
                                    host.console.fail(format!("subtitle visibility: {error}"))
                                }
                            },
                            KeyCode::Char('h') | KeyCode::Left => {
                                adjust_subtitle_field(
                                    &mut host.cfg.subtitles,
                                    subtitle_menu_selected,
                                    -1.0,
                                );
                                host.player.set_subtitle_style(&host.cfg.subtitles);
                                if let Err(error) = config::save(&host.cfg) {
                                    host.console
                                        .fail(format!("saving subtitle settings: {error}"));
                                }
                            }
                            KeyCode::Char('l') | KeyCode::Right => {
                                adjust_subtitle_field(
                                    &mut host.cfg.subtitles,
                                    subtitle_menu_selected,
                                    1.0,
                                );
                                host.player.set_subtitle_style(&host.cfg.subtitles);
                                if let Err(error) = config::save(&host.cfg) {
                                    host.console
                                        .fail(format!("saving subtitle settings: {error}"));
                                }
                            }
                            _ => {}
                        }
                    }
                    continue;
                }
                if eq_menu.is_open() {
                    if eq_menu.state().editor.is_some() {
                        if let Some(mode) = eq_menu_editor_key(
                            key.code,
                            eq_menu.state_mut(),
                            &mut host.cfg.eq_custom_presets,
                            &mut host.app,
                        ) {
                            persist_eq(&mut host.cfg, &host.app, &mut host.notifications);
                            eq_menu.set_selected(match mode {
                                eqm::EditorMode::Save => eqm::opening_selection(
                                    host.app.eq_source,
                                    host.cfg.eq_custom_presets.len(),
                                ),
                                eqm::EditorMode::Rename(i) => eqm::PRESET_COUNT + i,
                            });
                        }
                        continue;
                    }
                    let saved_len = host.cfg.eq_custom_presets.len();
                    let at = eq_menu.cursor(saved_len);
                    if key.code != eqm::DELETE_KEY {
                        eq_menu.state_mut().disarm_delete();
                    }
                    if key.code == KeyCode::Esc {
                        eq_menu.close();
                        persist_eq(&mut host.cfg, &host.app, &mut host.notifications);
                    } else if key.code == eqm::SAVE_KEY {
                        eq_menu.state_mut().editor = Some(eqm::NameEditor::save());
                        eq_menu.state_mut().message.clear();
                    } else if key.code == eqm::RENAME_KEY {
                        if let eqm::Cursor::Saved(i) = at {
                            let current = host.cfg.eq_custom_presets.name_at(i).unwrap_or("");
                            eq_menu.state_mut().editor = Some(eqm::NameEditor::rename(i, current));
                            eq_menu.state_mut().message.clear();
                        } else {
                            eq_menu.state_mut().message = eq_saved_row_hint(&host.cfg, "rename");
                        }
                    } else if key.code == eqm::DELETE_KEY {
                        if let eqm::Cursor::Saved(i) = at {
                            let name = host
                                .cfg
                                .eq_custom_presets
                                .name_at(i)
                                .unwrap_or_default()
                                .to_string();
                            if eq_menu.state_mut().delete.press(i) {
                                host.cfg.eq_custom_presets.remove(i);
                                host.app.eq_source =
                                    wim::eq::source_after_removal(host.app.eq_source, i);
                                host.app.eq_custom =
                                    host.app.eq_source.is_some() && host.app.eq_custom;
                                persist_eq(&mut host.cfg, &host.app, &mut host.notifications);
                                eq_menu.set_selected(eq_menu.selected().min(
                                    eqm::selectable_count(host.cfg.eq_custom_presets.len()) - 1,
                                ));
                                eq_menu.state_mut().message = format!("deleted '{name}'");
                            } else {
                                eq_menu.state_mut().message = format!(
                                    "press {} again to delete '{name}'",
                                    key_label(eqm::DELETE_KEY)
                                );
                            }
                        } else {
                            eq_menu.state_mut().message = eq_saved_row_hint(&host.cfg, "delete");
                        }
                    } else {
                        let bands = host.cfg.visualizer_bands as usize;
                        let enabled = host.cfg.eq_enabled;
                        match key.code {
                            KeyCode::Char('j') | KeyCode::Down => eq_menu.set_selected(
                                (eq_menu.selected() + 1).min(eqm::selectable_count(saved_len) - 1),
                            ),
                            KeyCode::Char('k') | KeyCode::Up => {
                                eq_menu.set_selected(eq_menu.selected().saturating_sub(1))
                            }
                            KeyCode::Enter => core::apply_eq_row(
                                &mut host.app,
                                &host.player,
                                &mut host.notifications,
                                &host.cfg.eq_custom_presets,
                                eq_menu.selected(),
                                enabled,
                                bands,
                            ),
                            KeyCode::Char('h' | 'l') | KeyCode::Left | KeyCode::Right => {
                                let dir = if matches!(key.code, KeyCode::Char('h') | KeyCode::Left)
                                {
                                    -1.0
                                } else {
                                    1.0
                                };
                                if let eqm::Cursor::Band(band) = at {
                                    adjust_eq_band(
                                        &mut host.app,
                                        &host.player,
                                        &mut host.notifications,
                                        band,
                                        dir,
                                        enabled,
                                        bands,
                                    );
                                }
                            }
                            _ => {}
                        }
                    }
                    continue;
                }
                if keybinds_open {
                    if keybinds_capturing {
                        keybinds_capturing = false;
                        if key.code == KeyCode::Esc {
                            keybinds_refused = None;
                        } else {
                            let action = ALL_ACTIONS[keybinds_selected];
                            keybinds_refused = keymap.refusal(action, key.code);
                            if keybinds_refused.is_none() {
                                keymap.set(action, key.code);
                                host.cfg.keybinds = keymap.overrides();
                                if let Err(error) = config::save(&host.cfg) {
                                    host.console.fail(format!("saving keybinds: {error}"));
                                }
                            }
                        }
                    } else {
                        match key.code {
                            KeyCode::Esc | KeyCode::Char('K') => keybinds_open = false,
                            KeyCode::Char('j') | KeyCode::Down => {
                                keybinds_selected = (keybinds_selected + 1) % ALL_ACTIONS.len();
                                keybinds_refused = None;
                            }
                            KeyCode::Char('k') | KeyCode::Up => {
                                keybinds_selected =
                                    (keybinds_selected + ALL_ACTIONS.len() - 1) % ALL_ACTIONS.len();
                                keybinds_refused = None;
                            }
                            KeyCode::Char('r') => {
                                keybinds_capturing = true;
                                keybinds_refused = None;
                            }
                            KeyCode::Char('0') => {
                                keymap.reset(ALL_ACTIONS[keybinds_selected]);
                                host.cfg.keybinds = keymap.overrides();
                                if let Err(error) = config::save(&host.cfg) {
                                    host.console.fail(format!("saving keybinds: {error}"));
                                }
                            }
                            KeyCode::Char('R') => {
                                keymap.reset_all();
                                host.cfg.keybinds = keymap.overrides();
                                if let Err(error) = config::save(&host.cfg) {
                                    host.console.fail(format!("saving keybinds: {error}"));
                                }
                            }
                            _ => {}
                        }
                    }
                    continue;
                }
                if queue_open {
                    let order = wim::ui::queue::selectable(
                        host.play_queue.entries(),
                        host.play_queue.pos(),
                    );
                    let at = order
                        .iter()
                        .position(|index| *index == queue_selected)
                        .unwrap_or(0);
                    match key.code {
                        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => {
                            queue_open = false;
                        }
                        KeyCode::Char('j') | KeyCode::Down => {
                            if let Some(next) = order.get(at + 1) {
                                queue_selected = *next;
                            }
                        }
                        KeyCode::Char('k') | KeyCode::Up => {
                            if at > 0 {
                                queue_selected = order[at - 1];
                            }
                        }
                        KeyCode::Enter if order.contains(&queue_selected) => {
                            if let Err(error) = host
                                .player
                                .set_property("playlist-pos", serde_json::json!(queue_selected))
                            {
                                host.console.fail(format!("error jumping: {error}"));
                            }
                        }
                        KeyCode::Char('d') if host.play_queue.remove(queue_selected) => {
                            if let Err(error) = host.player.playlist_remove(queue_selected) {
                                host.console.fail(format!("error removing: {error}"));
                            }
                            queue_selected = queue_selected
                                .min(host.play_queue.entries().len().saturating_sub(1));
                        }
                        KeyCode::Char('J') | KeyCode::Char('K') => {
                            let first_upcoming =
                                host.play_queue.pos().map_or(0, |position| position + 1);
                            let last = host.play_queue.entries().len().saturating_sub(1);
                            let target = if key.code == KeyCode::Char('J') {
                                queue_selected + 1
                            } else {
                                queue_selected.saturating_sub(1)
                            };
                            if queue_selected >= first_upcoming
                                && target >= first_upcoming
                                && target <= last
                                && target != queue_selected
                                && host.play_queue.move_entry(queue_selected, target)
                            {
                                if let Err(error) =
                                    host.player.playlist_move(queue_selected, target)
                                {
                                    host.console.fail(format!("error moving: {error}"));
                                }
                                queue_selected = target;
                            }
                        }
                        _ => {}
                    }
                    continue;
                }
                if key.code == keymap.get(KeyAction::OpenQueueView) && host.console.input.is_empty()
                {
                    queue_open = true;
                    queue_selected = host.play_queue.pos().unwrap_or(0);
                    continue;
                }
                if key.code == subtitle_key(&keymap) && host.console.input.is_empty() {
                    subtitle_menu_open = true;
                    continue;
                }
                if key.code == KeyCode::Char('E') && host.console.input.is_empty() {
                    eq_menu.open(host.app.eq_source, host.cfg.eq_custom_presets.len());
                    continue;
                }
                if key.code == KeyCode::Char('K') && host.console.input.is_empty() {
                    keybinds_open = true;
                    keybinds_selected = 0;
                    keybinds_refused = None;
                    continue;
                }
                // '?' on an empty line is the short command list: the
                // one thing a new user should not need the docs for.
                // anywhere else it is a character like any other.
                if key.code == KeyCode::Char('?') && host.console.input.is_empty() {
                    for line in core::short_help() {
                        host.console.note(line);
                    }
                    continue;
                }
                let mut console = std::mem::take(&mut host.console);
                let action = console.handle_key(key, &host.candidates());
                host.console = console;
                match action {
                    ConsoleAction::Run(p) => {
                        host.settle.notified("", false, host.now());
                        host.submit_program(p);
                    }
                    // nothing to close: the console *is* the screen.
                    ConsoleAction::Close | ConsoleAction::None => {}
                }
            }
            _ => {}
        }
    }

    core::save_playback_state(&host.player, &host.play_queue);
    drop(guard);
    Ok(())
}

/// Two lines, once, saying what this is and where to look next. Not a
/// splash: it is the first two lines of scrollback and scrolls away
/// like any other line, which is the only kind of banner worth having.
fn banner(console: &mut Console, cfg: &config::Config, keymap: &Keymap) {
    let where_from = match (cfg.library_paths.len(), cfg.navidrome_servers.len()) {
        (0, 0) => "no library yet; run wim once to set one up".to_string(),
        (paths, 0) => format!("{paths} local path(s)"),
        (0, n) => format!("{n} server(s)"),
        (paths, n) => format!("{paths} local path(s), {n} server(s)"),
    };
    console.note(format!(
        "shellswim {} - {where_from}",
        env!("CARGO_PKG_VERSION")
    ));
    console.note(format!(
        "? commands · {} queue · {} subtitles · K keys · E eq · tab complete · dry(...) preview",
        key_label(keymap.get(KeyAction::OpenQueueView)),
        key_label(subtitle_key(keymap))
    ));
}

/// A lowercase letter on an empty prompt starts a command (`top`,
/// `toggle`), so the default `t` becomes `T` here.
fn subtitle_key(keymap: &Keymap) -> KeyCode {
    match keymap.get(KeyAction::OpenSubtitleMenu) {
        KeyCode::Char(c) if c.is_ascii_lowercase() => KeyCode::Char(c.to_ascii_uppercase()),
        other => other,
    }
}

fn adjust_subtitle_field(sub: &mut config::SubtitleConfig, field: usize, direction: f64) {
    match field {
        0 => sub.delay_secs = (sub.delay_secs + direction * 0.5).clamp(-30.0, 30.0),
        1 => {
            sub.font_size = (sub.font_size as i64 + (direction * 2.0) as i64).clamp(10, 100) as u32
        }
        2 => sub.position = (sub.position as i64 + (direction * 5.0) as i64).clamp(0, 150) as u32,
        3 => sub.outline_size = (sub.outline_size + direction * 0.5).clamp(0.0, 5.0),
        4 => sub.background_opacity = (sub.background_opacity + direction * 0.1).clamp(0.0, 1.0),
        5 => sub.blur = (sub.blur + direction * 0.5).clamp(0.0, 10.0),
        6 => sub.color_level = (sub.color_level + direction as f32 * 0.05).clamp(0.0, 1.0),
        _ => {}
    }
}

/// Everything one frame draws, bundled to stay under clippy's argument
/// limit the way `CoreCtx` is.
struct Screen<'a> {
    console: &'a Console,
    playback: &'a mpv::Playback,
    queue: &'a PlayQueue,
    now_playing: &'a Option<SearchItem>,
    now: Duration,
    /// Seconds left of a between-track gap; shellswim is mostly this
    /// bar, so "paused with no reason given" is most of the screen.
    gap_remaining: Option<f64>,
    lyrics: Option<(&'a wim::lyricspane::LyricsPane, Option<usize>)>,
    setup: Option<&'a LyricsSetup>,
    subtitles: &'a config::SubtitleConfig,
    queue_open: bool,
    queue_selected: usize,
    keymap: &'a Keymap,
    subtitle_menu_open: bool,
    subtitle_menu_selected: usize,
    keybinds_open: bool,
    keybinds_selected: usize,
    keybinds_capturing: bool,
    keybinds_refused: Option<&'a str>,
    eq: Option<EqScreen<'a>>,
}

struct EqScreen<'a> {
    gains: &'a [f64; 10],
    saved: &'a wim::eq::CustomPresets,
    source: Option<wim::eq::Source>,
    modified: bool,
    selected: usize,
    state: &'a eqm::EqMenuState,
}

struct LyricsSetup {
    providers: Vec<String>,
    key_file: String,
}

impl LyricsSetup {
    fn provider_for_key(&self, key: KeyCode) -> &'static str {
        match key {
            KeyCode::F(1) => "local",
            KeyCode::F(2) => "opensubsonic",
            KeyCode::F(3) => "lrclib",
            KeyCode::F(4) => "karalyr",
            KeyCode::F(5) => "musixmatch",
            _ => "lyrics-ovh",
        }
    }

    fn text(&self) -> String {
        let choices = [
            ("local", "sidecars and embedded tags"),
            ("opensubsonic", "server-provided lyrics"),
            ("lrclib", "synced; no API key"),
            ("karalyr", "word-synced; non-commercial terms"),
            ("musixmatch", "optional; requires an API key"),
            ("lyrics-ovh", "plain text; no API key"),
        ];
        let mut rows = vec![
            "Choose lyrics sources (F1-F6 toggle; defaults work without setup):".to_owned(),
            String::new(),
        ];
        rows.extend(
            choices
                .iter()
                .enumerate()
                .map(|(index, (provider, description))| {
                    format!(
                        " F{} [{}] {:<13} {}",
                        index + 1,
                        if self.providers.iter().any(|selected| selected == provider) {
                            'x'
                        } else {
                            ' '
                        },
                        provider,
                        description
                    )
                }),
        );
        rows.extend([
            String::new(),
            "Musixmatch API key file (optional; do not paste the key):".to_owned(),
            format!(" {}_", self.key_file),
            String::new(),
            "Enter saves and continues; Esc keeps the defaults".to_owned(),
        ]);
        rows.join("\n")
    }
}

fn draw(guard: &mut term::TerminalGuard, s: &Screen) -> std::io::Result<()> {
    let cursor_visible = anim::blink(s.now, anim::CURSOR_BLINK);
    let title = s.now_playing.as_ref().map(label);
    guard
        .terminal
        .draw(|f| {
            // the console takes everything; the bar keeps two rows, the
            // track name over the transport strip. that is all the chrome
            // there is.
            // a rule, then space / bar / space, so the bar is delimited
            // rather than jammed against the console's last line.
            let [body, rule, _, bar, _] = Layout::vertical([
                Constraint::Min(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .areas(f.area());
            if let Some(setup) = s.setup {
                let widget = Paragraph::new(setup.text())
                    .alignment(Alignment::Center)
                    .wrap(Wrap { trim: false })
                    .style(Style::default().fg(shade(NEAR_WHITE)))
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title("shellswim · first-run setup")
                            .border_style(Style::default().fg(shade(DIM))),
                    );
                f.render_widget(widget, f.area());
                return;
            }
            if let Some((pane, active_line)) = s.lyrics {
                let [console_area, lyrics_area] =
                    Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)])
                        .areas(body);
                let divider_x = console_area.x + console_area.width.saturating_sub(1);
                let console_area = ratatui::layout::Rect {
                    width: console_area.width.saturating_sub(1),
                    ..console_area
                };
                let lines: Vec<&str> = pane
                    .lines()
                    .unwrap_or_default()
                    .iter()
                    .map(String::as_str)
                    .collect();
                f.render_widget(
                    LyricsView {
                        lines: &lines,
                        scroll: pane.scroll(),
                        active_line,
                        track: pane.track(),
                        source: pane.source(),
                        synced: pane.is_synced(),
                        following: pane.following(),
                        attribution: pane.attribution(),
                    },
                    lyrics_area,
                );
                f.render_widget(
                    wim::ui::divider::VerticalRule {
                        highlighted: false,
                        junction_rows: Vec::new(),
                    },
                    ratatui::layout::Rect {
                        x: divider_x,
                        width: 1,
                        ..body
                    },
                );
                f.render_widget(
                    SwimConsole {
                        console: s.console,
                        cursor_visible,
                    },
                    console_area,
                );
            } else if s.queue_open {
                f.render_widget(
                    wim::ui::queue::QueueView {
                        entries: s.queue.entries(),
                        pos: s.queue.pos(),
                        selected: s.queue_selected,
                        focused: true,
                        elapsed: s.now,
                        keymap: s.keymap,
                    },
                    body,
                );
            } else if s.subtitle_menu_open {
                let values = [
                    format!("{:+.1}s", s.subtitles.delay_secs),
                    s.subtitles.font_size.to_string(),
                    s.subtitles.position.to_string(),
                    format!("{:.1}", s.subtitles.outline_size),
                    format!("{:.1}", s.subtitles.background_opacity),
                    format!("{:.1}", s.subtitles.blur),
                    format!("{:.2}", s.subtitles.color_level),
                ];
                f.render_widget(
                    wim::ui::subtitle_menu::SubtitleMenu {
                        values: values.each_ref().map(String::as_str),
                        selected: s.subtitle_menu_selected,
                        keymap: s.keymap,
                        alignment_status: "",
                        alignment_controls: false,
                    },
                    body,
                );
            } else if let Some(eq) = &s.eq {
                f.render_widget(
                    eqm::EqMenu {
                        gains: eq.gains,
                        saved: eq.saved,
                        source: eq.source,
                        modified: eq.modified,
                        selected: eq.selected,
                        state: eq.state,
                        elapsed: s.now,
                    },
                    body,
                );
            } else if s.keybinds_open {
                let keys: Vec<String> = ALL_ACTIONS
                    .iter()
                    .map(|action| key_label(s.keymap.get(*action)))
                    .collect();
                f.render_widget(
                    wim::ui::keybinds_menu::KeybindsMenu {
                        keys: &keys,
                        selected: s.keybinds_selected,
                        capturing: s.keybinds_capturing,
                        refused: s.keybinds_refused,
                    },
                    body,
                );
            } else {
                f.render_widget(
                    SwimConsole {
                        console: s.console,
                        cursor_visible,
                    },
                    body,
                );
            }
            f.render_widget(
                HorizontalRule {
                    highlighted: false,
                    junctions: Vec::new(),
                },
                rule,
            );
            f.render_widget(
                SwimBar {
                    title: title.as_deref(),
                    elapsed_secs: s.playback.elapsed_secs,
                    // the entry's own length where it knows better than
                    // mpv, which is a generated silence; see
                    // `core::playing_duration`.
                    duration_secs: wim::core::playing_duration(
                        s.queue.current(),
                        s.playback.duration_secs,
                    ),
                    gap_remaining: s.gap_remaining,
                },
                bar,
            );
        })
        .map(|_| ())
}

#[cfg(test)]
mod subtitle_adjust_tests {
    use super::*;

    #[test]
    fn subtitle_adjustments_step_and_clamp_to_supported_ranges() {
        let mut config = config::SubtitleConfig::default();
        adjust_subtitle_field(&mut config, 0, -1.0);
        assert_eq!(config.delay_secs, -0.5);
        adjust_subtitle_field(&mut config, 1, 1.0);
        assert_eq!(config.font_size, 44);
        adjust_subtitle_field(&mut config, 2, -1.0);
        assert_eq!(config.position, 5);

        config.delay_secs = 30.0;
        config.font_size = 100;
        config.position = 150;
        adjust_subtitle_field(&mut config, 0, 1.0);
        adjust_subtitle_field(&mut config, 1, 1.0);
        adjust_subtitle_field(&mut config, 2, 1.0);
        assert_eq!(config.delay_secs, 30.0);
        assert_eq!(config.font_size, 100);
        assert_eq!(config.position, 150);
    }
}
