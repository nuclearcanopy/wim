//! The host side of shellsong: the few decisions `main.rs` has to make
//! for the language that are worth having apart from the event loop.
//! Everything here is pure, so each one is testable with no terminal,
//! no mpv and no network; `main.rs` keeps only the ipc writes and the
//! state mutations. The per-op application table lives there, next to
//! `dispatch_action`.

use std::sync::Arc;
use std::time::Duration;

use crate::search::Corpus;
use crate::shellsong::{Candidates, Level, Pos};
use crate::subsonic::Client;

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
}

/// Matches offered per keystroke. `Console` caps the list again at its
/// own `MAX_CANDIDATES`; stopping here keeps the clone count down on a
/// one-character token that matches half the library.
const MAX_CORPUS_CANDIDATES: usize = 50;

impl Candidates for CorpusCandidates<'_> {
    fn corpus(&self, token: &str) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for item in self.corpus.search(token) {
            let name = item.primary_name();
            // deliberately the bare name, not `results_list::label`'s
            // "title · artist": a candidate is inserted back into the
            // command line, so the decorated form would never be a
            // prefix extension of what was typed and the completer
            // would drop every one of them.
            if name.is_empty() || names.iter().any(|n| n == name) {
                continue;
            }
            names.push(name.to_string());
            if names.len() >= MAX_CORPUS_CANDIDATES {
                break;
            }
        }
        names
    }

    fn saved_queues(&self) -> Vec<String> {
        self.saved.to_vec()
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

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
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
        };
        assert_eq!(cand.corpus("same"), vec!["same title".to_string()]);
    }

    #[test]
    fn an_empty_token_offers_nothing() {
        let corpus = local_corpus(&["flatline"]);
        let cand = CorpusCandidates {
            corpus: &corpus,
            saved: &[],
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
