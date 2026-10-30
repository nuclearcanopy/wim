use std::io::{self, Stdout, Write};

use crossterm::cursor::Hide;
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

    /// Draws, unless mpv currently owns the screen.
    ///
    /// Fullscreen video is mpv drawing straight to this terminal, and
    /// ratatui repaints from its own buffer: a single `draw` while the
    /// vo is up overwrites the picture with the ui that was there
    /// before. The gate lives here rather than at the call site so the
    /// one `draw` in the app stays one expression. See docs/video.md.
    pub fn draw_unless_video<F>(&mut self, mpv_owns_screen: bool, render: F) -> io::Result<()>
    where
        F: FnOnce(&mut ratatui::Frame),
    {
        if mpv_owns_screen {
            return Ok(());
        }
        self.terminal.draw(render).map(|_| ())
    }

    /// Takes the screen back after mpv's video output has been turned
    /// off again.
    ///
    /// Both terminal vos wrap themselves in `?1049h`/`?1049l`,
    /// `?25l`/`?25h` and `?1003h`/`?1003l`, so once one is torn down the
    /// terminal is on the *primary* buffer with the cursor visible and
    /// wim's own mouse capture switched off; all three have to be
    /// re-asserted. The clear is the other half: ratatui diffs against
    /// what it last drew, and mpv overwrote all of it, so without
    /// dropping that notion of the screen only the cells wim's own
    /// buffer calls changed would be repainted.
    pub fn reclaim(&mut self) -> io::Result<()> {
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, Hide)?;
        if self.mouse {
            execute!(stdout, EnableMouseCapture)?;
            // same two follow-ups as `new`, and for the same reasons.
            write!(stdout, "\x1b[?1003l")?;
            write!(stdout, "\x1b]22;default\x07")?;
        }
        stdout.flush()?;
        self.terminal.clear()
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
