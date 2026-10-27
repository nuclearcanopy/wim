use std::io::{self, Stdout};

use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

/// Enables raw mode + alternate screen + bracketed paste on
/// construction, restores all three on drop (including on panic, via a
/// hook installed here) so a crash never leaves the user's shell in a
/// broken state. Bracketed paste matters specifically for the
/// navidrome password field — without it, a terminal's native paste
/// (e.g. from a password manager) isn't reliably delivered as a single
/// `Event::Paste`, and raw keystroke synthesis isn't guaranteed either.
pub struct TerminalGuard {
    pub terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    pub fn new() -> io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;

        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = crossterm::terminal::disable_raw_mode();
            let _ = execute!(io::stdout(), DisableBracketedPaste, LeaveAlternateScreen);
            default_hook(info);
        }));

        let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
        Ok(Self { terminal })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = execute!(io::stdout(), DisableBracketedPaste, LeaveAlternateScreen);
    }
}
