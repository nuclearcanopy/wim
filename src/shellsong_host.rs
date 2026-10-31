//! The host side of shellsong: the few decisions `main.rs` has to make
//! for the language that are worth having apart from the event loop.
//! Everything here is pure, so each one is testable with no terminal,
//! no mpv and no network; `main.rs` keeps only the ipc writes and the
//! state mutations. The per-op application table lives there, next to
//! `dispatch_action`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::search::Corpus;
use crate::shellsong::{Candidates, Level, Pos, Scope};
use crate::subsonic::Client;

// -------------------------------------------------------- variables

/// Where `let name = [...]` lives: one toml map, not a file each.
/// `queues.rs` gives a saved queue its own document because an entry
/// carries server credentials and a title; a variable is a handful of
/// short strings, and the whole set is read on startup and written on
/// every change, so one file is one read.
const VARS_FILE: &str = "vars.toml";

fn vars_path() -> Option<PathBuf> {
    crate::shellsong::state_path(VARS_FILE)
}

/// Best-effort, the same leniency the history and rank files take: an
/// unreadable state dir costs you your variables, not your session.
pub fn load_vars() -> Scope {
    vars_path().map(|p| read_vars(&p)).unwrap_or_default()
}

pub fn save_vars(scope: &Scope) {
    if let Some(path) = vars_path() {
        write_vars(&path, scope);
    }
}

/// Split from `load_vars` so a test can point it at a temp file; the
/// path resolution is the only part that is not testable.
fn read_vars(path: &Path) -> Scope {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| toml::from_str(&raw).ok())
        .unwrap_or_default()
}

fn write_vars(path: &Path, scope: &Scope) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(body) = toml::to_string_pretty(scope) {
        let _ = std::fs::write(path, body);
    }
}

// ---------------------------------------------------------- scripts

/// Script extension. One per file, shellsong lines, `#` comments.
const SHG: &str = "shg";

/// The one script that runs by itself, at startup.
pub const INIT_SCRIPT: &str = "init";

/// Where a bare `run(name)` looks: next to `config.toml`, not in the
/// state dir. A script is something you write and keep, the same class
/// of thing the config is, where history and ranks are not.
fn script_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("wim"))
}

/// `.shg` names in the config dir, alphabetical; the `run(...)`
/// completion candidates. Same best-effort shape as `queues::list`.
pub fn script_names() -> Vec<String> {
    let Some(read) = script_dir().and_then(|d| std::fs::read_dir(d).ok()) else {
        return Vec::new();
    };
    let mut names: Vec<String> = read
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let path = e.path();
            (path.extension()? == SHG)
                .then(|| path.file_stem()?.to_str().map(str::to_string))
                .flatten()
        })
        .collect();
    names.sort();
    names
}

/// Whether `init.shg` is there to run at startup.
pub fn has_init_script() -> bool {
    script_path(INIT_SCRIPT).is_some_and(|p| p.exists())
}

/// Resolves a `run(...)` argument. A bare name is a script in the
/// config dir (the `load_queue(name)` shape: a named thing, not a
/// path); anything with a separator, a leading `~` or a leading `.` is
/// taken as a path, so a script can live wherever you keep it.
fn script_path(name: &str) -> Option<PathBuf> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    if let Some(rest) = name.strip_prefix("~/") {
        return dirs::home_dir().map(|h| h.join(rest));
    }
    if is_path(name) {
        return Some(PathBuf::from(name));
    }
    script_dir().map(|d| d.join(format!("{name}.{SHG}")))
}

/// Whether the argument was written as a path rather than as a name.
/// Pure and tested; the two spellings get different "not found"
/// messages, since only a name has a list to suggest from.
fn is_path(name: &str) -> bool {
    name.contains('/') || name.starts_with('.') || name.starts_with('~')
}

/// Reads one script for `run(...)`. `Err` is already phrased for
/// whoever typed it, since only this side knows how a name resolves and
/// which scripts exist; a bare name that missed goes through
/// `namehint::unknown` like every other runtime name.
pub fn read_script(name: &str) -> Result<String, String> {
    let Some(path) = script_path(name) else {
        return Err(format!("'{name}' is not a usable script name"));
    };
    match std::fs::read_to_string(&path) {
        Ok(source) => Ok(source),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !is_path(name) => {
            let names = script_names();
            Err(crate::namehint::unknown(
                "script",
                name,
                names.iter().map(String::as_str),
            ))
        }
        Err(e) => Err(format!("couldn't read {}: {e}", path.display())),
    }
}

// ------------------------------------------------------- completion

/// Completion candidates off the live corpus. `Corpus::search` is
/// in-memory by construction (it scores `Corpus`'s own items and makes
/// no request), which is the whole reason it is what backs this:
/// completion runs on every keystroke, and anything that reached for
/// `search3` here would spam the server a request per character.
pub struct CorpusCandidates<'a> {
    pub corpus: &'a Corpus,
    /// Snapshotted when the console opens and after a `save_queue`,
    /// not read off disk here: this is called per keystroke too.
    pub saved: &'a [String],
    /// The user's saved eq curve names, so `eq()` completes to them as
    /// well as to the built-ins.
    pub eq_presets: Vec<String>,
    /// The live variables: what a submitted line is expanded against,
    /// and what the variable-name parameters complete from.
    pub vars: &'a Scope,
    /// `.shg` names behind `run(...)`'s completion; snapshotted on
    /// console open, same as `saved`, since listing them is a directory
    /// read and this is called per keystroke.
    pub scripts: &'a [String],
}

/// Matches offered per keystroke. `Console` caps the list again at its
/// own `MAX_CANDIDATES`; stopping here keeps the clone count down on a
/// one-character token that matches half the library.
const MAX_CORPUS_CANDIDATES: usize = 50;

impl Candidates for CorpusCandidates<'_> {
    fn corpus(&self, token: &str) -> Vec<String> {
        // `search_names` rather than `search`: same ranking, but it
        // stops at the cap and never clones a `SearchItem`, which on a
        // wide token is thousands of them per keystroke. Deliberately
        // the bare name, not `results_list::label`'s "title · artist":
        // a candidate is inserted back into the command line, so the
        // decorated form would never be a prefix extension of what was
        // typed and the completer would drop every one of them.
        self.corpus.search_names(token, MAX_CORPUS_CANDIDATES)
    }

    fn saved_queues(&self) -> Vec<String> {
        self.saved.to_vec()
    }

    fn eq_presets(&self) -> Vec<String> {
        self.eq_presets.clone()
    }

    fn scope(&self) -> &Scope {
        self.vars
    }

    fn scripts(&self) -> Vec<String> {
        self.scripts.to_vec()
    }

    /// The one method here that touches the disk, which is fine: it is
    /// called once per submitted line, not per keystroke.
    fn read_script(&self, name: &str) -> Result<String, String> {
        read_script(name)
    }
}

/// The live clients a saved queue's remote entries can be re-bound to.
/// Thin wrapper so `queues::load`'s call site reads as one thing.
pub fn clients_of(corpus: &Corpus) -> Vec<Arc<Client>> {
    corpus.remote_clients()
}

// ---------------------------------------------------- search_settled

/// How long a notified query stays unsettled with nothing coming back.
/// Comfortably past the 300ms remote-search debounce plus a round trip,
/// and far inside `shellsong`'s own 8s await timeout, so a slow server
/// costs `play(crest)` a beat rather than an error.
pub const SETTLE_WINDOW: Duration = Duration::from_millis(1200);

/// `Env::search_settled`: "the results for the current query are as
/// complete as they are going to get".
///
/// A query is only ever unsettled because a remote `search3` was
/// actually fired for it, which is also why `notified` takes whether
/// one was: `notify_query_changed` is a no-op with no indexed server,
/// so with no remote clients there is nothing to wait for and the
/// query settles the instant it changes. Otherwise the first outcome
/// off the channel settles it, and the window settles it regardless so
/// a server that never answers cannot wedge a program.
///
/// Deliberately "first answer wins" rather than counting one outcome
/// per server: outcomes carry no query tag and a failed server sends
/// nothing at all (see `spawn_background_search`), so there is no
/// honest count to wait for. A second server's late answer still gets
/// merged into the corpus; it just does not hold the program up.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SearchSettle {
    /// When the outstanding query was notified; `None` when nothing is
    /// outstanding, which is the settled state.
    pending_since: Option<Duration>,
}

impl SearchSettle {
    /// The query changed. `fired` is whether a remote search actually
    /// went out for it.
    pub fn notified(&mut self, fired: bool, now: Duration) {
        self.pending_since = fired.then_some(now);
    }

    /// An outcome arrived off `remote_search_result_rx`.
    pub fn answered(&mut self) {
        self.pending_since = None;
    }

    pub fn settled(&self, now: Duration) -> bool {
        match self.pending_since {
            None => true,
            Some(since) => now.saturating_sub(since) >= SETTLE_WINDOW,
        }
    }
}

// --------------------------------------------------------- positions

/// Resolves a `moveto(...)` position to absolute seconds. A percent
/// stays a percent all the way through the language precisely because
/// the duration is runtime state; this is where it stops being one.
pub fn seek_target(pos: Pos, duration: Option<f64>) -> Result<f64, &'static str> {
    match pos {
        Pos::Seconds(s) => Ok(s.max(0.0)),
        Pos::Percent(p) => match duration {
            Some(d) if d > 0.0 => Ok((d * p / 100.0).clamp(0.0, d)),
            _ => Err("nothing with a known duration is playing, so a percent has nothing to resolve against"),
        },
    }
}

// ----------------------------------------------------------- volume

/// The real ceiling, not a conservative one: above 100 mpv applies
/// digital gain, which clips. `volume(100%)` is how you ask for the
/// top.
pub const VOLUME_CEILING: f64 = 100.0;

/// Where `volume(...)` lands. An absolute level is already refused
/// above 100 at parse time; a nudge applies to mpv's current volume
/// and is clamped here, which is the only place that can know it.
pub fn volume_target(level: Level, current: f64) -> f64 {
    let wanted = match level {
        Level::Absolute(v) => v,
        Level::Delta(d) => current + d,
    };
    wanted.clamp(0.0, VOLUME_CEILING)
}

// ------------------------------------------------------------ ramps

/// A linear volume ramp: the same `base * t` shape the crossfade tick
/// already uses, driven by shellsong's `fade(...)` instead of by a
/// track boundary. Pure; the caller does the one `set_volume` write
/// per frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ramp {
    pub from: f64,
    pub to: f64,
    pub started: Duration,
    pub over: Duration,
}

impl Ramp {
    pub fn new(from: f64, to: f64, started: Duration, over: Duration) -> Ramp {
        Ramp {
            from,
            to,
            started,
            over,
        }
    }

    fn progress(&self, now: Duration) -> f64 {
        if self.over.is_zero() {
            return 1.0;
        }
        (now.saturating_sub(self.started).as_secs_f64() / self.over.as_secs_f64()).clamp(0.0, 1.0)
    }

    pub fn level_at(&self, now: Duration) -> f64 {
        let t = self.progress(now);
        self.from + (self.to - self.from) * t
    }

    pub fn finished(&self, now: Duration) -> bool {
        self.progress(now) >= 1.0
    }
}

/// What a pending `sleep(...)` should do this frame. Fading rather than
/// cutting the audio dead is the point of the command; the ramp is the
/// crossfade fade-out's, read off time remaining instead of time into
/// the track.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SleepStep {
    /// not close enough to fade yet.
    Waiting,
    /// set the volume to this and keep waiting.
    Fading(f64),
    /// the ramp is done: pause, and put the volume back.
    Expired,
}

pub fn sleep_step(due: Duration, fade: Duration, base: f64, now: Duration) -> SleepStep {
    if now >= due {
        return SleepStep::Expired;
    }
    let remaining = due.saturating_sub(now);
    if remaining > fade || fade.is_zero() {
        return SleepStep::Waiting;
    }
    let t = remaining.as_secs_f64() / fade.as_secs_f64();
    SleepStep::Fading(base * t.clamp(0.0, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::{LocalTrack, MediaKind};
    use crate::shellsong::Console;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::path::PathBuf;

    /// Borrowed rather than built per call site: `CorpusCandidates`
    /// holds a `&Scope`, so a temporary would not outlive the literal.
    fn no_vars() -> &'static Scope {
        static EMPTY: std::sync::LazyLock<Scope> = std::sync::LazyLock::new(Scope::default);
        &EMPTY
    }

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    // ----------------------------------------------------- variables

    /// A temp path per run, the same way `persistence.rs`'s test does
    /// it: the real `vars_path` is not injectable, so only the reader
    /// and the writer are exercised here.
    fn temp_vars_path(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("wim-vars-{tag}-{nanos}.toml"))
    }

    #[test]
    fn variables_round_trip_through_the_state_file() {
        let path = temp_vars_path("round-trip");
        let mut scope = Scope::default();
        scope.define(
            "bladee_classics",
            vec!["crest".into(), "icedancer".into(), "gods".into()],
        );
        scope.define("quiet", vec!["mazzy star".into()]);
        write_vars(&path, &scope);
        let back = read_vars(&path);
        assert_eq!(back, scope);
        assert_eq!(back.names(), vec!["bladee_classics", "quiet"]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_missing_or_corrupt_vars_file_reads_as_no_variables() {
        // best-effort, like the history file: an unreadable state dir
        // costs you your variables, not your session.
        assert!(read_vars(Path::new("/nonexistent/wim/vars.toml")).is_empty());
        let path = temp_vars_path("corrupt");
        std::fs::write(&path, "this is not toml [[[").expect("temp write");
        assert!(read_vars(&path).is_empty());
        let _ = std::fs::remove_file(&path);
    }

    fn millis(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    // ------------------------------------------------- search_settled

    #[test]
    fn nothing_outstanding_is_settled() {
        let settle = SearchSettle::default();
        assert!(settle.settled(secs(0)));
    }

    #[test]
    fn with_no_remote_clients_a_query_settles_immediately() {
        // `notify_query_changed` doesn't send at all without an
        // indexed server, so there is nothing to wait for; waiting
        // anyway would make every `play(x)` on a local-only library
        // sit out the whole window for no reason.
        let mut settle = SearchSettle::default();
        settle.notified(false, secs(10));
        assert!(settle.settled(secs(10)));
    }

    #[test]
    fn a_notified_query_is_unsettled_until_an_outcome_lands() {
        let mut settle = SearchSettle::default();
        settle.notified(true, secs(10));
        assert!(!settle.settled(secs(10)));
        assert!(!settle.settled(secs(10) + millis(400)));
        settle.answered();
        assert!(settle.settled(secs(10) + millis(400)));
    }

    #[test]
    fn a_notified_query_settles_on_the_window_even_with_no_answer() {
        // a wedged or unreachable server must cost a beat, not an
        // error: the language's own await timeout is 8s, and timing
        // out there reads as "the search subsystem is broken" rather
        // than "that server is slow".
        let mut settle = SearchSettle::default();
        settle.notified(true, secs(10));
        assert!(!settle.settled(secs(10) + SETTLE_WINDOW - millis(1)));
        assert!(settle.settled(secs(10) + SETTLE_WINDOW));
        assert!(settle.settled(secs(30)));
    }

    #[test]
    fn a_newer_query_restarts_the_window() {
        // typing on into a second query must not inherit the first
        // one's nearly-elapsed window and settle instantly on stale
        // results, which is the exact bug `search_settled` exists for.
        let mut settle = SearchSettle::default();
        settle.notified(true, secs(10));
        settle.notified(true, secs(11));
        assert!(!settle.settled(secs(11) + SETTLE_WINDOW - millis(1)));
        assert!(settle.settled(secs(11) + SETTLE_WINDOW));
    }

    #[test]
    fn the_window_clears_the_debounce_plus_a_round_trip() {
        assert!(SETTLE_WINDOW >= millis(300) * 2);
    }

    // ----------------------------------------------------- candidates

    fn local_corpus(titles: &[&str]) -> Corpus {
        let mut corpus = Corpus::new();
        corpus.add_local_tracks(titles.iter().map(|t| LocalTrack {
            path: PathBuf::from(format!("/music/{t}.flac")),
            title: (*t).to_string(),
            artist: Some("bladee".to_string()),
            album: None,
            duration_secs: None,
            kind: MediaKind::Music,
        }));
        corpus
    }

    #[test]
    fn candidates_are_bare_corpus_names() {
        let corpus = local_corpus(&["flatline", "hex"]);
        let cand = CorpusCandidates {
            corpus: &corpus,
            saved: &[],
            eq_presets: Vec::new(),
            vars: no_vars(),
            scripts: &[],
        };
        let got = cand.corpus("flat");
        // the bare title, not "flatline · bladee": a decorated label
        // could never be a prefix extension of "flat", so the
        // completer would discard it and the ghost would be empty.
        assert!(got.contains(&"flatline".to_string()), "got {got:?}");
        assert!(!got.iter().any(|n| n.contains('·')));
    }

    #[test]
    fn candidates_never_reach_the_network() {
        // "local only" is structural rather than a promise: the only
        // thing a remote `search3` could be fired through is a
        // `Corpus` client, and a corpus of local files has none. The
        // provider also calls nothing but `Corpus::search`, which is
        // in-memory scoring.
        let corpus = local_corpus(&["flatline", "hex", "crest"]);
        assert!(corpus.remote_clients().is_empty());
        let cand = CorpusCandidates {
            corpus: &corpus,
            saved: &[],
            eq_presets: Vec::new(),
            vars: no_vars(),
            scripts: &[],
        };
        assert_eq!(cand.corpus("hex"), vec!["hex".to_string()]);
    }

    #[test]
    fn candidates_are_deduped_and_capped() {
        // every track shares one album name, so an album-name query
        // would otherwise return the same string once per track.
        let mut corpus = Corpus::new();
        corpus.add_local_tracks((0..80).map(|i| LocalTrack {
            path: PathBuf::from(format!("/music/{i}.flac")),
            title: "same title".to_string(),
            artist: None,
            album: None,
            duration_secs: None,
            kind: MediaKind::Music,
        }));
        let cand = CorpusCandidates {
            corpus: &corpus,
            saved: &[],
            eq_presets: Vec::new(),
            vars: no_vars(),
            scripts: &[],
        };
        assert_eq!(cand.corpus("same"), vec!["same title".to_string()]);
    }

    #[test]
    fn an_empty_token_offers_nothing() {
        let corpus = local_corpus(&["flatline"]);
        let cand = CorpusCandidates {
            corpus: &corpus,
            saved: &[],
            eq_presets: Vec::new(),
            vars: no_vars(),
            scripts: &[],
        };
        assert!(cand.corpus("").is_empty());
    }

    #[test]
    fn saved_queues_come_from_the_snapshot_not_the_disk() {
        let corpus = Corpus::new();
        let saved = vec!["late-night".to_string()];
        let cand = CorpusCandidates {
            corpus: &corpus,
            saved: &saved,
            eq_presets: Vec::new(),
            vars: no_vars(),
            scripts: &[],
        };
        assert_eq!(cand.saved_queues(), saved);
    }

    // ------------------------------------------------------ positions

    #[test]
    fn a_percent_resolves_against_the_duration() {
        assert_eq!(seek_target(Pos::Percent(50.0), Some(200.0)), Ok(100.0));
        assert_eq!(seek_target(Pos::Percent(0.0), Some(200.0)), Ok(0.0));
        assert_eq!(seek_target(Pos::Percent(100.0), Some(200.0)), Ok(200.0));
        assert_eq!(seek_target(Pos::Percent(12.5), Some(80.0)), Ok(10.0));
    }

    #[test]
    fn seconds_go_straight_through_with_no_duration_needed() {
        assert_eq!(seek_target(Pos::Seconds(80.0), None), Ok(80.0));
        assert_eq!(seek_target(Pos::Seconds(-5.0), Some(200.0)), Ok(0.0));
    }

    #[test]
    fn a_percent_with_no_duration_reports_rather_than_guessing() {
        // seeking to "60% of nothing" would silently be 0:00, which
        // looks like a seek that worked.
        assert!(seek_target(Pos::Percent(60.0), None).is_err());
        assert!(seek_target(Pos::Percent(60.0), Some(0.0)).is_err());
    }

    // -------------------------------------------------------- volume

    #[test]
    fn an_absolute_level_is_taken_as_given() {
        assert_eq!(volume_target(Level::Absolute(50.0), 80.0), 50.0);
        assert_eq!(volume_target(Level::Absolute(0.0), 80.0), 0.0);
    }

    #[test]
    fn a_nudge_applies_to_the_current_volume() {
        assert_eq!(volume_target(Level::Delta(10.0), 40.0), 50.0);
        assert_eq!(volume_target(Level::Delta(-10.0), 40.0), 30.0);
    }

    #[test]
    fn the_ceiling_is_a_hundred() {
        // above 100 mpv applies digital gain and clips; a stale
        // per-application boost is what caused a real "disgusting
        // bass" bug here already.
        assert_eq!(volume_target(Level::Delta(50.0), 80.0), 100.0);
        assert_eq!(volume_target(Level::Delta(-50.0), 20.0), 0.0);
        assert_eq!(volume_target(Level::Absolute(1000.0), 20.0), 100.0);
    }

    // --------------------------------------------------------- ramps

    #[test]
    fn a_ramp_interpolates_and_then_finishes() {
        let ramp = Ramp::new(0.0, 100.0, secs(10), secs(4));
        assert_eq!(ramp.level_at(secs(10)), 0.0);
        assert_eq!(ramp.level_at(secs(12)), 50.0);
        assert_eq!(ramp.level_at(secs(14)), 100.0);
        assert!(!ramp.finished(secs(13)));
        assert!(ramp.finished(secs(14)));
        // clamped, not extrapolated past the end.
        assert_eq!(ramp.level_at(secs(99)), 100.0);
    }

    #[test]
    fn a_ramp_down_is_the_same_ramp_backwards() {
        let ramp = Ramp::new(80.0, 0.0, secs(0), secs(8));
        assert_eq!(ramp.level_at(secs(4)), 40.0);
        assert_eq!(ramp.level_at(secs(8)), 0.0);
    }

    #[test]
    fn a_zero_length_ramp_lands_immediately() {
        let ramp = Ramp::new(0.0, 100.0, secs(5), Duration::ZERO);
        assert_eq!(ramp.level_at(secs(5)), 100.0);
        assert!(ramp.finished(secs(5)));
    }

    // --------------------------------------------------------- sleep

    #[test]
    fn a_sleep_timer_waits_then_fades_then_expires() {
        let due = secs(100);
        let fade = secs(5);
        assert_eq!(sleep_step(due, fade, 80.0, secs(0)), SleepStep::Waiting);
        assert_eq!(sleep_step(due, fade, 80.0, secs(94)), SleepStep::Waiting);
        // 2.5s left of a 5s fade: half volume.
        assert_eq!(
            sleep_step(due, fade, 80.0, secs(97) + millis(500)),
            SleepStep::Fading(40.0)
        );
        assert_eq!(sleep_step(due, fade, 80.0, secs(100)), SleepStep::Expired);
        assert_eq!(sleep_step(due, fade, 80.0, secs(101)), SleepStep::Expired);
    }

    #[test]
    fn a_sleep_shorter_than_the_fade_starts_fading_at_once() {
        assert_eq!(
            sleep_step(secs(2), secs(5), 100.0, secs(0)),
            SleepStep::Fading(40.0)
        );
    }

    // ------------------------------------------------------- benchmark

    /// A library-shaped corpus: real artist/album/title spread, so the
    /// fuzzy scorer sees the same mix of near-misses a real one does.
    fn synthetic_corpus(n: usize) -> Corpus {
        const ARTISTS: [&str; 8] = [
            "Bladee",
            "Ecco2k",
            "Thaiboy Digital",
            "Burial",
            "Boards of Canada",
            "Godspeed You! Black Emperor",
            "Aphex Twin",
            "Mazzy Star",
        ];
        const ALBUMS: [&str; 6] = [
            "Icedancer",
            "Eversince",
            "Crest",
            "Untrue",
            "Selected Ambient Works",
            "The Fourth Wall",
        ];
        const TITLES: [&str; 8] = [
            "Blade Runner Blues",
            "Be Nice 2 Me",
            "Who Goes There",
            "Rewind",
            "Tears of Joy",
            "Obedient",
            "Hotel Breakfast",
            "Into Dust",
        ];
        let mut corpus = Corpus::new();
        corpus.add_local_tracks((0..n).map(|i| LocalTrack {
            path: PathBuf::from(format!("/music/{i:06}.flac")),
            title: format!("{} {i}", TITLES[i % TITLES.len()]),
            artist: Some(ARTISTS[(i / 7) % ARTISTS.len()].to_string()),
            album: Some(ALBUMS[(i / 13) % ALBUMS.len()].to_string()),
            duration_secs: None,
            kind: MediaKind::Music,
        }));
        corpus
    }

    /// Not an assertion: prints the per-keystroke wall time of the real
    /// completion path, corpus included, which is the number that
    /// decides whether the `:` prompt keeps up with typing. Absolute
    /// numbers are machine-dependent. Run with
    /// `cargo test --release --bin wim -- --ignored --nocapture bench_completion`.
    #[test]
    #[ignore = "timing benchmark, run manually"]
    fn bench_completion() {
        let saved: Vec<String> = Vec::new();
        for size in [10_000usize, 30_000] {
            let corpus = synthetic_corpus(size);
            let cand = CorpusCandidates {
                corpus: &corpus,
                saved: &saved,
                eq_presets: Vec::new(),
                vars: no_vars(),
                scripts: &[],
            };
            for line in ["play(blade r", "queue([bladee, obedient])", "volume(50%)"] {
                let iters = if size >= 30_000 { 10u32 } else { 25 };
                let mut worst = Duration::ZERO;
                let start = std::time::Instant::now();
                for _ in 0..iters {
                    let mut console = Console::empty();
                    for ch in line.chars() {
                        let key = KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE);
                        let one = std::time::Instant::now();
                        console.handle_key(key, &cand);
                        worst = worst.max(one.elapsed());
                    }
                }
                let keys = line.chars().count() as u32;
                let per = start.elapsed() / (iters * keys);
                println!(
                    "{size:>6} items  {:<28} {per:>10.3?}/key  worst {worst:>10.3?}",
                    line
                );
            }
        }
    }
}
