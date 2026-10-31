//! The player core: everything a shellsong `Op` can touch that is not
//! a pane. `wim` and `shellswim` both drive this; what they do not
//! share goes through `Surface`, whose default impls decline. Read
//! docs/library-seam.md before moving anything across that line.

use std::collections::HashSet;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::app::{self, Action, LoopMode};
use crate::cache::{self, CacheIndex};
use crate::download::{self, DownloadEvent};
use crate::notify::{Notifications, Severity};
use crate::playqueue::PlayQueue;
use crate::search::SearchItem;
use crate::shellsong::{self, Console, Op, Program, Target, ViewTarget};
use crate::shellsong_host::{self, Ramp, SearchSettle};
use crate::ui::controlsbar::Field;
use crate::ui::results_list::label;
use crate::{
    browse, config, eq, favorites, library, listenbrainz, lyrics, mpv, queues, subsonic, suggest,
    ui,
};

// MOVED_ITEMS_MARKER

/// What the core could not do by itself. Every method has a default
/// that declines with a one-line reason, so a front-end opts in to
/// exactly what it can honour and a new method cannot silently break
/// one. wim honours nearly all of it; shellswim honours what a shell
/// can show and declines the rest by name.
///
/// Passed alongside `CoreCtx` as a second, disjoint borrow: the
/// surface holds the front-end's pane locals, `CoreCtx` holds the
/// core's. See docs/library-seam.md for why this is a trait and not a
/// queue of requests.
pub trait Surface {
    /// Named in every decline, so the message says where the thing it
    /// is refusing does exist.
    fn front_end(&self) -> &'static str;

    /// `Target::Selection`: what is highlighted. wim resolves it the
    /// way its star/download keys do; a front-end with no highlight
    /// should answer with the head of the current result set, which is
    /// where wim's highlight starts.
    fn selection(&self, core: &CoreCtx) -> Option<SearchItem> {
        core.app.results.first().cloned()
    }

    /// `Focus` as the interpreter sees it; `go()` resolves against it.
    fn focus(&self) -> shellsong::Focus {
        shellsong::Focus::Sidebar
    }

    /// Whether a browse view is open, which widens what counts as a
    /// selection and which `go()` target is meant.
    fn browsing(&self) -> bool {
        false
    }

    /// One of the `ViewTarget::is_screen()` targets. The five that are
    /// not screens print into the console and never reach here.
    fn open_screen(&mut self, target: ViewTarget, core: &mut CoreCtx) -> (Dispatch, Report) {
        let _ = core;
        self.decline(format!("view({})", shellsong::view_name(target)))
    }

    fn move_selection(&mut self, step: SelectStep, core: &mut CoreCtx) -> (Dispatch, Report) {
        let _ = (step, core);
        self.decline("moving the selection")
    }

    /// `go()` with a browse view open: play from the highlighted row.
    fn go_browse_row(&mut self, core: &mut CoreCtx) -> (Dispatch, Report) {
        let _ = core;
        self.decline("playing from a browse row")
    }

    /// An album/playlist/artist opens its track listing rather than
    /// playing; `Action::Play` on one lands here.
    fn open_browse(&mut self, item: &SearchItem, core: &mut CoreCtx) -> (Dispatch, Report) {
        let _ = core;
        self.decline(format!("opening {}", label(item)))
    }

    /// Which bottom-bar control `go()` would press. `None` means
    /// nothing on the bar is focusable here.
    fn bar_field(&self) -> Option<Field> {
        None
    }

    /// Light a bar control for a moment. Pure feedback; a front-end
    /// with no such affordance ignores it rather than declining,
    /// because the op itself did happen.
    fn flash(&mut self, field: Field) {
        let _ = field;
    }

    /// Whether a standalone play takes over the full player pane.
    fn minimize_player(&mut self, minimize: bool) {
        let _ = minimize;
    }

    /// The queue view's cursor, for the two ops that have to reclamp
    /// it after shortening the queue under it.
    fn queue_cursor(&mut self) -> Option<&mut usize> {
        None
    }

    /// Dropped when an op takes over the screen; see
    /// `Op::closes_console`.
    fn close_console(&mut self) {}

    /// The one-line refusal. Shared so every decline reads the same.
    fn decline(&self, what: impl std::fmt::Display) -> (Dispatch, Report) {
        op_fail(format!("{} cannot do {what}; wim can", self.front_end()))
    }
}

/// Declines everything, which is what makes it the proof that the op
/// table needs no pane state: `core::tests` applies ops against this.
pub struct Headless;

impl Surface for Headless {
    fn front_end(&self) -> &'static str {
        "this front-end"
    }
}
