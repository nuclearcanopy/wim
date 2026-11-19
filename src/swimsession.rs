//! The UI-free half of shellswim's host: everything the prompt and a
//! one-shot `-c` share that does not touch a terminal, so the mobile
//! front-ends can drive the same session.
//!
//! The bin's `Host` holds one of these and keeps what is terminal- or
//! process-bound: the TUI, the TorBox wiring, the lyrics pane, the
//! instance lock and the mpv child. Playback goes through `&dyn Player`
//! arguments, never an mpv handle owned here.

use crate::catalog::{self, BackgroundSyncResult};
use crate::shellsong::{Console, LineKind, Program};
use crate::swimsurface::SwimSurface;
use crate::{app, cache, config, library, search};
use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Where a submitted program goes: straight to the active slot, or
/// behind whatever is running or already waiting. `Some` means "run it
/// now".
pub fn place_submitted_program(
    active: bool,
    core_waiting: bool,
    waiting: &mut VecDeque<Box<Program>>,
    program: Box<Program>,
) -> Option<Box<Program>> {
    if active || core_waiting || !waiting.is_empty() {
        waiting.push_back(program);
        None
    } else {
        Some(program)
    }
}

pub struct SwimSession {
    pub cfg: config::Config,
    pub app: app::App,
    pub cache_index: Arc<Mutex<cache::CacheIndex>>,
    startup_cache: Option<Receiver<cache::CacheIndex>>,
    warming_corpus: Option<search::WarmingCorpus>,
    cached_entries: Option<search::CachedEntries>,
    pub scan_rx: Receiver<library::ScanBatch>,
    pub sync_rx: Receiver<BackgroundSyncResult>,
    pub servers_tx: mpsc::Sender<Vec<config::NavidromeConfig>>,
    pub server_reachable: HashMap<String, bool>,
    /// What the core hands over to the front-end (`SwimSurface`).
    pub swim: SwimSurface,
    pub console: Console,
    /// The live `let` bindings and the per-identity settings, both
    /// read once here and written back by the ops that change them.
    pub vars: crate::shellsong::Scope,
    pub program: Option<Box<Program>>,
    queued_programs: VecDeque<Box<Program>>,
    pub start: Instant,
}

impl SwimSession {
    /// Nothing here blocks on the library or the network: the scan and
    /// the catalog warm on their own threads.
    pub fn new(cfg: config::Config) -> SwimSession {
        let startup_cache = Some(cache::spawn_startup(
            cfg.offline_cache_max_bytes,
            Some(cfg.cache_eviction_days),
        ));
        // `rescan()` replaces this channel wholesale: the scan thread
        // signs off by dropping its sender, so a finished one cannot
        // be reused.
        let (scan_tx, scan_rx) = mpsc::channel::<library::ScanBatch>();
        library::spawn_scan(cfg.library_paths.clone(), scan_tx);
        // last session's catalog, off disk, so the first prompt can
        // already complete track names.
        let warming_corpus = Some(search::spawn_warm_from_cache(&cfg.navidrome_servers));
        let (sync_tx, sync_rx) = mpsc::channel::<BackgroundSyncResult>();
        let (servers_tx, servers_rx) = mpsc::channel();
        catalog::spawn_background_sync(cfg.navidrome_servers.clone(), sync_tx, servers_rx);

        let mut app = app::App::new(search::Corpus::new());
        app.scanning_library = true;
        app.syncing_servers = !cfg.navidrome_servers.is_empty();
        app.loading_cached_catalog = !cfg.navidrome_servers.is_empty();
        let restored_eq = crate::eq::restore(
            cfg.eq_preset.as_deref(),
            cfg.eq_gains,
            &cfg.eq_custom_presets,
        );
        app.eq_source = restored_eq.source;
        app.eq_custom = restored_eq.modified;
        app.eq_gains = restored_eq.gains;
        if let Some(crate::eq::Source::Builtin(index)) = restored_eq.source {
            app.eq_preset = index;
        }
        SwimSession {
            cfg,
            app,
            cache_index: Arc::new(Mutex::new(cache::CacheIndex::default())),
            startup_cache,
            warming_corpus,
            cached_entries: None,
            scan_rx,
            sync_rx,
            servers_tx,
            server_reachable: HashMap::new(),
            swim: SwimSurface::default(),
            console: Console::new(),
            vars: crate::shellsong_host::load_vars(),
            program: None,
            queued_programs: VecDeque::new(),
            start: Instant::now(),
        }
    }

    pub fn poll_startup_cache(&mut self) -> anyhow::Result<bool> {
        let Some(rx) = &self.startup_cache else {
            return Ok(true);
        };
        match rx.try_recv() {
            Ok(index) => {
                *self
                    .cache_index
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = index;
                self.startup_cache = None;
                crate::trace_mark!("playback cache ready");
                Ok(true)
            }
            Err(TryRecvError::Empty) => Ok(false),
            Err(TryRecvError::Disconnected) => anyhow::bail!("playback cache startup failed"),
        }
    }

    pub fn wait_startup_cache(&mut self) -> anyhow::Result<()> {
        if let Some(rx) = self.startup_cache.take() {
            let index = rx
                .recv()
                .map_err(|_| anyhow::anyhow!("playback cache startup failed"))?;
            *self
                .cache_index
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = index;
        }
        Ok(())
    }

    /// One-shots have no prompt to show while a catalog warms, and must
    /// search the same cached library they did before startup was split.
    pub fn wait_startup_catalog(&mut self) {
        if let Some(warming) = self.warming_corpus.take() {
            let mut cached = warming.take().into_cached_entries();
            self.app
                .corpus
                .merge_cached_batch(&mut cached, usize::MAX, |client| {
                    self.cfg
                        .navidrome_servers
                        .iter()
                        .any(|nav| client.matches(nav))
                });
            self.app.resync_results();
        }
        self.app.loading_cached_catalog = false;
    }

    /// Warmed catalogs and local scans do not depend on mpv becoming ready.
    pub fn pump_library(&mut self) {
        if self
            .warming_corpus
            .as_ref()
            .is_some_and(|warm| warm.is_finished())
        {
            self.cached_entries = self
                .warming_corpus
                .take()
                .map(|warm| warm.take().into_cached_entries());
        }
        let mut changed = false;
        if let Some(cached) = &mut self.cached_entries {
            changed |= self.app.corpus.merge_cached_batch(
                cached,
                search::CACHED_ROWS_PER_FRAME,
                |client| {
                    self.cfg
                        .navidrome_servers
                        .iter()
                        .any(|nav| client.matches(nav))
                },
            ) > 0;
            if cached.is_empty() {
                self.cached_entries = None;
                self.app.loading_cached_catalog = false;
            }
        }
        for _ in 0..8 {
            match self.scan_rx.try_recv() {
                Ok(batch) => {
                    self.app.corpus.add_local_tracks(batch);
                    changed = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.app.scanning_library = false;
                    break;
                }
            }
        }
        if changed {
            self.app.resync_results();
        }
    }

    pub fn has_cached_rows_pending(&self) -> bool {
        self.cached_entries.is_some()
    }

    pub fn now(&self) -> Duration {
        self.start.elapsed()
    }

    /// Retain every submitted program. Shellsong programs may await a
    /// search or remote operation for several frames, so replacing the
    /// active slot would silently discard the user's earlier command.
    pub fn submit_program(&mut self, program: Box<Program>) {
        match place_submitted_program(
            self.program.is_some(),
            self.swim.queued.is_some(),
            &mut self.queued_programs,
            program,
        ) {
            Some(program) => self.program = Some(program),
            None => self.console.note(format!(
                "queued command ({} waiting)",
                self.queued_programs.len()
            )),
        }
    }

    pub fn has_program_work(&self) -> bool {
        self.program.is_some() || !self.queued_programs.is_empty() || self.swim.queued.is_some()
    }

    /// Drops everything waiting, for a one-shot that gave up.
    pub fn abandon_programs(&mut self) {
        self.program = None;
        self.queued_programs.clear();
        self.swim.queued = None;
    }

    /// Starts the next program if nothing is active: the one the core
    /// queued, else the oldest waiting submission.
    pub fn take_next_program(&mut self) {
        if self.program.is_none() {
            self.program = self
                .swim
                .queued
                .take()
                .or_else(|| self.queued_programs.pop_front());
        }
    }

    /// Parses a line against the live bindings and script directory,
    /// the same environment used by interactive submission.
    pub fn begin(&mut self, line: &str) {
        self.console.push(LineKind::Command, line.to_string());
        let read = |name: &str| crate::shellsong_host::read_script(name);
        let world = crate::shellsong::World {
            scope: &self.vars,
            read: Some(&read),
        };
        match Program::parse_in(line, &world) {
            Ok(p) => self.submit_program(Box::new(p)),
            Err(e) => self.console.fail(e.to_string()),
        }
    }
}

#[cfg(test)]
mod program_queue_tests {
    use super::*;
    use crate::shellsong::Op;

    #[test]
    fn a_submission_waits_behind_the_active_program_without_replacing_it() {
        let mut waiting = VecDeque::new();
        let submitted = Box::new(Program::of(vec![Op::ClearConsole]));
        let submitted_ptr = (&*submitted) as *const Program;
        let active = place_submitted_program(true, false, &mut waiting, submitted);

        assert!(active.is_none());
        assert_eq!(waiting.len(), 1);
        assert_eq!(
            (&*waiting.pop_front().unwrap()) as *const Program,
            submitted_ptr
        );
    }

    #[test]
    fn a_submission_waits_behind_a_track_binding_even_when_idle() {
        let mut waiting = VecDeque::new();
        let submitted = Box::new(Program::of(vec![Op::ClearConsole]));

        assert!(place_submitted_program(false, true, &mut waiting, submitted).is_none());
        assert_eq!(waiting.len(), 1);
    }

    #[test]
    fn later_submissions_keep_fifo_order_behind_core_work() {
        let mut waiting = VecDeque::new();
        let first = Box::new(Program::of(vec![Op::ClearConsole]));
        let second = Box::new(Program::of(vec![Op::ClearQueue]));
        let first_ptr = (&*first) as *const Program;
        let second_ptr = (&*second) as *const Program;

        assert!(place_submitted_program(false, true, &mut waiting, first).is_none());
        assert!(place_submitted_program(false, false, &mut waiting, second).is_none());
        assert_eq!(waiting.len(), 2);
        assert_eq!(
            (&*waiting.pop_front().unwrap()) as *const Program,
            first_ptr
        );
        assert_eq!(
            (&*waiting.pop_front().unwrap()) as *const Program,
            second_ptr
        );
    }
}
