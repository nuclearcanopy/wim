//! The host side of shellsong: the few decisions `main.rs` has to make
//! for the language that are worth having apart from the event loop.
//! Everything here is pure, so each one is testable with no terminal,
//! no mpv and no network; `main.rs` keeps only the ipc writes and the
//! state mutations. The per-op application table lives there, next to
//! `dispatch_action`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::notify::{Notifications, Severity};
use crate::search::{Corpus, SearchItem};
use crate::shellsong::{Candidates, Level, Pos, Scope, Setting, SettingKind, Tier};
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

/// Reporting rather than best-effort, unlike the read above: a
/// read-only state dir used to leave `let morning = [...]` saying
/// "defined morning: 2 terms" with the variable gone next launch. Same
/// contract as `queues::save`, the other store a typed command writes.
pub fn save_vars(scope: &Scope) -> Result<(), String> {
    write_vars(&vars_path().ok_or_else(no_state_dir)?, scope)
}

/// Split from `load_vars` so a test can point it at a temp file; the
/// path resolution is the only part that is not testable.
fn read_vars(path: &Path) -> Scope {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| toml::from_str(&raw).ok())
        .unwrap_or_default()
}

fn write_vars(path: &Path, scope: &Scope) -> Result<(), String> {
    let body = toml::to_string_pretty(scope).map_err(|e| format!("encoding variables: {e}"))?;
    write_state(path, &body)
}

fn no_state_dir() -> String {
    "no state or data dir found for this platform".to_string()
}

/// The one write both shellsong state stores go through, naming the
/// path and the reason so the message is actionable.
fn write_state(path: &Path, body: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("creating {}: {e}", parent.display()))?;
    }
    std::fs::write(path, body).map_err(|e| format!("writing {}: {e}", path.display()))
}

// ------------------------------------------------- bound settings

/// Where the per-identity settings live. State rather than config:
/// these are written by `remember(...)`, not hand-edited, same class as
/// the saved queues.
const BINDINGS_FILE: &str = "bindings.toml";

/// One identity's remembered settings. Flat optionals rather than a
/// map, so toml reads as `eq = "night"` and a setting that is not bound
/// is simply absent.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Bound {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eq: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub speed: Option<f64>,
}

impl Bound {
    fn set(&mut self, setting: &Setting) {
        match setting {
            Setting::Eq(name) => self.eq = Some(name.clone()),
            Setting::Volume(v) => self.volume = Some(*v),
            Setting::Speed(s) => self.speed = Some(*s),
        }
    }

    fn clear(&mut self, kind: SettingKind) {
        match kind {
            SettingKind::Eq => self.eq = None,
            SettingKind::Volume => self.volume = None,
            SettingKind::Speed => self.speed = None,
        }
    }

    fn has(&self, kind: SettingKind) -> bool {
        match kind {
            SettingKind::Eq => self.eq.is_some(),
            SettingKind::Volume => self.volume.is_some(),
            SettingKind::Speed => self.speed.is_some(),
        }
    }

    fn is_empty(&self) -> bool {
        self.eq.is_none() && self.volume.is_none() && self.speed.is_none()
    }

    /// One line naming what is bound, for the listing and the toast.
    pub fn summary(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(name) = &self.eq {
            parts.push(format!("eq {name}"));
        }
        if let Some(v) = self.volume {
            parts.push(format!("volume {v}%"));
        }
        if let Some(s) = self.speed {
            parts.push(format!("speed {s}x"));
        }
        parts.join(", ")
    }
}

/// The three names a binding can hang off, read straight off the item.
///
/// **Names, not ids**, deliberately, and for the reason `eq::resolve`
/// gives for the same choice: an id points at a different thing the
/// moment the library is rescanned, a server is swapped or the same
/// track is played from a local copy instead of a stream, and a
/// remembered eq should survive all three. The cost is that two
/// genuinely different tracks sharing an artist and a title share a
/// binding, which is the rarer problem.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Identity {
    pub track: Option<String>,
    pub album: Option<String>,
    pub artist: Option<String>,
}

/// The identity's name at `tier`, which is the key a binding hangs
/// off. `None` when the track has nothing there (a local file with no
/// album tag), which is a refusal rather than an empty key.
pub fn name_at(id: &Identity, tier: Tier) -> Option<String> {
    id.at(tier).map(str::to_string)
}

impl Identity {
    fn at(&self, tier: Tier) -> Option<&str> {
        match tier {
            Tier::Track => self.track.as_deref(),
            Tier::Album => self.album.as_deref(),
            Tier::Artist => self.artist.as_deref(),
        }
    }
}

fn key(text: &str) -> Option<String> {
    let text = text.trim().to_lowercase();
    (!text.is_empty()).then_some(text)
}

/// The identity of whatever is playing. A track's key carries its
/// artist as well as its title, since a title alone collides across a
/// library ("intro" is every third record).
pub fn identity_of(item: &SearchItem) -> Identity {
    let (title, artist, album) = match item {
        SearchItem::LocalMusic {
            title,
            artist,
            album,
            ..
        } => (title.clone(), artist.clone(), album.clone()),
        SearchItem::LocalVideo { title, .. } => (title.clone(), None, None),
        SearchItem::Song {
            title,
            artist,
            album,
            ..
        } => (title.clone(), Some(artist.clone()), Some(album.clone())),
        // never reaches the queue: `resolve_tracks` expands all three
        // before anything is played.
        SearchItem::Artist { .. } | SearchItem::Album { .. } | SearchItem::Playlist { .. } => {
            (String::new(), None, None)
        }
        // a silence *does* reach the queue, unlike the three above, and
        // deliberately has no identity: `key("")` is `None`, so every
        // tier refuses and no per-identity eq or volume fires when one
        // starts. a silence must not change how the next track sounds.
        SearchItem::Silence { .. } => (String::new(), None, None),
    };
    let artist = artist.and_then(|a| key(&a));
    Identity {
        track: key(&title).map(|t| match &artist {
            Some(a) => format!("{a} - {t}"),
            None => t,
        }),
        album: album.and_then(|a| key(&a)),
        artist,
    }
}

/// Every per-identity setting, by tier then by identity. `BTreeMap`, so
/// the listing and the toml are both in a stable order.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Bindings {
    #[serde(default)]
    track: BTreeMap<String, Bound>,
    #[serde(default)]
    album: BTreeMap<String, Bound>,
    #[serde(default)]
    artist: BTreeMap<String, Bound>,
}

impl Bindings {
    fn tier(&self, tier: Tier) -> &BTreeMap<String, Bound> {
        match tier {
            Tier::Track => &self.track,
            Tier::Album => &self.album,
            Tier::Artist => &self.artist,
        }
    }

    fn tier_mut(&mut self, tier: Tier) -> &mut BTreeMap<String, Bound> {
        match tier {
            Tier::Track => &mut self.track,
            Tier::Album => &mut self.album,
            Tier::Artist => &mut self.artist,
        }
    }

    pub fn remember(&mut self, tier: Tier, name: &str, setting: &Setting) {
        self.tier_mut(tier)
            .entry(name.to_string())
            .or_default()
            .set(setting);
    }

    /// Drops one setting, or every setting, at `tier`. False when there
    /// was nothing bound there to drop, so the host can say so rather
    /// than reporting a no-op as a success (R9).
    pub fn forget(&mut self, tier: Tier, name: &str, setting: Option<SettingKind>) -> bool {
        let map = self.tier_mut(tier);
        let Some(bound) = map.get_mut(name) else {
            return false;
        };
        match setting {
            None => {
                map.remove(name);
                true
            }
            Some(kind) => {
                let had = bound.has(kind);
                bound.clear(kind);
                if bound.is_empty() {
                    map.remove(name);
                }
                had
            }
        }
    }

    /// What applies to `id`. Precedence runs track, album, artist
    /// **per setting**, not per identity: a track-level eq and an
    /// album-level volume both apply, because letting the narrower tier
    /// mask the wider one would make remembering an eq silently drop a
    /// remembered volume.
    pub fn resolve(&self, id: &Identity) -> Bound {
        let mut out = Bound::default();
        for tier in [Tier::Track, Tier::Album, Tier::Artist] {
            let Some(bound) = id.at(tier).and_then(|name| self.tier(tier).get(name)) else {
                continue;
            };
            out.eq = out.eq.or_else(|| bound.eq.clone());
            out.volume = out.volume.or(bound.volume);
            out.speed = out.speed.or(bound.speed);
        }
        out
    }

    pub fn is_empty(&self) -> bool {
        self.track.is_empty() && self.album.is_empty() && self.artist.is_empty()
    }

    /// What `view(bindings)` prints: one line per bound identity.
    pub fn lines(&self) -> Vec<String> {
        if self.is_empty() {
            return vec!["nothing is remembered; remember(track, eq(night)) binds one".to_string()];
        }
        let mut out = Vec::new();
        for tier in [Tier::Track, Tier::Album, Tier::Artist] {
            for (name, bound) in self.tier(tier) {
                out.push(format!("{}: {name} -> {}", tier.label(), bound.summary()));
            }
        }
        out
    }
}

/// What the per-identity settings are currently overriding, holding the
/// value that was in effect *before* each override so it can be put
/// back. `None` means that setting is not overridden.
///
/// Putting it back is the whole reason this exists: leaving a bound
/// track's volume applied to the next, unbound one is the bug that has
/// the user wondering why everything went quiet after one song.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Overridden {
    pub eq: Option<String>,
    pub volume: Option<f64>,
    pub speed: Option<f64>,
}

/// When `settle` considers two readings of a setting the same value,
/// and so has nothing to apply. A plain `PartialEq` was wrong for the
/// one setting that is a name: `remember(artist, eq(Night))` stored
/// "Night" while mpv's live curve reads back as "night", so the eq
/// never settled and every track got a redundant ipc write and a toast.
pub trait SameValue {
    fn same(&self, other: &Self) -> bool;
}

impl SameValue for f64 {
    fn same(&self, other: &Self) -> bool {
        self == other
    }
}

impl SameValue for String {
    /// `eq::same_name`'s rule, which is what the eq menu and
    /// `config.toml`'s `eq_preset` already compare by: trimmed and
    /// ascii-case-insensitive, so one curve however it was typed.
    fn same(&self, other: &Self) -> bool {
        self.trim().eq_ignore_ascii_case(other.trim())
    }
}

/// One setting's transition on a track change.
///
/// `active` is the value to restore and is updated in place, `wanted`
/// is what the new track's bindings ask for, `current` is what is in
/// effect right now. Returns the value to apply, or `None` for "leave
/// it alone". Pure, so the restore rule is tested with no mpv.
pub fn settle<T: Clone + SameValue>(
    active: &mut Option<T>,
    wanted: Option<T>,
    current: T,
) -> Option<T> {
    match wanted {
        Some(want) => {
            // the user's own value is captured the first time an
            // override takes over, not on every bound track in a row,
            // or a chain of them would restore to the previous binding
            // instead of to what the user had chosen.
            if active.is_none() {
                *active = Some(current.clone());
            }
            (!want.same(&current)).then_some(want)
        }
        None => active.take().filter(|previous| !previous.same(&current)),
    }
}

fn bindings_path() -> Option<PathBuf> {
    crate::shellsong::state_path(BINDINGS_FILE)
}

pub fn load_bindings() -> Bindings {
    bindings_path()
        .map(|p| read_bindings(&p))
        .unwrap_or_default()
}

/// Reports its failure for the same reason `save_vars` does: a
/// `remember(...)` that said so and then lost the binding is the same
/// bug twice.
pub fn save_bindings(bindings: &Bindings) -> Result<(), String> {
    write_bindings(&bindings_path().ok_or_else(no_state_dir)?, bindings)
}

fn read_bindings(path: &Path) -> Bindings {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| toml::from_str(&raw).ok())
        .unwrap_or_default()
}

fn write_bindings(path: &Path, bindings: &Bindings) -> Result<(), String> {
    let body = toml::to_string_pretty(bindings).map_err(|e| format!("encoding settings: {e}"))?;
    write_state(path, &body)
}

/// `config::save_or_warn`'s shape for the two shellsong state stores:
/// persist, and say so if you cannot. Every write site in the host has
/// a `Notifications` to hand, so neither of these has to be silent.
pub fn save_vars_or_warn(scope: &Scope, notifications: &mut Notifications) {
    if let Err(e) = save_vars(scope) {
        notifications.push(Severity::Error, format!("couldn't save variables: {e}"));
    }
}

pub fn save_bindings_or_warn(bindings: &Bindings, notifications: &mut Notifications) {
    if let Err(e) = save_bindings(bindings) {
        notifications.push(
            Severity::Error,
            format!("couldn't save remembered settings: {e}"),
        );
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
/// Comfortably past `core::REMOTE_SEARCH_DEBOUNCE` plus a round trip
/// (the test below holds the two to that), and far inside `shellsong`'s
/// own 8s await timeout, so a slow server costs `play(crest)` a beat
/// rather than an error.
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
///
/// Both spellings are held to the same range, which is the whole point:
/// `moveto(1h)` on a three-minute track used to report "moved to 3600s"
/// and skip the track, where `moveto(200%)` could not. Past the end is
/// *refused* rather than clamped, the habit everywhere else in the
/// language (a count above 256, a time over 24h): clamping would end
/// the track and say "moved to", which reads exactly like a seek that
/// worked, and `moveto(1h)` for `moveto(1m)` is a typo, not a request
/// to skip. An unknown duration still goes through, since a live stream
/// has no end to be past.
pub fn seek_target(pos: Pos, duration: Option<f64>) -> Result<f64, String> {
    match pos {
        Pos::Seconds(s) => {
            let s = s.max(0.0);
            match duration {
                Some(d) if d > 0.0 && s > d => Err(format!(
                    "{s:.0}s is past the end of this {d:.0}s track; moveto(100%) is the end"
                )),
                _ => Ok(s),
            }
        }
        Pos::Percent(p) => match duration {
            Some(d) if d > 0.0 => Ok((d * p / 100.0).clamp(0.0, d)),
            _ => Err(
                "nothing with a known duration is playing, so a percent has nothing to resolve against"
                    .to_string(),
            ),
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

    // ------------------------------------------------ bound settings

    fn song(title: &str, artist: &str, album: &str) -> SearchItem {
        SearchItem::LocalMusic {
            path: PathBuf::from(format!("/music/{title}.flac")),
            title: title.to_string(),
            artist: Some(artist.to_string()),
            album: Some(album.to_string()),
        }
    }

    fn bound(eq: Option<&str>, volume: Option<f64>, speed: Option<f64>) -> Bound {
        Bound {
            eq: eq.map(str::to_string),
            volume,
            speed,
        }
    }

    #[test]
    fn an_identity_is_the_names_lowercased_with_the_artist_on_the_track() {
        let id = identity_of(&song("Crest", "Bladee", "Icedancer"));
        // artist on the track key, because a bare title collides
        // across a library ("intro" is every third record).
        assert_eq!(id.track.as_deref(), Some("bladee - crest"));
        assert_eq!(id.album.as_deref(), Some("icedancer"));
        assert_eq!(id.artist.as_deref(), Some("bladee"));
        // a local file with no tags has only a title to hang off, and
        // `remember(album, ...)` on it has to refuse rather than bind
        // an empty key.
        let bare = SearchItem::LocalVideo {
            path: PathBuf::from("/v/x.mkv"),
            title: "x".to_string(),
        };
        let id = identity_of(&bare);
        assert_eq!(id.track.as_deref(), Some("x"));
        assert_eq!(name_at(&id, Tier::Album), None);
        assert_eq!(name_at(&id, Tier::Artist), None);
    }

    /// Precedence is per setting, not per identity: a track-level eq
    /// and an album-level volume both apply. Masking one with the other
    /// would make remembering an eq silently drop a remembered volume.
    #[test]
    fn precedence_runs_track_album_artist_one_setting_at_a_time() {
        let item = song("crest", "bladee", "icedancer");
        let id = identity_of(&item);
        let mut b = Bindings::default();
        b.remember(Tier::Artist, "bladee", &Setting::Eq("rock".into()));
        b.remember(Tier::Artist, "bladee", &Setting::Speed(0.9));
        b.remember(Tier::Album, "icedancer", &Setting::Eq("flat".into()));
        b.remember(Tier::Album, "icedancer", &Setting::Volume(55.0));
        b.remember(Tier::Track, "bladee - crest", &Setting::Eq("night".into()));
        // track wins the eq, the album still supplies the volume, the
        // artist still supplies the speed.
        assert_eq!(b.resolve(&id), bound(Some("night"), Some(55.0), Some(0.9)));
        // drop the narrowest and the next one down takes over.
        assert!(b.forget(Tier::Track, "bladee - crest", Some(SettingKind::Eq)));
        assert_eq!(b.resolve(&id), bound(Some("flat"), Some(55.0), Some(0.9)));
        // an unrelated track gets only what its artist and album bind.
        let other = identity_of(&song("hex", "ecco2k", "e"));
        assert_eq!(b.resolve(&other), Bound::default());
    }

    #[test]
    fn forget_reports_whether_there_was_anything_to_drop() {
        let mut b = Bindings::default();
        b.remember(Tier::Track, "a", &Setting::Eq("night".into()));
        b.remember(Tier::Track, "a", &Setting::Volume(40.0));
        assert!(!b.forget(Tier::Track, "nothing here", None));
        assert!(!b.forget(Tier::Track, "a", Some(SettingKind::Speed)));
        assert!(b.forget(Tier::Track, "a", Some(SettingKind::Eq)));
        assert!(b.forget(Tier::Track, "a", None));
        assert!(b.is_empty(), "the last setting going leaves no stub");
    }

    /// The guard for the whole stage. A setting the new track does not
    /// bind goes back to what the *user* chose, not to the last bound
    /// value; leaving it applied is what has everything quiet after one
    /// quiet song.
    #[test]
    fn a_setting_is_restored_when_the_next_track_binds_nothing() {
        let mut active: Option<f64> = None;
        // user is at 80; a bound track wants 40.
        assert_eq!(settle(&mut active, Some(40.0), 80.0), Some(40.0));
        assert_eq!(active, Some(80.0), "the user's own value is held");
        // an unbound track follows: back to 80, and nothing is held.
        assert_eq!(settle(&mut active, None, 40.0), Some(80.0));
        assert_eq!(active, None);
        // and with nothing overridden, an unbound track changes nothing.
        assert_eq!(settle(&mut active, None, 80.0), None);
        assert_eq!(active, None);
    }

    /// A run of bound tracks must not ratchet the restore point along,
    /// or the last one's value becomes "what the user chose".
    #[test]
    fn a_chain_of_bound_tracks_still_restores_to_the_users_own_value() {
        let mut active: Option<f64> = None;
        assert_eq!(settle(&mut active, Some(40.0), 80.0), Some(40.0));
        assert_eq!(settle(&mut active, Some(30.0), 40.0), Some(30.0));
        assert_eq!(settle(&mut active, Some(20.0), 30.0), Some(20.0));
        assert_eq!(active, Some(80.0), "still the value from before the run");
        assert_eq!(settle(&mut active, None, 20.0), Some(80.0));
    }

    #[test]
    fn settling_on_a_value_that_is_already_in_effect_applies_nothing() {
        // no op means no ipc write and no scrollback line for a
        // setting that did not move.
        let mut active: Option<String> = None;
        assert_eq!(
            settle(&mut active, Some("night".to_string()), "night".to_string()),
            None
        );
        // held anyway, so the restore still knows where to go back to.
        assert_eq!(active.as_deref(), Some("night"));
        let mut active: Option<f64> = Some(80.0);
        assert_eq!(settle(&mut active, None, 80.0), None);
        assert!(active.is_none());
    }

    /// The eq is the one setting whose value is a name, and names are
    /// compared `eq::same_name`'s way. Comparing them with `PartialEq`
    /// meant `remember(artist, eq(Night))` never settled: the stored
    /// "Night" never equalled the live "night", so every single track
    /// got a redundant ipc write and another toast, forever.
    #[test]
    fn an_eq_name_settles_whatever_case_it_was_remembered_in() {
        let mut active: Option<String> = None;
        assert_eq!(
            settle(&mut active, Some("Night".to_string()), "night".to_string()),
            None,
            "one curve, so there is nothing to apply"
        );
        assert_eq!(active.as_deref(), Some("night"), "held for the restore");
        // the restore side is the same comparison from the other end.
        let mut active = Some(" Night ".to_string());
        assert_eq!(settle(&mut active, None, "night".to_string()), None);
        assert!(active.is_none());
        // a genuinely different curve still applies.
        let mut active: Option<String> = None;
        assert_eq!(
            settle(&mut active, Some("rock".to_string()), "night".to_string()),
            Some("rock".to_string())
        );
    }

    #[test]
    fn bindings_round_trip_through_the_state_file() {
        let path = temp_vars_path("bindings");
        let mut b = Bindings::default();
        b.remember(Tier::Track, "bladee - crest", &Setting::Eq("night".into()));
        b.remember(Tier::Album, "icedancer", &Setting::Volume(55.0));
        b.remember(Tier::Artist, "bladee", &Setting::Speed(0.9));
        write_bindings(&path, &b).expect("temp write");
        assert_eq!(read_bindings(&path), b);
        // and the listing names every tier, so `view(bindings)` shows
        // the lot rather than only the narrowest.
        let lines = b.lines();
        assert_eq!(lines.len(), 3);
        assert!(
            lines[0].starts_with("track: bladee - crest -> eq night"),
            "{lines:?}"
        );
        assert!(
            lines[1].starts_with("album: icedancer -> volume 55%"),
            "{lines:?}"
        );
        assert!(
            lines[2].starts_with("artist: bladee -> speed 0.9x"),
            "{lines:?}"
        );
        assert_eq!(Bindings::default().lines().len(), 1);
        assert!(read_bindings(Path::new("/nonexistent/wim/bindings.toml")).is_empty());
        let _ = std::fs::remove_file(&path);
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
        write_vars(&path, &scope).expect("temp write");
        let back = read_vars(&path);
        assert_eq!(back, scope);
        assert_eq!(back.names(), vec!["bladee_classics", "quiet"]);
        let _ = std::fs::remove_file(&path);
    }

    /// The failure the maintainer hits with an unwritable
    /// `$XDG_STATE_HOME`: `: let morning = [mazzy star, burial]` said
    /// "defined morning: 2 terms" and the variable was gone next
    /// launch, where `queues::save` has always reported the same
    /// failure. Both stores now answer the same way.
    #[test]
    fn a_state_write_that_cannot_land_is_reported_not_swallowed() {
        // a plain file standing where the parent directory would have
        // to be: unwritable for any user, root included, where a
        // chmod-ed directory would not be.
        let blocker = temp_vars_path("unwritable");
        std::fs::write(&blocker, "not a directory").expect("temp write");
        let path = blocker.join("wim").join(VARS_FILE);
        let parent = path.parent().expect("has a parent").display().to_string();
        let why = write_vars(&path, &Scope::default()).expect_err("must not report success");
        assert!(why.contains(&parent), "{why}");
        let why = write_bindings(&path, &Bindings::default()).expect_err("must not report success");
        assert!(why.contains(&parent), "{why}");
        let _ = std::fs::remove_file(&blocker);
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

    /// Coupled to the constant rather than to a copy of its value: the
    /// hardcoded `300` here pinned neither number, so raising the
    /// debounce past half the window would have left `play(crest)`
    /// settling before the request it is waiting for had even gone out.
    #[test]
    fn the_window_clears_the_debounce_plus_a_round_trip() {
        let debounce = crate::core::REMOTE_SEARCH_DEBOUNCE;
        assert!(
            SETTLE_WINDOW >= debounce * 2,
            "a {debounce:?} debounce needs more than a {SETTLE_WINDOW:?} window"
        );
        // and the behaviour the margin exists for: a query whose
        // request has only just left is still unsettled.
        let mut settle = SearchSettle::default();
        settle.notified(true, secs(10));
        assert!(!settle.settled(secs(10) + debounce));
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

    /// `moveto(1h)` on a three-minute track reported "moved to 3600s"
    /// and skipped it: the seconds branch clamped only the bottom while
    /// the percent branch on the next line clamped both ends. Refused
    /// rather than clamped, like every other out-of-range value here.
    #[test]
    fn a_time_past_the_end_is_refused_the_way_a_percent_over_a_hundred_is() {
        let three_minutes = Some(185.0);
        let why = seek_target(Pos::Seconds(3600.0), three_minutes).expect_err("must not seek");
        assert!(why.contains("past the end"), "{why}");
        assert!(why.contains("185s"), "{why}");
        // the end itself is reachable, from either spelling.
        assert_eq!(seek_target(Pos::Seconds(185.0), three_minutes), Ok(185.0));
        assert_eq!(seek_target(Pos::Percent(100.0), three_minutes), Ok(185.0));
        // nothing with a known end cannot be past it: a live stream
        // reports no duration, and seeking into it is still legal.
        assert_eq!(seek_target(Pos::Seconds(3600.0), None), Ok(3600.0));
        assert_eq!(seek_target(Pos::Seconds(3600.0), Some(0.0)), Ok(3600.0));
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
