//! shellswim's half of the seam. It has no panes, so most of
//! `Surface` is a decline; the two it honours it honours by printing,
//! which is the only thing a shell can do with a screen.
//!
//! Why decline rather than silently no-op: `view(eq)` doing nothing
//! and `view(eq)` saying where the eq lives are very different
//! answers, and only one of them is true. See docs/library-seam.md.

use wim::core;
use wim::core::{op_fail, op_note, CoreCtx, Dispatch, Report, SelectStep, Surface};
use wim::search::SearchItem;
use wim::shellsong::ViewTarget;
use wim::ui::results_list::label;

/// What shellswim keeps that the core does not. Almost nothing, which
/// is the point: the list below is the entire difference between the
/// two front-ends' state.
#[derive(Default)]
pub struct SwimSurface {
    /// Whether the last `view(now)` asked for the full readout, so a
    /// second one does not repeat itself. Purely cosmetic.
    pub verbose_now: bool,
}

impl SwimSurface {
    /// A borrowing view, so the surface and a `CoreCtx` are two
    /// disjoint borrows the way wim's are.
    pub fn borrowed(&mut self) -> Borrowed<'_> {
        Borrowed { state: self }
    }
}

pub struct Borrowed<'a> {
    state: &'a mut SwimSurface,
}

impl Surface for Borrowed<'_> {
    fn front_end(&self) -> &'static str {
        "shellswim"
    }

    /// There is no highlight to read, so the selection is the head of
    /// the current result set; that is where wim's highlight starts,
    /// so `play()` after a `search()` means the same thing in both.
    fn selection(&self, core: &CoreCtx) -> Option<SearchItem> {
        core.app.results.first().cloned()
    }

    /// `view(now)` and `view(lyrics)` are honoured as text; the rest
    /// are panes and are declined by name.
    fn open_screen(&mut self, target: ViewTarget, core: &mut CoreCtx) -> (Dispatch, Report) {
        match target {
            ViewTarget::Now => self.print_now(core),
            ViewTarget::Lyrics => print_lyrics(core),
            // `decline`'s generic wording would read "...; wim does"
            // after a message that already named the key, so these
            // say it once, themselves.
            ViewTarget::Eq => op_fail(no_pane("equaliser", "wim's 'e'")),
            ViewTarget::Settings => op_fail(no_pane("settings", "wim's ','")),
            ViewTarget::Keybinds => op_fail(no_pane("keybinds", "wim's 'K'")),
            ViewTarget::Suggestions => op_fail(no_pane("suggestions", "wim's 'g'")),
            ViewTarget::Starred => op_fail(format!(
                "{}, but star(...) and view(queue) work here",
                no_pane("starred", "wim's 'F'")
            )),
            // the five listings never reach a surface; the core prints
            // them. honoured here too rather than left to panic if
            // `is_screen()` and this ever disagree.
            other => core::print_listing(other, core),
        }
    }

    fn move_selection(&mut self, _step: SelectStep, _core: &mut CoreCtx) -> (Dispatch, Report) {
        op_fail(NO_SELECTION)
    }

    fn go_browse_row(&mut self, _core: &mut CoreCtx) -> (Dispatch, Report) {
        op_fail(NO_BROWSE_ROW)
    }

    fn open_browse(&mut self, item: &SearchItem, _core: &mut CoreCtx) -> (Dispatch, Report) {
        // a page is a pane, but what was wanted is almost always the
        // tracks, and `queue` plus `view(queue)` gets those.
        op_fail(no_page(item.primary_name()))
    }
}

impl Borrowed<'_> {
    /// `view(now)`: what is playing, what is behind it, and where in
    /// the track we are. One line, or three on a repeat, because the
    /// bar is already showing the short version.
    fn print_now(&mut self, core: &mut CoreCtx) -> (Dispatch, Report) {
        let Some(item) = core.now_playing.clone() else {
            return op_fail("nothing is playing");
        };
        let upcoming = core.queue.entries().len().saturating_sub(
            core.queue
                .pos()
                .map(|p| p + 1)
                .unwrap_or(core.queue.entries().len()),
        );
        if !self.state.verbose_now {
            self.state.verbose_now = true;
            return op_note(now_line(&label(&item), upcoming));
        }
        // the second ask wants more than the bar already shows.
        for (i, entry) in core.queue.entries().iter().enumerate().take(6) {
            let here = if Some(i) == core.queue.pos() {
                "playing"
            } else if Some(i) > core.queue.pos() {
                "queued"
            } else {
                "played"
            };
            core.console
                .note(format!("{here:>7}  {}. {}", i + 1, label(entry)));
        }
        op_note(now_line(&label(&item), upcoming))
    }
}

/// The three refusals, as values rather than inline strings: they are
/// the user-facing half of the seam, so they are asserted directly.
const NO_SELECTION: &str = "nothing is highlighted here; name what you mean, e.g. play(crest)";
const NO_BROWSE_ROW: &str = "no browse view here; play(album name) plays the whole thing";

/// `shellswim has no X pane; <where> has it`. One shape, so every
/// refusal of a pane reads the same and always points somewhere.
fn no_pane(what: &str, instead: &str) -> String {
    format!("shellswim has no {what} pane; {instead} has it")
}

fn no_page(name: &str) -> String {
    format!("no page to open '{name}' in; queue({name}) then view(queue)")
}

/// `playing: X` with the queue depth behind it, so "what is playing"
/// and "what is queued" are one legible line rather than two guesses.
fn now_line(name: &str, upcoming: usize) -> String {
    match upcoming {
        0 => format!("playing: {name} (nothing queued behind it)"),
        1 => format!("playing: {name} (1 queued behind it)"),
        n => format!("playing: {name} ({n} queued behind it)"),
    }
}

/// `view(lyrics)`: wim opens a pane, shellswim prints them. Both ask
/// the same loader and render the reply through `core::lyrics_text`,
/// so the two cannot report different things, and neither blocks.
fn print_lyrics(core: &mut CoreCtx) -> (Dispatch, Report) {
    let Some(item) = core.now_playing.clone() else {
        return op_fail("nothing is playing");
    };
    let Some((artist, title, album)) = core::lyrics_query(&item) else {
        return op_fail("no lyrics for this item");
    };
    let name = label(&item);
    core.lyrics_loader.request(
        core.lyrics_client,
        name.clone(),
        &artist,
        &title,
        &album,
        None,
    );
    op_note(format!("fetching lyrics for {name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_now_line_says_what_is_queued_behind_it() {
        assert_eq!(
            now_line("BLADEE - FLATLINE", 0),
            "playing: BLADEE - FLATLINE (nothing queued behind it)"
        );
        assert_eq!(
            now_line("x", 1),
            "playing: x (1 queued behind it)",
            "singular, not '1 queued'"
        );
        assert_eq!(now_line("x", 7), "playing: x (7 queued behind it)");
    }

    #[test]
    fn every_decline_names_where_the_thing_does_exist() {
        // a refusal that does not say where to go is just a wall, and
        // one that says it twice reads like a bug.
        let line = no_pane("equaliser", "wim's 'e'");
        assert_eq!(line, "shellswim has no equaliser pane; wim's 'e' has it");
        assert_eq!(line.matches("has it").count(), 1, "said once: {line}");
        assert!(!line.contains("wim does"), "no doubled pointer: {line}");
    }

    #[test]
    fn the_generic_decline_still_points_at_wim() {
        // the `Surface` default, for anything shellswim has not given
        // its own wording to.
        let mut state = SwimSurface::default();
        let surf = state.borrowed();
        let (dispatch, report) = surf.decline("thing");
        assert_eq!(dispatch, Dispatch::Continue, "a decline is not fatal");
        let Report::Failed(text) = report else {
            panic!("a decline must read as a failure, not a note");
        };
        assert_eq!(text, "shellswim has no thing; wim does");
    }

    #[test]
    fn every_refusal_names_a_way_forward() {
        // a refusal that does not say what to do instead is a wall.
        // these are the exact strings the declining arms return, kept
        // as functions precisely so they can be asserted without a
        // live mpv behind a `CoreCtx`.
        assert!(NO_SELECTION.contains("play("), "{NO_SELECTION}");
        assert!(NO_BROWSE_ROW.contains("play("), "{NO_BROWSE_ROW}");
        assert!(
            no_page("Working On Dying").contains("queue(Working On Dying)"),
            "it must name the command that gets you the tracks"
        );
    }
}
