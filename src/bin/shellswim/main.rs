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
use wim::playqueue::PlayQueue;
use wim::search::SearchItem;
use wim::shellsong::{Console, ConsoleAction, LineKind};
use wim::swimconsole::{SwimBar, SwimConsole};
use wim::theme::{shade, DIM, NEAR_WHITE};
use wim::ui::divider::HorizontalRule;
use wim::ui::lyrics_view::LyricsView;
use wim::ui::results_list::label;
use wim::{anim, config, mpv, term};
#[cfg(feature = "dev-telemetry")]
use wim::trace;

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

    banner(&mut host.console, &host.cfg);
    let mut first_frame = true;

    loop {
        host.pump();
        if let Ok(budget) = frame_rx.try_recv() {
            frame = budget;
        }

        // never blocks: a command that searches suspends on
        // `AwaitResults` and resumes on a later frame, once the
        // results for its query have settled.
        if host.program.is_some() && host.step() == Drive::Quit {
            break;
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
            || host.lyrics_pane.is_open()
            || host.program.is_some()
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
                        host.program = Some(p);
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
fn banner(console: &mut Console, cfg: &config::Config) {
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
    console.note("? for the commands, tab to complete, dry(...) to see without doing.");
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
    guard.draw_unless_video(false, |f| {
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
}
