//! shellswim's half of the seam. It has no panes, so most of
//! `Surface` is a decline; the views it honours it honours by printing,
//! which is the only thing a shell can do with a screen.
//!
//! Why decline rather than silently no-op: `view(eq)` doing nothing
//! and `view(eq)` saying where the eq lives are very different
//! answers, and only one of them is true. See docs/library-seam.md.

use wim::core;
use wim::core::{op_fail, op_note, CoreCtx, Dispatch, Report, SelectStep, Surface};
use wim::search::SearchItem;
use wim::shellsong::{Op, Program, ViewTarget};
use wim::ui::results_list::label;

/// A printed list that the prompt can also open as a workspace, where
/// enter plays and `a` queues the selected row.
pub struct ListWorkspace {
    pub title: String,
    pub rows: Vec<(SearchItem, String)>,
}

/// What shellswim keeps that the core does not. Almost nothing, which
/// is the point: the list below is the entire difference between the
/// two front-ends' state.
#[derive(Default)]
pub struct SwimSurface {
    /// `search()` asked for its results to be printed; the host does
    /// it once they settle. See `Surface::show_results`.
    pub echo_results: bool,
    /// Whether the last `view(now)` asked for the full readout, so a
    /// second one does not repeat itself. Purely cosmetic.
    pub verbose_now: bool,
    /// The Shellswim host turns this request into its optional keybinds
    /// workspace after the current Shellsong step has returned.
    pub keybinds_requested: bool,
    /// Same hand-off for the equalizer editor.
    pub eq_requested: bool,
    /// Same hand-off for the settings editor.
    pub settings_requested: bool,
    /// Same hand-off for the undo tree.
    pub undo_requested: bool,
    /// Starred items or suggestions the prompt should open as a list.
    pub list_requested: Option<ListWorkspace>,
    /// Interactive prompt requested a live lyrics display. One-shot
    /// commands still print lyrics to stdout after the same lookup.
    pub lyrics_requested: bool,
    /// Ops the core handed over to be run as a program, waiting for
    /// the host to pick them up (`Host::step`).
    ///
    /// This slot is why `queue_program` is implemented at all: the
    /// trait default **drops** the ops, and `identity_settings` has
    /// already recorded them in `Overridden` as applied by then, so
    /// binding an eq, changing it by hand and playing an unbound track
    /// "restored" the curve that was replaced. wim's own surface holds
    /// the same slot; see docs/library-seam.md.
    pub queued: Option<Box<Program>>,
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
    /// Held rather than run here: the host owns the program slot, and
    /// `Host::step` only starts this once nothing else is running, so
    /// a track change cannot cut a typed program short.
    fn queue_program(&mut self, ops: Vec<Op>) {
        if !ops.is_empty() {
            self.state.queued = Some(Box::new(Program::of(ops)));
        }
    }

    fn show_results(&mut self) {
        // only a flag: the results are not in yet, and printing the
        // partial set would say "2 results" and then disagree with
        // itself a frame later.
        self.state.echo_results = true;
    }

    fn front_end(&self) -> &'static str {
        "shellswim"
    }

    /// There is no highlight to read, so the selection is the head of
    /// the current result set; that is where wim's highlight starts,
    /// so `play()` after a `search()` means the same thing in both.
    fn selection(&self, core: &CoreCtx) -> Option<SearchItem> {
        core.app.result(0).cloned()
    }

    /// `view(now)` and `view(lyrics)` are honoured as text; the rest
    /// are panes and are declined by name.
    fn open_screen(&mut self, target: ViewTarget, core: &mut CoreCtx) -> (Dispatch, Report) {
        match target {
            ViewTarget::Now => self.print_now(core),
            ViewTarget::Lyrics => {
                self.state.lyrics_requested = true;
                print_lyrics(core)
            }
            // `decline`'s generic wording would read "...; wim does"
            // after a message that already named the key, so these
            // say it once, themselves.
            ViewTarget::Eq => self.print_eq(core),
            ViewTarget::Settings => self.print_settings(core),
            ViewTarget::Keybinds => {
                self.state.keybinds_requested = true;
                let keymap = wim::keymap::Keymap::from_overrides(&core.cfg.keybinds);
                for action in wim::keymap::ALL_ACTIONS {
                    core.console.note(format!(
                        "{:>24}  {}",
                        action.label(),
                        wim::keymap::key_label(keymap.get(*action))
                    ));
                }
                op_note("keybinds · K opens the editor".to_owned())
            }
            ViewTarget::Undo => self.print_undo(core),
            ViewTarget::Suggestions => self.print_suggestions(core),
            ViewTarget::Starred => self.print_starred(core),
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
    fn print_starred(&mut self, core: &mut CoreCtx) -> (Dispatch, Report) {
        let starred: Vec<_> = core
            .app
            .corpus
            .items()
            .filter(|item| core::is_starred(item, core.favorites, core.remote_starred))
            .collect();
        if starred.is_empty() {
            return op_fail("nothing starred in the loaded library; star(item) to add one");
        }
        for item in &starred {
            core.console.note(label(item));
        }
        self.state.list_requested = Some(ListWorkspace {
            title: "starred".to_owned(),
            rows: starred
                .iter()
                .map(|item| ((*item).clone(), label(item)))
                .collect(),
        });
        op_note(format!(
            "{} starred items · use unstar(item) to remove",
            starred.len()
        ))
    }

    fn print_suggestions(&mut self, core: &mut CoreCtx) -> (Dispatch, Report) {
        let starred = core::suggestion_starred_keys(core.remote_starred, core.favorites);
        let catalog = wim::suggest::Catalog::from_corpus(&core.app.corpus);
        let cache = core.cache.lock().expect("cache mutex poisoned");
        let suggestions = wim::suggest::suggest(&wim::suggest::Request::new(
            &catalog,
            &cache,
            &starred,
            wim::cache::now_secs(),
        ));
        drop(cache);
        let count = wim::ui::suggestions::count(&suggestions);
        if count == 0 {
            let reason = if core.app.corpus.is_empty() {
                if core.app.scanning_library || core.app.syncing_servers {
                    "library is still loading; try suggestions again shortly"
                } else {
                    "no library yet; configure library_paths or a Navidrome server"
                }
            } else {
                "nothing to suggest yet; suggestions are based on what you play"
            };
            return op_fail(reason);
        }
        let mut rows = Vec::new();
        for section in &suggestions.sections {
            core.console.note(section.reason.title().to_string());
            for item in &section.items {
                core.console
                    .note(format!("  {} — {}", label(&item.item), item.reason_text));
                rows.push((
                    item.item.clone(),
                    format!("{} — {}", label(&item.item), item.reason_text),
                ));
            }
        }
        self.state.list_requested = Some(ListWorkspace {
            title: "suggestions".to_owned(),
            rows,
        });
        op_note(format!("{count} suggestions"))
    }

    fn print_eq(&mut self, core: &mut CoreCtx) -> (Dispatch, Report) {
        self.state.eq_requested = true;
        let source = core
            .app
            .eq_source
            .and_then(|source| wim::eq::source_name(source, &core.cfg.eq_custom_presets))
            .unwrap_or("custom curve");
        core.console.note(format!(
            "equalizer: {} · {}",
            if core.cfg.eq_enabled { "on" } else { "off" },
            source
        ));
        for (hz, gain) in wim::eq::BAND_HZ.iter().zip(core.app.eq_gains) {
            core.console.note(format!("{hz:>5.0} Hz  {gain:+.1} dB"));
        }
        let presets = wim::eq::PRESETS
            .iter()
            .map(|preset| preset.name)
            .chain(
                core.cfg
                    .eq_custom_presets
                    .iter()
                    .map(|preset| preset.name.as_str()),
            )
            .collect::<Vec<_>>()
            .join(", ");
        core.console.note(format!("presets: {presets}"));
        op_note("choose a curve with eq(name) · E opens the editor".to_owned())
    }

    fn print_undo(&mut self, core: &mut CoreCtx) -> (Dispatch, Report) {
        self.state.undo_requested = true;
        let rows = core.undo.rows(core.now);
        if rows.is_empty() {
            return op_fail("nothing has been done yet");
        }
        for row in rows {
            let branch = if row.depth == 0 {
                String::new()
            } else if row.last_child {
                format!("{}└─", "  ".repeat(row.depth - 1))
            } else {
                format!("{}├─", "  ".repeat(row.depth - 1))
            };
            let current = if row.current { "●" } else { " " };
            let fork = if row.branches > 1 {
                format!(" ({} branches)", row.branches)
            } else {
                String::new()
            };
            let barrier = row
                .barrier
                .map(|why| format!(" · barrier: {why}"))
                .unwrap_or_default();
            core.console
                .note(format!("{current} {branch}{}{fork}{barrier}", row.label));
        }
        op_note("undo history · u/r walk it, h/l cross branches in the tree view".to_owned())
    }

    fn print_settings(&mut self, core: &mut CoreCtx) -> (Dispatch, Report) {
        self.state.settings_requested = true;
        let cfg = core.cfg;
        let lines = [
            format!("equalizer: {} · {} visualizer bands", on_off(cfg.eq_enabled), cfg.visualizer_bands),
            format!("crossfade: {} · {:.1}s", on_off(*core.crossfade_enabled), cfg.crossfade_seconds),
            format!("between tracks: {:.1}s", cfg.gap_seconds),
            format!("offline cache: {} bytes · download after {} distinct days", cfg.offline_cache_max_bytes, cfg.cache_days_threshold),
            format!("cache eviction: {} days", cfg.cache_eviction_days),
            format!("download quality: {} · {} kbps", cfg.offline_download_format, cfg.effective_offline_download_bitrate_kbps()),
            format!("scrobble: {:.0}% or {:.0}s", cfg.scrobble.threshold_percent, cfg.scrobble.threshold_seconds),
            format!("lyrics sources: {}", cfg.lyrics.providers.join(", ")),
            format!("subtitle style: {}px · position {} · delay {:+.1}s", cfg.subtitles.font_size, cfg.subtitles.position, cfg.subtitles.delay_secs),
            "change playback with volume(...), gap(...), crossfade(), eq(...); see help() for the full command list".to_owned(),
        ];
        for line in lines {
            core.console.note(line);
        }
        op_note("current settings · the settings key opens the editor".to_owned())
    }

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

fn on_off(value: bool) -> &'static str {
    if value {
        "on"
    } else {
        "off"
    }
}

/// The three refusals, as values rather than inline strings: they are
/// the user-facing half of the seam, so they are asserted directly.
const NO_SELECTION: &str = "nothing is highlighted here; name what you mean, e.g. play(crest)";
const NO_BROWSE_ROW: &str = "no browse view here; play(album name) plays the whole thing";

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
    if let Some((server, id)) = core::lyrics_remote(&item) {
        core.lyrics_loader.request_subsonic(
            core.lyrics_client,
            server,
            id,
            name.clone(),
            &artist,
            &title,
            &album,
            None,
        );
    } else if let Some(path) = core::lyrics_path(&item) {
        core.lyrics_loader.request_local(
            core.lyrics_client,
            name.clone(),
            &artist,
            &title,
            &album,
            None,
            path,
        );
    } else {
        core.lyrics_loader.request(
            core.lyrics_client,
            name.clone(),
            &artist,
            &title,
            &album,
            None,
        );
    }
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
    fn starred_snapshot_has_an_actionable_empty_state() {
        let text = "nothing starred in the loaded library; star(item) to add one";
        assert!(text.contains("star(item)"));
        assert!(text.contains("loaded library"));
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

/// `queue_program` is the one `Surface` method whose trait default
/// loses data rather than merely declining.
#[cfg(test)]
mod queued_program {
    use super::*;
    use wim::shellsong::{Level, Op};

    /// The gap this closed: the default implementation drops the ops,
    /// and by the time it is called `identity_settings` has already
    /// recorded them in `Overridden` as applied. So binding an eq,
    /// changing it by hand and then playing an unbound track
    /// "restored" the curve that had been replaced. Driven through the
    /// trait, not the field, because the trait default was the bug.
    #[test]
    fn the_core_can_hand_a_program_over_to_the_host() {
        let mut swim = SwimSurface::default();
        assert!(swim.queued.is_none(), "nothing queued to begin with");

        {
            let mut surf: Box<dyn Surface> = Box::new(swim.borrowed());
            surf.queue_program(vec![Op::SetVolume(Level::Absolute(25.0))]);
        }

        // `Program`'s node count is private, so what is asserted is
        // the contract that was broken: the ops arrive somewhere the
        // host can reach rather than being dropped on the floor.
        assert!(
            swim.queued.is_some(),
            "the ops went nowhere, which is the trait default's behaviour"
        );
    }

    /// An empty hand-over is not a program. `track_started` returns an
    /// empty `ops` for every unbound track, which is almost all of
    /// them, and a queued empty program would make `Host::step` run a
    /// no-op frame on every single track change.
    #[test]
    fn nothing_bound_queues_nothing() {
        let mut swim = SwimSurface::default();
        {
            let mut surf: Box<dyn Surface> = Box::new(swim.borrowed());
            surf.queue_program(Vec::new());
        }
        assert!(swim.queued.is_none());
    }
}
