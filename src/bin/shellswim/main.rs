//! `shellswim`: wim's player with nothing but the prompt. A
//! full-screen shellsong console, the playing track and its scrubber;
//! no panes, no sidebar, no results list. Everything is a command.
//!
//! It is a second front-end over the same `wim` library, which is the
//! point: the interpreter, the op table and the player are shared, and
//! all that lives here is presentation plus a `Surface` that declines
//! what a shell cannot show. See docs/library-seam.md and
//! docs/shellswim.md.

use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};

use wim::core::{self, Drive};
use wim::playqueue::PlayQueue;
use wim::search::SearchItem;
use wim::shellsong::{Console, ConsoleAction, LineKind};
use wim::swimconsole::{SwimConsole, TrackLine};
use wim::ui::controlsbar::ControlsBar;
use wim::ui::results_list::label;
use wim::{anim, app, config, mpv, term, trace};

mod cli;
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
            trace::init();
            oneshot(&cli)
        }
        Run::Prompt => {
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
    trace::mark("host ready (no terminal)");
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
    trace::mark("terminal ready");

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
                app: &host.app,
                queue: &host.play_queue,
                now_playing: &now_playing,
                now: host.now(),
                crossfade: host.crossfade_enabled,
            },
        )?;
        if first_frame {
            trace::mark("first frame drawn");
            first_frame = false;
        }

        // the scrubber moves while something plays and costs nothing
        // when nothing does.
        let animating = host.program.is_some()
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
                        host.settle.notified(false, host.now());
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
    app: &'a app::App,
    queue: &'a PlayQueue,
    now_playing: &'a Option<SearchItem>,
    now: Duration,
    crossfade: bool,
}

fn draw(guard: &mut term::TerminalGuard, s: &Screen) -> std::io::Result<()> {
    let cursor_visible = anim::blink(s.now, anim::CURSOR_BLINK);
    let title = s.now_playing.as_ref().map(label);
    let upcoming = s
        .queue
        .pos()
        .map_or(0, |p| s.queue.entries().len().saturating_sub(p + 1));
    guard.draw_unless_video(false, |f| {
        // the console takes everything; the bar keeps two rows, the
        // track name over the transport strip. that is all the chrome
        // there is.
        let [body, name, bar] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(f.area());
        f.render_widget(
            SwimConsole {
                console: s.console,
                cursor_visible,
            },
            body,
        );
        f.render_widget(
            TrackLine {
                title: title.as_deref(),
                upcoming,
            },
            name,
        );
        f.render_widget(
            ControlsBar {
                move_mode: false,
                focus_label: "shell",
                focused: false,
                queue_len: s.queue.entries().len(),
                loop_mode: s.app.loop_mode,
                shuffle_on: s.app.shuffle,
                speed: s.app.speed,
                eq_active: s.app.eq_gains.iter().any(|g| *g != 0.0),
                crossfade_on: s.crossfade,
                paused: s.playback.paused,
                elapsed_secs: s.playback.elapsed_secs,
                // the entry's own length where it knows better than
                // mpv, which is a generated silence; see
                // `core::playing_duration`. shellswim is mostly this
                // bar, so a 15m gap drawing as a full one-second
                // track is most of what is on screen.
                duration_secs: wim::core::playing_duration(
                    s.queue.current(),
                    s.playback.duration_secs,
                ),
                flash: None,
                selected: None,
                scrubbing: false,
                resize_hint: None,
            },
            bar,
        );
    })
}
