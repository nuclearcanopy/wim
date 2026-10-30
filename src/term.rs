use std::io::{self, Stdout, Write};

use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

/// Enables raw mode + alternate screen + bracketed paste (+ mouse
/// capture, when asked for) on construction and restores all of them on
/// drop, including on panic via a hook installed here, so a crash never
/// leaves the user's shell in a broken state. Bracketed paste matters
/// specifically for the navidrome password field; without it, a
/// terminal's native paste (e.g. from a password manager) isn't
/// reliably delivered as a single `Event::Paste`, and raw keystroke
/// synthesis isn't guaranteed either. Mouse capture is the one whose
/// restore matters most: left on after a crash, the shell prints escape
/// gibberish on every pointer move until it is reset by hand.
pub struct TerminalGuard {
    pub terminal: Terminal<CrosstermBackend<Stdout>>,
    mouse: bool,
}

impl TerminalGuard {
    pub fn new(mouse: bool) -> io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;
        if mouse {
            execute!(stdout, EnableMouseCapture)?;
            // crossterm's EnableMouseCapture also turns on any-event
            // tracking (?1003), which reports every pointer motion
            // across the terminal and would cost a full redraw per
            // event, idle or not. ?1002 stays on, which is the motion
            // actually wanted: motion while a button is held, i.e. a
            // drag on the scrubber.
            write!(stdout, "\x1b[?1003l")?;
            // the terminal defaults the pointer to an i-beam, because
            // it assumes the screen is selectable text. with capture
            // on it is not: a click is a button press, so ask for the
            // ordinary arrow. OSC 22 is the pointer-shape sequence
            // kitty defined; kitty, ghostty, wezterm and foot honour
            // it, and a terminal that does not know it ignores the
            // whole sequence, so sending it unconditionally is safe.
            // alacritty is one of the ones that ignores it, so the
            // i-beam stays there until the maintainer moves terminal.
            write!(stdout, "\x1b]22;default\x07")?;
            stdout.flush()?;
        }

        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore(mouse);
            default_hook(info);
        }));

        let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
        Ok(Self { terminal, mouse })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore(self.mouse);
    }
}

/// The one teardown sequence both exit paths run; two copies is how the
/// panic path ends up restoring less than `Drop` does.
fn restore(mouse: bool) {
    let _ = crossterm::terminal::disable_raw_mode();
    if mouse {
        let _ = execute!(io::stdout(), DisableMouseCapture);
        // empty payload resets the pointer to whatever the terminal
        // would have chosen, so wim does not leave a shape behind on
        // the shell it hands the screen back to.
        let mut out = io::stdout();
        let _ = write!(out, "\x1b]22;\x07");
        let _ = out.flush();
    }
    let _ = execute!(io::stdout(), DisableBracketedPaste, LeaveAlternateScreen);
}
