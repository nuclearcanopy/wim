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
    }
    let _ = execute!(io::stdout(), DisableBracketedPaste, LeaveAlternateScreen);
}
