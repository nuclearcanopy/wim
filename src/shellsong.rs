//! shellsong: a command language for the player. everything is a call
//! (`pause()`, `queue([hex, 4 star crest])`), lowered here into `Op`s
//! the host applies through the same paths its keybinds already use, so
//! behaviour can never drift from the keyboard's. search is
//! asynchronous, so `Program` is a resumable state machine rather than
//! a loop. the full language reference is docs/shellsong.md.

// nothing calls into this module yet: opening the console on ':' and
// draining `Program::step` in the input loop is the host's wiring step,
// which lands separately. this allow goes away with it.
#![allow(dead_code)]

use std::fmt;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use crossterm::event::KeyCode;

use crate::app::LoopMode;
use crate::eq;
use crate::search::SearchItem;
use crate::ui::textfit;

/// The key that opens the console. Also listed in
/// `keymap::reserved_key_purpose` so no action can be bound onto it
/// and then silently shadowed by this.
pub const OPEN_KEY: KeyCode = KeyCode::Char(':');

/// How long a `Resolve` will wait for the search it follows before
/// giving up. Comfortably past the host's 300ms remote-search debounce
/// plus a slow round trip; a wedged search subsystem reports instead of
/// hanging the program forever.
const AWAIT_TIMEOUT: Duration = Duration::from_secs(8);

/// Upper bound on `repeat(n, ...)`; the expansion is static, so an
/// absurd `n` would otherwise allocate an absurd program.
const MAX_REPEAT: u32 = 256;

/// Default ramp for `fade(in|out)` when no duration is given.
const DEFAULT_FADE_SECS: f64 = 5.0;

// ---------------------------------------------------------------- kinds

/// Result kind, for the preference order a searching command resolves
/// a term through (see `Command::prefs` and docs/shellsong.md R7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Song,
    Album,
    Artist,
    Playlist,
    Video,
}

impl Kind {
    /// Local music and remote songs are one kind here, same as
    /// `SearchItem::category` groups them: for the language they are
    /// interchangeable ("a song").
    pub fn of(item: &SearchItem) -> Kind {
        match item {
            SearchItem::LocalMusic { .. } | SearchItem::Song { .. } => Kind::Song,
            SearchItem::Album { .. } => Kind::Album,
            SearchItem::Artist { .. } => Kind::Artist,
            SearchItem::Playlist { .. } => Kind::Playlist,
            SearchItem::LocalVideo { .. } => Kind::Video,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Kind::Song => "song",
            Kind::Album => "album",
            Kind::Artist => "artist",
            Kind::Playlist => "playlist",
            Kind::Video => "video",
        }
    }
}

/// `kind=` ladder: every rung widens the one before it, so a suffix or
/// keyword argument can only ever *add* acceptable kinds. One line per
/// rung is the whole definition.
const KIND_LADDER: &[(&str, &[Kind])] = &[
    ("song", &[Kind::Song]),
    ("album", &[Kind::Song, Kind::Album]),
    ("artist", &[Kind::Song, Kind::Album, Kind::Artist]),
    (
        "any",
        &[
            Kind::Song,
            Kind::Album,
            Kind::Artist,
            Kind::Playlist,
            Kind::Video,
        ],
    ),
];

/// Name suffixes, pure sugar for a `kind=` argument and nothing else
/// (docs/shellsong.md R6): `queue_aa([x])` *is* `queue([x], kind=album)`,
/// which `dry()` reports. One line per suffix.
const KIND_SUFFIXES: &[(&str, &str)] = &[("_aa", "album"), ("_ar", "artist"), ("_any", "any")];

fn ladder(name: &str) -> Option<&'static [Kind]> {
    KIND_LADDER
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, ks)| *ks)
}

// ------------------------------------------------------------- literals

/// A position in a track. `Percent` deliberately stays a percent: the
/// track duration is runtime state the parser has no business knowing,
/// so the host resolves it when applying the op.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Pos {
    Percent(f64),
    Seconds(f64),
}

/// A volume request: an absolute level or a nudge, both in percent.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Level {
    Absolute(f64),
    Delta(f64),
}

/// What `show(target)` asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShowTarget {
    Queue,
    Now,
    History,
    Index,
    Playlists,
}

const SHOW_TARGETS: &[(&str, ShowTarget)] = &[
    ("queue", ShowTarget::Queue),
    ("now", ShowTarget::Now),
    ("history", ShowTarget::History),
    ("index", ShowTarget::Index),
    ("playlists", ShowTarget::Playlists),
];

// ------------------------------------------------------------------- op

/// One unit of work for the host. Every variant maps to something the
/// host already does for a keypress; see docs/shellsong.md for the
/// table of which call each one goes through.
#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    TogglePause,
    SkipNext,
    SkipBack,
    SeekAbsolute(Pos),
    SeekRelative(f64),
    SetVolume(Level),
    Mute(bool),
    CycleLoop,
    SetLoop(LoopMode),
    ToggleShuffle,
    SetShuffle(bool),
    SetSpeed(f64),
    SpeedUp,
    SpeedDown,
    OpenEqMenu,
    /// index into `eq::PRESETS`.
    SetEqPreset(usize),
    ToggleCrossfade,
    /// replace the query and re-filter, exactly as typing it would.
    Search(String),
    MoveSelection(i32),
    SelectTop,
    SelectBottom,
    /// whatever enter would do on the current selection.
    Go,
    /// act on `Env::results[index]`, resolved by the executor.
    PlayIndex(usize),
    EnqueueIndex {
        index: usize,
        front: bool,
    },
    StarIndex {
        index: usize,
        on: bool,
    },
    DownloadIndex(usize),
    /// zero-argument forms, acting on whatever is highlighted.
    StarSelection(bool),
    DownloadSelection,
    ClearQueue,
    /// 1-based queue positions, descending so the host can remove in
    /// order without reindexing.
    RemoveFromQueue(Vec<usize>),
    GotoQueue(usize),
    ShuffleQueue,
    SaveQueue(String),
    LoadQueue(String),
    Show(ShowTarget),
    OpenLyrics,
    OpenStarred,
    OpenSettings,
    OpenKeybinds,
    Rescan,
    /// seconds until fade-out + pause; `None` cancels.
    Sleep(Option<f64>),
    Fade {
        into: bool,
        secs: f64,
    },
    Quit,
}

impl Op {
    /// One line naming what this op did, for the scrollback. Nothing in
    /// shellsong is silent (docs/shellsong.md R9); the host pushes this
    /// after applying an op, or a richer message when it knows one
    /// (a track title, a queue count).
    pub fn describe(&self) -> String {
        match self {
            Op::TogglePause => "pause toggled".into(),
            Op::SkipNext => "skipped".into(),
            Op::SkipBack => "back".into(),
            Op::SeekAbsolute(Pos::Percent(p)) => format!("moved to {p}%"),
            Op::SeekAbsolute(Pos::Seconds(s)) => format!("moved to {s}s"),
            Op::SeekRelative(s) => format!("seek {s:+}s"),
            Op::SetVolume(Level::Absolute(v)) => format!("volume: {v}%"),
            Op::SetVolume(Level::Delta(d)) => format!("volume {d:+}%"),
            Op::Mute(true) => "muted".into(),
            Op::Mute(false) => "unmuted".into(),
            Op::CycleLoop => "loop cycled".into(),
            Op::SetLoop(m) => format!("loop: {}", m.label()),
            Op::ToggleShuffle => "shuffle toggled".into(),
            Op::SetShuffle(on) => format!("shuffle: {}", if *on { "on" } else { "off" }),
            Op::SetSpeed(s) => format!("speed: {s}x"),
            Op::SpeedUp => "speed up".into(),
            Op::SpeedDown => "speed down".into(),
            Op::OpenEqMenu => "eq menu".into(),
            Op::SetEqPreset(i) => {
                format!("eq: {}", eq::PRESETS.get(*i).map(|p| p.name).unwrap_or("?"))
            }
            Op::ToggleCrossfade => "crossfade toggled".into(),
            Op::Search(q) => format!("search: {q}"),
            Op::MoveSelection(d) if *d < 0 => "up".into(),
            Op::MoveSelection(_) => "down".into(),
            Op::SelectTop => "top".into(),
            Op::SelectBottom => "bottom".into(),
            Op::Go => "opened selection".into(),
            Op::PlayIndex(_) => "playing".into(),
            Op::EnqueueIndex { front: true, .. } => "queued next".into(),
            Op::EnqueueIndex { front: false, .. } => "queued".into(),
            Op::StarIndex { on: true, .. } | Op::StarSelection(true) => "starred".into(),
            Op::StarIndex { on: false, .. } | Op::StarSelection(false) => "unstarred".into(),
            Op::DownloadIndex(_) | Op::DownloadSelection => "downloading".into(),
            Op::ClearQueue => "queue cleared".into(),
            Op::RemoveFromQueue(v) => format!("removed {} from the queue", v.len()),
            Op::GotoQueue(n) => format!("jumped to queue {n}"),
            Op::ShuffleQueue => "queue shuffled".into(),
            Op::SaveQueue(n) => format!("saved queue as '{n}'"),
            Op::LoadQueue(n) => format!("loaded queue '{n}'"),
            Op::Show(t) => format!("show {}", show_name(*t)),
            Op::OpenLyrics => "lyrics".into(),
            Op::OpenStarred => "starred songs".into(),
            Op::OpenSettings => "settings".into(),
            Op::OpenKeybinds => "keybinds".into(),
            Op::Rescan => "rescanning the library".into(),
            Op::Sleep(Some(s)) => format!("sleep in {s}s"),
            Op::Sleep(None) => "sleep cancelled".into(),
            Op::Fade { into: true, secs } => format!("fading in over {secs}s"),
            Op::Fade { into: false, secs } => format!("fading out over {secs}s"),
            Op::Quit => "bye".into(),
        }
    }
}

fn show_name(target: ShowTarget) -> &'static str {
    SHOW_TARGETS
        .iter()
        .find(|(_, t)| *t == target)
        .map(|(n, _)| *n)
        .unwrap_or("?")
}

// ------------------------------------------------------------ signatures

/// Parameter type. This is what resolves "is `front` a keyword or a
/// search term": each command declares its parameter list by type and
/// the parser validates against it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ty {
    /// one or more search terms; a single value is accepted anywhere a
    /// list is (docs/shellsong.md R5).
    Terms,
    /// free text (a query, a playlist name, a command name).
    Name,
    /// unsigned time literal: absolute position.
    Pos,
    /// signed time literal: a relative offset.
    Offset,
    /// percent, absolute (`50%`) or signed (`+10%`).
    Level,
    /// a positive integer.
    Count,
    /// a 1-based queue position.
    Index,
    /// one or more 1-based queue positions.
    Indices,
    /// a number (multiplier) or `up` / `down` / `reset`.
    Speed,
    /// a time literal, or `off` to cancel.
    Duration,
    /// one of an enumerated keyword set.
    Words(Words),
    /// a nested call, for the higher-order commands.
    Call,
}

/// The enumerated keyword sets. `options` is what both the parser and
/// tab completion read, so a keyword that completes always parses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Words {
    Position,
    LoopMode,
    Switch,
    EqPreset,
    Show,
    FadeDir,
}

impl Words {
    pub fn options(self) -> Vec<&'static str> {
        match self {
            Words::Position => vec!["front", "back"],
            Words::LoopMode => vec!["one", "all", "off"],
            Words::Switch => vec!["on", "off"],
            Words::EqPreset => eq::PRESETS.iter().map(|p| p.name).collect(),
            Words::Show => SHOW_TARGETS.iter().map(|(n, _)| *n).collect(),
            Words::FadeDir => vec!["in", "out"],
        }
    }
}

impl Ty {
    /// Keywords this type accepts, for completion and for error
    /// messages; empty for the free-form types.
    pub fn options(self) -> Vec<&'static str> {
        match self {
            Ty::Words(w) => w.options(),
            Ty::Speed => vec!["up", "down", "reset"],
            Ty::Duration => vec!["off"],
            _ => Vec::new(),
        }
    }

    fn describe(self) -> String {
        match self {
            Ty::Terms => "a term or [list of terms]".into(),
            Ty::Name => "a name".into(),
            Ty::Pos => "an unsigned position like 30s, 1m20s or 50%".into(),
            Ty::Offset => "a signed offset like +30s or -10s".into(),
            Ty::Level => "a percent like 50% or +10%".into(),
            Ty::Count => "a positive whole number".into(),
            Ty::Index => "a queue position (1 or higher)".into(),
            Ty::Indices => "one or more queue positions".into(),
            Ty::Speed => "a multiplier like 1.5, or up / down / reset".into(),
            Ty::Duration => "a duration like 30m, or off".into(),
            Ty::Words(w) => w.options().join(" or "),
            Ty::Call => "a call like skip()".into(),
        }
    }
}

#[derive(Debug)]
pub struct Param {
    pub name: &'static str,
    pub ty: Ty,
    pub required: bool,
}

const fn req(name: &'static str, ty: Ty) -> Param {
    Param {
        name,
        ty,
        required: true,
    }
}

const fn opt(name: &'static str, ty: Ty) -> Param {
    Param {
        name,
        ty,
        required: false,
    }
}

/// One command. The table below is the single source of truth: the
/// parser, `help()` and tab completion all read it, so they cannot
/// disagree about what exists.
#[derive(Debug)]
pub struct Command {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub params: &'static [Param],
    /// kind preference order for a `Terms` parameter, best first; empty
    /// for commands that do not search.
    pub prefs: &'static [Kind],
    pub help: &'static str,
}

impl Command {
    /// `name(param, param?)`, generated from `params` so a signature
    /// can never drift from what the parser enforces.
    pub fn signature(&self) -> String {
        let args = self
            .params
            .iter()
            .map(|p| {
                if p.required {
                    p.name.to_string()
                } else {
                    format!("{}?", p.name)
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!("{}({args})", self.name)
    }
}

const NONE: &[Param] = &[];
const SONG: &[Kind] = &[Kind::Song];
const SONG_ALBUM: &[Kind] = &[Kind::Song, Kind::Album];

const fn cmd(
    name: &'static str,
    aliases: &'static [&'static str],
    params: &'static [Param],
    prefs: &'static [Kind],
    help: &'static str,
) -> Command {
    Command {
        name,
        aliases,
        params,
        prefs,
        help,
    }
}

pub const COMMANDS: &[Command] = &[
    // transport
    cmd(
        "pause",
        &["toggle", "play_pause"],
        NONE,
        &[],
        "play / pause",
    ),
    cmd("skip", &["next", "forward", "fwd"], NONE, &[], "next track"),
    cmd("back", &["prev", "previous"], NONE, &[], "previous track"),
    cmd(
        "moveto",
        &["move"],
        &[req("pos", Ty::Pos)],
        &[],
        "seek to an absolute position",
    ),
    cmd(
        "seek",
        &[],
        &[req("offset", Ty::Offset)],
        &[],
        "seek by a signed offset",
    ),
    cmd(
        "volume",
        &["vol"],
        &[req("level", Ty::Level)],
        &[],
        "set or nudge the volume (clamped to 100%)",
    ),
    cmd("mute", &[], NONE, &[], "mute"),
    cmd("unmute", &[], NONE, &[], "unmute"),
    // modes
    cmd(
        "loop",
        &["repeat_mode"],
        &[opt("mode", Ty::Words(Words::LoopMode))],
        &[],
        "cycle loop mode, or set it",
    ),
    cmd(
        "shuffle",
        &["random", "randomize", "shuf"],
        &[opt("state", Ty::Words(Words::Switch))],
        &[],
        "toggle shuffle mode, or set it",
    ),
    cmd(
        "shuffle_queue",
        &[],
        NONE,
        &[],
        "reorder the queue now, without touching shuffle mode",
    ),
    cmd(
        "speed",
        &["tempo"],
        &[req("speed", Ty::Speed)],
        &[],
        "set playback speed",
    ),
    cmd(
        "eq",
        &["equalizer"],
        &[opt("preset", Ty::Words(Words::EqPreset))],
        &[],
        "open the eq menu, or apply a preset",
    ),
    cmd("crossfade", &["xfade"], NONE, &[], "toggle crossfade"),
    // search and playback
    cmd(
        "search",
        &["find", "grep"],
        &[req("query", Ty::Name)],
        &[],
        "run a search and leave the results up",
    ),
    cmd(
        "play",
        &[],
        &[req("terms", Ty::Terms)],
        SONG_ALBUM,
        "play the first term, queue the rest",
    ),
    cmd(
        "queue",
        &["enqueue"],
        &[req("terms", Ty::Terms)],
        SONG,
        "append each term to the queue",
    ),
    cmd(
        "add_to_queue",
        &[],
        &[
            opt("position", Ty::Words(Words::Position)),
            req("terms", Ty::Terms),
        ],
        SONG,
        "queue at the front or the back (default back)",
    ),
    cmd(
        "clear_queue",
        &["clear", "flush"],
        NONE,
        &[],
        "empty the queue",
    ),
    cmd(
        "remove",
        &[],
        &[req("indices", Ty::Indices)],
        &[],
        "drop queue entries by position",
    ),
    cmd(
        "goto",
        &[],
        &[req("index", Ty::Index)],
        &[],
        "jump to a queue position",
    ),
    cmd(
        "save_queue",
        &[],
        &[req("name", Ty::Name)],
        &[],
        "save the queue as a local playlist",
    ),
    cmd(
        "load_queue",
        &[],
        &[req("name", Ty::Name)],
        &[],
        "replace the queue with a saved playlist",
    ),
    cmd(
        "star",
        &["favorite", "fav", "like"],
        &[opt("terms", Ty::Terms)],
        SONG_ALBUM,
        "star the selection, or whatever each term finds",
    ),
    cmd(
        "unstar",
        &["unfavorite", "unfav", "unlike"],
        &[opt("terms", Ty::Terms)],
        SONG_ALBUM,
        "unstar the selection, or whatever each term finds",
    ),
    cmd(
        "download",
        &["dl", "offline", "cache"],
        &[opt("terms", Ty::Terms)],
        SONG_ALBUM,
        "download for offline playback",
    ),
    // selection
    cmd(
        "go",
        &["enter", "open", "select"],
        NONE,
        &[],
        "act on the current selection, same as enter",
    ),
    cmd("up", &[], NONE, &[], "move the selection up"),
    cmd("down", &[], NONE, &[], "move the selection down"),
    cmd("top", &[], NONE, &[], "select the first result"),
    cmd("bottom", &[], NONE, &[], "select the last result"),
    // introspection and overlays
    cmd(
        "show",
        &["ls"],
        &[req("target", Ty::Words(Words::Show))],
        &[],
        "show queue / now / history / index / playlists",
    ),
    cmd("lyrics", &[], NONE, &[], "open lyrics for what is playing"),
    cmd("starred", &[], NONE, &[], "open the starred songs view"),
    cmd("settings", &[], NONE, &[], "open the settings menu"),
    cmd("keybinds", &[], NONE, &[], "open the keybind menu"),
    cmd("rescan", &[], NONE, &[], "re-walk the local library"),
    cmd(
        "sleep",
        &[],
        &[req("after", Ty::Duration)],
        &[],
        "fade out and pause after a delay; sleep(off) cancels",
    ),
    cmd(
        "fade",
        &[],
        &[
            req("direction", Ty::Words(Words::FadeDir)),
            opt("over", Ty::Duration),
        ],
        &[],
        "fade the volume in or out",
    ),
    cmd(
        "help",
        &[],
        &[opt("name", Ty::Name)],
        &[],
        "list every command, or one command's signature",
    ),
    cmd("quit", &["exit"], NONE, &[], "quit wim"),
    // higher-order
    cmd(
        "repeat",
        &[],
        &[req("count", Ty::Count), req("call", Ty::Call)],
        &[],
        "run a call n times",
    ),
    cmd(
        "dry",
        &["explain"],
        &[req("call", Ty::Call)],
        &[],
        "report what a call would do, running nothing",
    ),
];

/// Resolves a written name to a command plus the `kind=` value its
/// suffix desugars to, if any.
fn lookup(name: &str) -> Option<(&'static Command, Option<&'static str>)> {
    if let Some(c) = COMMANDS
        .iter()
        .find(|c| c.name == name || c.aliases.contains(&name))
    {
        return Some((c, None));
    }
    for (suffix, kind) in KIND_SUFFIXES {
        let Some(stem) = name.strip_suffix(suffix) else {
            continue;
        };
        if let Some(c) = COMMANDS
            .iter()
            .find(|c| c.name == stem || c.aliases.contains(&stem))
        {
            if !c.prefs.is_empty() {
                return Some((c, Some(kind)));
            }
        }
    }
    None
}

/// Every written name the parser accepts, canonical and alias alike;
/// what tab completion offers and what the did-you-mean suggestion
/// searches.
pub fn every_name() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = Vec::new();
    for c in COMMANDS {
        names.push(c.name);
        names.extend_from_slice(c.aliases);
    }
    names.sort_unstable();
    names
}

/// Closest known name to `name`, for "did you mean"; `strsim` is
/// already how search.rs recovers typos, so the same metric is used.
fn did_you_mean(name: &str) -> Option<&'static str> {
    let budget = 2.max(name.chars().count() / 3);
    every_name()
        .into_iter()
        .map(|c| (strsim::damerau_levenshtein(name, c), c))
        .filter(|(d, _)| *d <= budget)
        .min_by_key(|(d, c)| (*d, c.len()))
        .map(|(_, c)| c)
}

/// `help()` output, generated from `COMMANDS`; canonical names only,
/// aliases in a trailing column (docs/shellsong.md R3).
pub fn help_lines() -> Vec<String> {
    let width = COMMANDS
        .iter()
        .map(|c| textfit::width(&c.signature()))
        .max()
        .unwrap_or(0);
    COMMANDS.iter().map(|c| help_line(c, width)).collect()
}

fn help_line(c: &Command, width: usize) -> String {
    let sig = c.signature();
    let pad = width.saturating_sub(textfit::width(&sig));
    let aliases = if c.aliases.is_empty() {
        String::new()
    } else {
        format!("  [{}]", c.aliases.join(", "))
    };
    format!("{sig}{:pad$}  {}{aliases}", "", c.help)
}

// ----------------------------------------------------------------- env

/// What the host tells a running program about the world, rebuilt per
/// step from live state. This indirection is the whole reason the
/// executor is testable with no terminal, no mpv and no network.
#[derive(Debug, Clone, Default)]
pub struct Env {
    /// monotonic clock reading supplied by the host (e.g.
    /// `start.elapsed()`); only read to time out a stuck await.
    pub now: Duration,
    /// the query the results list is currently showing.
    pub query: String,
    /// kinds of the current results, in display order; index into this
    /// is what the index-carrying ops refer to.
    pub results: Vec<Kind>,
    /// the host's answer to "are the results for `query` as complete as
    /// they are going to get": no remote search still outstanding.
    pub search_settled: bool,
    pub queue_len: usize,
    /// a browse view (album/artist/playlist/starred) is open.
    pub browsing: bool,
}

// --------------------------------------------------------------- errors

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub message: String,
    /// 1-based column of the offending token in the submitted input.
    pub column: usize,
    pub token: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.token.is_empty() {
            write!(f, "col {}: {}", self.column, self.message)
        } else {
            write!(
                f,
                "col {}: {} (at '{}')",
                self.column, self.message, self.token
            )
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeError {
    /// the search for `0` never settled; the search subsystem is stuck,
    /// which is a different problem from "nothing matched".
    TimedOut(String),
    NothingSelected,
    OutOfRange {
        what: &'static str,
        index: usize,
        len: usize,
    },
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RuntimeError::TimedOut(q) => write!(f, "timed out waiting for results for '{q}'"),
            RuntimeError::NothingSelected => write!(f, "nothing is selected"),
            RuntimeError::OutOfRange { what, index, len } => {
                write!(f, "{what} {index} is out of range (1..{len})")
            }
        }
    }
}

// ------------------------------------------------------------- lowering

/// What a resolved term gets done to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TermAction {
    Play,
    Enqueue { front: bool },
    Star(bool),
    Download,
}

impl TermAction {
    fn op(self, index: usize) -> Op {
        match self {
            TermAction::Play => Op::PlayIndex(index),
            TermAction::Enqueue { front } => Op::EnqueueIndex { index, front },
            TermAction::Star(on) => Op::StarIndex { index, on },
            TermAction::Download => Op::DownloadIndex(index),
        }
    }

    fn past(self) -> &'static str {
        match self {
            TermAction::Play => "played",
            TermAction::Enqueue { .. } => "queued",
            TermAction::Star(true) => "starred",
            TermAction::Star(false) => "unstarred",
            TermAction::Download => "downloading",
        }
    }
}

/// A lowered instruction. Mostly a plain `Op` to hand over; the other
/// variants are the ones that have to look at `Env` first, which is
/// where the asynchrony lives.
#[derive(Debug, Clone, PartialEq)]
enum Node {
    Emit(Op),
    Note(String),
    /// emit a search and arm the await that the following `Resolve`
    /// blocks on.
    Search(String),
    Resolve {
        group: usize,
        term: String,
        prefs: Vec<Kind>,
        action: TermAction,
    },
    /// end-of-group tally, so a multi-term call reports once overall
    /// rather than only per-term.
    Summary {
        group: usize,
        past: &'static str,
        total: usize,
    },
}

#[derive(Debug, Clone, Default)]
struct Group {
    ok: usize,
}

// ------------------------------------------------------------- program

#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// the host should apply this now, then call `step` again.
    Emit(Op),
    /// the host should push this into the console scrollback.
    Note(String),
    /// results for `query` are not in yet; resume next frame.
    AwaitResults {
        query: String,
    },
    Failed(RuntimeError),
    Done,
}

#[derive(Debug, Clone)]
struct Pending {
    query: String,
    since: Duration,
}

/// A parsed, lowered program plus where it has got to. Driven by the
/// host: call `step` repeatedly, applying each `Emit` before the next
/// call, and stop for the frame on `AwaitResults` or `Done`. Never
/// blocks.
#[derive(Debug, Clone)]
pub struct Program {
    nodes: Vec<Node>,
    pc: usize,
    pending: Option<Pending>,
    groups: Vec<Group>,
    done: bool,
}

impl Program {
    pub fn parse(input: &str) -> Result<Program, ParseError> {
        let mut lowering = Lowering::default();
        for (offset, text) in statements(input) {
            let call = parse_call(input, offset, text)?;
            lowering.lower(&call, false);
        }
        let groups = vec![Group::default(); lowering.groups];
        Ok(Program {
            nodes: lowering.nodes,
            pc: 0,
            pending: None,
            groups,
            done: false,
        })
    }

    /// True once the program has nothing left to do; the host drops it.
    pub fn is_done(&self) -> bool {
        self.done || self.pc >= self.nodes.len()
    }

    pub fn step(&mut self, env: &Env) -> Step {
        if self.done {
            return Step::Done;
        }
        let Some(node) = self.nodes.get(self.pc).cloned() else {
            self.done = true;
            return Step::Done;
        };
        match node {
            Node::Emit(op) => {
                self.pc += 1;
                self.guard(env, op)
            }
            Node::Note(text) => {
                self.pc += 1;
                Step::Note(text)
            }
            Node::Search(query) => {
                self.pc += 1;
                self.pending = Some(Pending {
                    query: query.clone(),
                    since: env.now,
                });
                Step::Emit(Op::Search(query))
            }
            Node::Resolve {
                group,
                term,
                prefs,
                action,
            } => self.resolve(env, group, &term, &prefs, action),
            Node::Summary { group, past, total } => {
                self.pc += 1;
                let ok = self.groups.get(group).map(|g| g.ok).unwrap_or(0);
                Step::Note(format!("{past} {ok} of {total}"))
            }
        }
    }

    /// The few ops whose target has to exist before the host is asked
    /// to apply them; everything else is unconditional.
    fn guard(&mut self, env: &Env, op: Op) -> Step {
        let bad = match &op {
            Op::Go if env.results.is_empty() && !env.browsing => {
                Some(RuntimeError::NothingSelected)
            }
            Op::GotoQueue(n) if *n == 0 || *n > env.queue_len => Some(RuntimeError::OutOfRange {
                what: "queue position",
                index: *n,
                len: env.queue_len,
            }),
            Op::RemoveFromQueue(v) => v.iter().find(|n| **n == 0 || **n > env.queue_len).map(|n| {
                RuntimeError::OutOfRange {
                    what: "queue position",
                    index: *n,
                    len: env.queue_len,
                }
            }),
            _ => None,
        };
        match bad {
            Some(e) => self.fail(e),
            None => Step::Emit(op),
        }
    }

    fn fail(&mut self, e: RuntimeError) -> Step {
        self.done = true;
        Step::Failed(e)
    }

    fn resolve(
        &mut self,
        env: &Env,
        group: usize,
        term: &str,
        prefs: &[Kind],
        action: TermAction,
    ) -> Step {
        if let Some(pending) = self.pending.clone() {
            let waited = env.now.saturating_sub(pending.since);
            if waited >= AWAIT_TIMEOUT {
                return self.fail(RuntimeError::TimedOut(pending.query));
            }
            if env.query != pending.query || !env.search_settled {
                return Step::AwaitResults {
                    query: pending.query,
                };
            }
            self.pending = None;
        }
        self.pc += 1;
        match best_match(&env.results, prefs) {
            Some(index) => {
                if let Some(g) = self.groups.get_mut(group) {
                    g.ok += 1;
                }
                Step::Emit(action.op(index))
            }
            None => Step::Note(format!(
                "no {} match for '{term}'",
                prefs
                    .iter()
                    .map(|k| k.label())
                    .collect::<Vec<_>>()
                    .join("/")
            )),
        }
    }
}

/// First result of the most-preferred kind actually present. Results
/// arrive best-first within a category (see `Corpus::search`), so the
/// first of a kind *is* that kind's best match; this is the ambiguity
/// resolution docs/shellsong.md R7 describes, not a filter.
fn best_match(results: &[Kind], prefs: &[Kind]) -> Option<usize> {
    prefs
        .iter()
        .find_map(|want| results.iter().position(|k| k == want))
}

// -------------------------------------------------------------- lowering

#[derive(Default)]
struct Lowering {
    nodes: Vec<Node>,
    groups: usize,
}

impl Lowering {
    /// `dry` lowers the same shape and then turns every node into a
    /// note, so the report is generated from the real plan rather than
    /// a second description of it.
    fn lower(&mut self, call: &Call, dry: bool) {
        if dry {
            let mut inner = Lowering::default();
            inner.lower(call, false);
            self.nodes.push(Node::Note(format!("would run: {}", call)));
            for node in inner.nodes {
                if let Some(text) = describe_node(&node) {
                    self.nodes.push(Node::Note(text));
                }
            }
            return;
        }
        match call.command.name {
            "dry" => {
                if let Some(Arg::Call(inner)) = call.args.first() {
                    self.lower(inner, true);
                }
            }
            "repeat" => {
                let (Some(Arg::Count(n)), Some(Arg::Call(inner))) =
                    (call.args.first(), call.args.get(1))
                else {
                    return;
                };
                for _ in 0..*n {
                    self.lower(inner, false);
                }
            }
            "help" => self.lower_help(call),
            _ => self.lower_plain(call),
        }
    }

    fn lower_help(&mut self, call: &Call) {
        match call.args.first() {
            Some(Arg::Name(name)) => match lookup(name) {
                Some((c, _)) => self.nodes.push(Node::Note(help_line(c, 0))),
                None => self
                    .nodes
                    .push(Node::Note(format!("no command called '{name}'"))),
            },
            _ => {
                for line in help_lines() {
                    self.nodes.push(Node::Note(line));
                }
            }
        }
    }

    fn lower_terms(&mut self, call: &Call, terms: &[String], action: TermAction) {
        let prefs = call.prefs();
        let group = self.groups;
        self.groups += 1;
        for (i, term) in terms.iter().enumerate() {
            // play's first term plays and the rest queue behind it, so
            // `play([a, b])` is "play a, then b".
            let action = match (action, i) {
                (TermAction::Play, 0) => TermAction::Play,
                (TermAction::Play, _) => TermAction::Enqueue { front: false },
                (other, _) => other,
            };
            self.nodes.push(Node::Search(term.clone()));
            self.nodes.push(Node::Resolve {
                group,
                term: term.clone(),
                prefs: prefs.to_vec(),
                action,
            });
        }
        if terms.len() > 1 {
            self.nodes.push(Node::Summary {
                group,
                past: action.past(),
                total: terms.len(),
            });
        }
    }

    fn lower_plain(&mut self, call: &Call) {
        let name = call.command.name;
        let terms = call.terms();
        match name {
            "play" => {
                if let Some(t) = terms {
                    self.lower_terms(call, t, TermAction::Play);
                }
            }
            "queue" | "add_to_queue" => {
                let front = call.keyword(Words::Position) == Some("front");
                if let Some(t) = terms {
                    self.lower_terms(call, t, TermAction::Enqueue { front });
                }
            }
            "star" | "unstar" => {
                let on = name == "star";
                match terms {
                    Some(t) => self.lower_terms(call, t, TermAction::Star(on)),
                    None => self.nodes.push(Node::Emit(Op::StarSelection(on))),
                }
            }
            "download" => match terms {
                Some(t) => self.lower_terms(call, t, TermAction::Download),
                None => self.nodes.push(Node::Emit(Op::DownloadSelection)),
            },
            _ => {
                if let Some(op) = call.simple_op() {
                    self.nodes.push(Node::Emit(op));
                }
            }
        }
    }
}

/// How `dry` renders one planned node; `None` for bookkeeping nodes
/// that have no observable effect.
fn describe_node(node: &Node) -> Option<String> {
    match node {
        Node::Emit(op) => Some(format!("  {}", op.describe())),
        Node::Note(text) => Some(format!("  {text}")),
        Node::Search(q) => Some(format!("  search '{q}'")),
        Node::Resolve {
            term,
            prefs,
            action,
            ..
        } => Some(format!(
            "  {} the best {} match for '{term}'",
            action.past(),
            prefs
                .iter()
                .map(|k| k.label())
                .collect::<Vec<_>>()
                .join("/")
        )),
        Node::Summary { .. } => None,
    }
}

// ---------------------------------------------------------------- ast

#[derive(Debug, Clone, PartialEq)]
enum Arg {
    Terms(Vec<String>),
    Name(String),
    Pos(Pos),
    Offset(f64),
    Level(Level),
    Count(u32),
    Index(usize),
    Indices(Vec<usize>),
    Speed(SpeedArg),
    Duration(Option<f64>),
    Word(&'static str),
    Call(Box<Call>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum SpeedArg {
    Set(f64),
    Up,
    Down,
    Reset,
}

#[derive(Debug, Clone)]
struct Call {
    command: &'static Command,
    args: Vec<Arg>,
    /// `kind=` value, written or desugared from a name suffix.
    kind: Option<&'static str>,
}

impl PartialEq for Call {
    fn eq(&self, other: &Self) -> bool {
        self.command.name == other.command.name
            && self.args == other.args
            && self.kind == other.kind
    }
}

impl fmt::Display for Call {
    /// Canonical, fully desugared form; this is what makes a suffix
    /// observably equal to its `kind=` argument.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts: Vec<String> = self.args.iter().map(render_arg).collect();
        if let Some(kind) = self.kind {
            parts.push(format!("kind={kind}"));
        }
        write!(f, "{}({})", self.command.name, parts.join(", "))
    }
}

fn render_arg(arg: &Arg) -> String {
    match arg {
        Arg::Terms(t) if t.len() == 1 => t[0].clone(),
        Arg::Terms(t) => format!("[{}]", t.join(", ")),
        Arg::Name(n) => n.clone(),
        Arg::Pos(Pos::Percent(p)) => format!("{p}%"),
        Arg::Pos(Pos::Seconds(s)) => format!("{s}s"),
        Arg::Offset(s) => format!("{s:+}s"),
        Arg::Level(Level::Absolute(v)) => format!("{v}%"),
        Arg::Level(Level::Delta(d)) => format!("{d:+}%"),
        Arg::Count(n) => n.to_string(),
        Arg::Index(n) => n.to_string(),
        Arg::Indices(v) => format!(
            "[{}]",
            v.iter()
                .map(usize::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Arg::Speed(SpeedArg::Set(s)) => s.to_string(),
        Arg::Speed(SpeedArg::Up) => "up".into(),
        Arg::Speed(SpeedArg::Down) => "down".into(),
        Arg::Speed(SpeedArg::Reset) => "reset".into(),
        Arg::Duration(Some(s)) => format!("{s}s"),
        Arg::Duration(None) => "off".into(),
        Arg::Word(w) => (*w).to_string(),
        Arg::Call(c) => c.to_string(),
    }
}

impl Call {
    fn terms(&self) -> Option<&[String]> {
        self.args.iter().find_map(|a| match a {
            Arg::Terms(t) => Some(t.as_slice()),
            _ => None,
        })
    }

    /// Preference order after any `kind=` widening; the suffix and the
    /// keyword argument land in exactly the same place.
    fn prefs(&self) -> &'static [Kind] {
        self.kind.and_then(ladder).unwrap_or(self.command.prefs)
    }

    fn keyword(&self, set: Words) -> Option<&'static str> {
        let options = set.options();
        self.args.iter().find_map(|a| match a {
            Arg::Word(w) if options.contains(w) => Some(*w),
            _ => None,
        })
    }

    /// The op for every command whose whole behaviour is one op; the
    /// term-resolving and higher-order ones are handled in `Lowering`.
    fn simple_op(&self) -> Option<Op> {
        let first = self.args.first();
        Some(match self.command.name {
            "pause" => Op::TogglePause,
            "skip" => Op::SkipNext,
            "back" => Op::SkipBack,
            "moveto" => match first {
                Some(Arg::Pos(p)) => Op::SeekAbsolute(*p),
                _ => return None,
            },
            "seek" => match first {
                Some(Arg::Offset(s)) => Op::SeekRelative(*s),
                _ => return None,
            },
            "volume" => match first {
                Some(Arg::Level(l)) => Op::SetVolume(*l),
                _ => return None,
            },
            "mute" => Op::Mute(true),
            "unmute" => Op::Mute(false),
            "loop" => match self.keyword(Words::LoopMode) {
                Some("one") => Op::SetLoop(LoopMode::One),
                Some("all") => Op::SetLoop(LoopMode::All),
                Some("off") => Op::SetLoop(LoopMode::Off),
                _ => Op::CycleLoop,
            },
            "shuffle" => match self.keyword(Words::Switch) {
                Some("on") => Op::SetShuffle(true),
                Some("off") => Op::SetShuffle(false),
                _ => Op::ToggleShuffle,
            },
            "shuffle_queue" => Op::ShuffleQueue,
            "speed" => match first {
                Some(Arg::Speed(SpeedArg::Set(s))) => Op::SetSpeed(*s),
                Some(Arg::Speed(SpeedArg::Up)) => Op::SpeedUp,
                Some(Arg::Speed(SpeedArg::Down)) => Op::SpeedDown,
                Some(Arg::Speed(SpeedArg::Reset)) => Op::SetSpeed(1.0),
                _ => return None,
            },
            "eq" => match self.keyword(Words::EqPreset).and_then(eq::index_of) {
                Some(i) => Op::SetEqPreset(i),
                None => Op::OpenEqMenu,
            },
            "crossfade" => Op::ToggleCrossfade,
            "search" => match first {
                Some(Arg::Name(q)) => Op::Search(q.clone()),
                _ => return None,
            },
            "clear_queue" => Op::ClearQueue,
            "remove" => match first {
                Some(Arg::Indices(v)) => {
                    // descending, so the host can remove in order
                    // without every removal reindexing the next.
                    let mut v = v.clone();
                    v.sort_unstable_by(|a, b| b.cmp(a));
                    v.dedup();
                    Op::RemoveFromQueue(v)
                }
                _ => return None,
            },
            "goto" => match first {
                Some(Arg::Index(n)) => Op::GotoQueue(*n),
                _ => return None,
            },
            "save_queue" => match first {
                Some(Arg::Name(n)) => Op::SaveQueue(n.clone()),
                _ => return None,
            },
            "load_queue" => match first {
                Some(Arg::Name(n)) => Op::LoadQueue(n.clone()),
                _ => return None,
            },
            "go" => Op::Go,
            "up" => Op::MoveSelection(-1),
            "down" => Op::MoveSelection(1),
            "top" => Op::SelectTop,
            "bottom" => Op::SelectBottom,
            "show" => {
                let name = self.keyword(Words::Show)?;
                let target = SHOW_TARGETS.iter().find(|(n, _)| *n == name)?.1;
                Op::Show(target)
            }
            "lyrics" => Op::OpenLyrics,
            "starred" => Op::OpenStarred,
            "settings" => Op::OpenSettings,
            "keybinds" => Op::OpenKeybinds,
            "rescan" => Op::Rescan,
            "sleep" => match first {
                Some(Arg::Duration(d)) => Op::Sleep(*d),
                _ => return None,
            },
            "fade" => {
                let into = self.keyword(Words::FadeDir) == Some("in");
                let secs = match self.args.get(1) {
                    Some(Arg::Duration(Some(s))) => *s,
                    _ => DEFAULT_FADE_SECS,
                };
                Op::Fade { into, secs }
            }
            "quit" => Op::Quit,
            _ => return None,
        })
    }
}

// --------------------------------------------------------------- parser

/// Byte ranges of each statement in `input`, split on `;` and newlines
/// at nesting depth 0 with `#` comments stripped. Depth-aware so a call
/// may span lines.
fn statements(input: &str) -> Vec<(usize, &str)> {
    let bytes = input.as_bytes();
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut depth = 0i32;
    let mut quoted = false;
    let mut i = 0usize;
    while i < bytes.len() {
        let c = bytes[i] as char;
        match c {
            '"' => quoted = !quoted,
            '(' | '[' if !quoted => depth += 1,
            ')' | ']' if !quoted => depth -= 1,
            ';' | '\n' if !quoted && depth <= 0 => {
                push_statement(input, start, i, &mut out);
                start = i + 1;
            }
            '#' if !quoted => {
                push_statement(input, start, i, &mut out);
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    push_statement(input, start, bytes.len(), &mut out);
    out
}

fn push_statement<'a>(input: &'a str, from: usize, to: usize, out: &mut Vec<(usize, &'a str)>) {
    // a comment running to the very end of the input leaves `from` one
    // past it; nothing to push rather than a panicking slice.
    if from >= to || to > input.len() {
        return;
    }
    let raw = &input[from..to];
    let trimmed = raw.trim_start();
    let offset = from + (raw.len() - trimmed.len());
    let trimmed = trimmed.trim_end();
    if !trimmed.is_empty() {
        out.push((offset, trimmed));
    }
}

fn column(input: &str, offset: usize) -> usize {
    input[..offset.min(input.len())].chars().count() + 1
}

fn err(input: &str, offset: usize, token: &str, message: impl Into<String>) -> ParseError {
    ParseError {
        message: message.into(),
        column: column(input, offset),
        token: token.to_string(),
    }
}

/// Index of the `)` matching the `(` at `open`, quote- and depth-aware.
fn matching(text: &str, open: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0i32;
    let mut quoted = false;
    for (i, b) in bytes.iter().enumerate().skip(open) {
        match *b as char {
            '"' => quoted = !quoted,
            '(' | '[' if !quoted => depth += 1,
            ')' | ']' if !quoted => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Splits on commas at depth 0, keeping each piece's offset so an error
/// deep in an argument list still points at the right column.
fn split_commas(text: &str, base: usize) -> Vec<(usize, &str)> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut depth = 0i32;
    let mut quoted = false;
    for (i, b) in bytes.iter().enumerate() {
        match *b as char {
            '"' => quoted = !quoted,
            '(' | '[' if !quoted => depth += 1,
            ')' | ']' if !quoted => depth -= 1,
            ',' if !quoted && depth == 0 => {
                out.push(piece(text, start, i, base));
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(piece(text, start, bytes.len(), base));
    out
}

fn piece(text: &str, from: usize, to: usize, base: usize) -> (usize, &str) {
    let raw = &text[from..to];
    let lead = raw.len() - raw.trim_start().len();
    (base + from + lead, raw.trim())
}

/// `name(args...)`. `input`/`offset` are only carried for error
/// columns; `text` is the already-trimmed statement or nested call.
fn parse_call(input: &str, offset: usize, text: &str) -> Result<Call, ParseError> {
    let Some(lp) = text.find('(') else {
        let name = text.split_whitespace().next().unwrap_or(text);
        let message = if lookup(name).is_some() {
            format!("{name} is a command: write {name}()")
        } else {
            match did_you_mean(name) {
                Some(s) => format!("unknown command '{name}'; did you mean '{s}'?"),
                None => format!("unknown command '{name}'"),
            }
        };
        return Err(err(input, offset, name, message));
    };
    let name = text[..lp].trim();
    if name.is_empty() {
        return Err(err(input, offset, "(", "a call needs a name before its ("));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        return Err(err(
            input,
            offset,
            name,
            "command names are lowercase snake_case",
        ));
    }
    let Some(rp) = matching(text, lp) else {
        return Err(err(
            input,
            offset + lp,
            "(",
            format!("{name}( is never closed"),
        ));
    };
    let trailing = text[rp + 1..].trim();
    if !trailing.is_empty() {
        return Err(err(
            input,
            offset + rp + 1,
            trailing,
            "unexpected text after the closing )",
        ));
    }
    let (command, kind) = lookup(name).ok_or_else(|| {
        let message = match did_you_mean(name) {
            Some(s) => format!("unknown command '{name}'; did you mean '{s}'?"),
            None => format!("unknown command '{name}'"),
        };
        err(input, offset, name, message)
    })?;

    let inner = &text[lp + 1..rp];
    let mut raw: Vec<(usize, &str)> = if inner.trim().is_empty() {
        Vec::new()
    } else {
        split_commas(inner, offset + lp + 1)
    };
    // a trailing comma is allowed and leaves one empty tail piece.
    if raw.last().is_some_and(|(_, t)| t.is_empty()) {
        raw.pop();
    }
    if let Some((off, _)) = raw.iter().find(|(_, t)| t.is_empty()) {
        return Err(err(input, *off, "", format!("{name}: empty argument")));
    }

    let mut kind = kind;
    let mut positional: Vec<(usize, &str)> = Vec::new();
    for (off, piece) in raw {
        match kwarg(piece) {
            Some(("kind", value)) => {
                if command.prefs.is_empty() {
                    return Err(err(
                        input,
                        off,
                        piece,
                        format!("{name} does not search, so kind= means nothing here"),
                    ));
                }
                let Some(_) = ladder(value) else {
                    return Err(err(
                        input,
                        off,
                        value,
                        format!(
                            "kind= expects {}",
                            KIND_LADDER
                                .iter()
                                .map(|(n, _)| *n)
                                .collect::<Vec<_>>()
                                .join(" or ")
                        ),
                    ));
                };
                kind = Some(ladder_name(value));
            }
            Some((other, _)) => {
                return Err(err(
                    input,
                    off,
                    other,
                    format!("{name}: no argument called '{other}'"),
                ))
            }
            None => positional.push((off, piece)),
        }
    }

    let args = bind(input, command, &positional)?;
    Ok(Call {
        command,
        args,
        kind,
    })
}

fn ladder_name(value: &str) -> &'static str {
    KIND_LADDER
        .iter()
        .find(|(n, _)| *n == value)
        .map(|(n, _)| *n)
        .unwrap_or("song")
}

/// `ident=value`, used only for `kind=`; anything else (a bare term
/// that happens to contain `=`) is not a keyword argument.
fn kwarg(piece: &str) -> Option<(&str, &str)> {
    let eq = piece.find('=')?;
    let name = piece[..eq].trim();
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        return None;
    }
    Some((name, piece[eq + 1..].trim()))
}

/// Matches arguments to parameters. With exactly as many arguments as
/// parameters the match is strict positional, which is what makes
/// `add_to_queue(sideways, x)` an error naming the keyword set rather
/// than a confusing "too many arguments"; with fewer, optional
/// parameters whose value does not typecheck are skipped, which is what
/// makes `add_to_queue(x)` default its position.
fn bind(
    input: &str,
    command: &'static Command,
    given: &[(usize, &str)],
) -> Result<Vec<Arg>, ParseError> {
    let strict = given.len() == command.params.len();
    let mut args = Vec::new();
    let mut next = 0usize;
    for param in command.params {
        let Some((off, text)) = given.get(next) else {
            if param.required {
                return Err(err(
                    input,
                    given.last().map(|(o, _)| *o).unwrap_or(0),
                    "",
                    format!(
                        "{}: missing {} ({})",
                        command.name,
                        param.name,
                        param.ty.describe()
                    ),
                ));
            }
            continue;
        };
        match coerce(input, command, param, *off, text) {
            Ok(arg) => {
                args.push(arg);
                next += 1;
            }
            Err(e) => {
                if param.required || strict {
                    return Err(e);
                }
                // an optional parameter whose slot does not typecheck
                // was simply not supplied; try this argument against
                // the next parameter instead.
            }
        }
    }
    if next < given.len() {
        let (off, text) = given[next];
        return Err(err(
            input,
            off,
            text,
            format!(
                "{} takes {}; got {}",
                command.signature(),
                command.params.len(),
                given.len()
            ),
        ));
    }
    Ok(args)
}

fn coerce(
    input: &str,
    command: &'static Command,
    param: &Param,
    off: usize,
    text: &str,
) -> Result<Arg, ParseError> {
    let bad = |what: String| err(input, off, text, what);
    let expected = || {
        bad(format!(
            "{}: {} expects {}",
            command.name,
            param.name,
            param.ty.describe()
        ))
    };
    match param.ty {
        Ty::Terms => {
            let items = list_items(input, off, text)?;
            if items.iter().any(String::is_empty) {
                return Err(bad(format!("{}: an empty term", command.name)));
            }
            Ok(Arg::Terms(items))
        }
        Ty::Name => {
            if text.starts_with('[') {
                return Err(expected());
            }
            Ok(Arg::Name(unquote(text).to_string()))
        }
        Ty::Pos => match parse_time(text) {
            Ok((None, pos)) => Ok(Arg::Pos(pos)),
            Ok((Some(_), _)) => Err(bad(format!(
                "{} is absolute: write {} for a relative move",
                command.name,
                format_args!("seek({text})")
            ))),
            Err(why) => Err(bad(format!("{}: {why}", command.name))),
        },
        Ty::Offset => match parse_time(text) {
            Ok((Some(sign), Pos::Seconds(s))) => Ok(Arg::Offset(if sign == '-' { -s } else { s })),
            Ok((Some(_), Pos::Percent(_))) => Err(bad(
                "seek takes a time, not a percent; use moveto(50%)".to_string(),
            )),
            Ok((None, _)) => Err(bad(format!(
                "seek is relative and needs a sign: write seek(+{text}) or moveto({text})"
            ))),
            Err(why) => Err(bad(format!("seek: {why}"))),
        },
        Ty::Level => parse_level(text).map(Arg::Level).map_err(bad),
        Ty::Count => {
            let n: u32 = text.parse().map_err(|_| expected())?;
            if n == 0 {
                return Err(bad("a count of 0 would do nothing; use 1 or more".into()));
            }
            if n > MAX_REPEAT {
                return Err(bad(format!("a count above {MAX_REPEAT} is refused")));
            }
            Ok(Arg::Count(n))
        }
        Ty::Index => {
            let n: usize = text.parse().map_err(|_| expected())?;
            if n == 0 {
                return Err(bad("queue positions start at 1".into()));
            }
            Ok(Arg::Index(n))
        }
        Ty::Indices => {
            let mut out = Vec::new();
            for item in list_items(input, off, text)? {
                let n: usize = item.parse().map_err(|_| expected())?;
                if n == 0 {
                    return Err(bad("queue positions start at 1".into()));
                }
                out.push(n);
            }
            if out.is_empty() {
                return Err(expected());
            }
            Ok(Arg::Indices(out))
        }
        Ty::Speed => Ok(Arg::Speed(match text {
            "up" => SpeedArg::Up,
            "down" => SpeedArg::Down,
            "reset" => SpeedArg::Reset,
            _ => {
                let s: f64 = text.parse().map_err(|_| expected())?;
                if !(0.25..=3.0).contains(&s) {
                    return Err(bad("speed must be between 0.25 and 3".into()));
                }
                SpeedArg::Set(s)
            }
        })),
        Ty::Duration => {
            if text == "off" {
                return Ok(Arg::Duration(None));
            }
            match parse_time(text) {
                Ok((None, Pos::Seconds(s))) => Ok(Arg::Duration(Some(s))),
                Ok(_) => Err(expected()),
                Err(why) => Err(bad(format!("{}: {why}", command.name))),
            }
        }
        Ty::Words(set) => set
            .options()
            .into_iter()
            .find(|o| *o == text)
            .map(Arg::Word)
            .ok_or_else(expected),
        Ty::Call => {
            if !text.contains('(') {
                return Err(expected());
            }
            parse_call(input, off, text).map(|c| Arg::Call(Box::new(c)))
        }
    }
}

/// `[a, b]` or a single bare value; a single value is accepted anywhere
/// a list is, as a property of the type rather than per command
/// (docs/shellsong.md R5). Items are plain unquoted strings split on
/// commas, so `[4 star crest]` is one item; quotes are only needed for
/// an item containing a literal comma.
fn list_items(input: &str, off: usize, text: &str) -> Result<Vec<String>, ParseError> {
    if !text.starts_with('[') {
        return Ok(vec![unquote(text).to_string()]);
    }
    if !text.ends_with(']') {
        return Err(err(input, off, text, "[ is never closed"));
    }
    let inner = &text[1..text.len() - 1];
    if inner.trim().is_empty() {
        return Err(err(input, off, text, "an empty [] has nothing to act on"));
    }
    let mut items: Vec<String> = split_commas(inner, 0)
        .into_iter()
        .map(|(_, t)| unquote(t).to_string())
        .collect();
    if items.last().is_some_and(String::is_empty) {
        items.pop();
    }
    Ok(items)
}

fn unquote(text: &str) -> &str {
    text.strip_prefix('"')
        .and_then(|t| t.strip_suffix('"'))
        .unwrap_or(text)
        .trim()
}

/// `(sign, position)`. Accepts `50%`, `30s`, `1m20s`, `1h2m3s`, the
/// colon forms `1:20` / `1:02:03`, and a bare number meaning seconds.
/// Units must run h, m, s and may not repeat, so `1m2h` is refused
/// rather than silently reordered.
fn parse_time(text: &str) -> Result<(Option<char>, Pos), String> {
    let (sign, rest) = match text.strip_prefix('+') {
        Some(r) => (Some('+'), r),
        None => match text.strip_prefix('-') {
            Some(r) => (Some('-'), r),
            None => (None, text),
        },
    };
    if rest.is_empty() {
        return Err(format!("'{text}' is not a time"));
    }
    if let Some(num) = rest.strip_suffix('%') {
        let pct: f64 = num
            .parse()
            .map_err(|_| format!("'{num}' is not a number of percent"))?;
        if !(0.0..=100.0).contains(&pct) {
            return Err(format!("{pct}% is outside 0-100"));
        }
        return Ok((sign, Pos::Percent(pct)));
    }
    if rest.contains(':') {
        let parts: Vec<&str> = rest.split(':').collect();
        if parts.len() > 3 {
            return Err(format!("'{rest}' has too many : parts"));
        }
        let mut secs = 0.0f64;
        for part in &parts {
            let v: f64 = part
                .parse()
                .map_err(|_| format!("'{part}' is not a number"))?;
            if v < 0.0 {
                return Err(format!("'{rest}' has a negative part"));
            }
            secs = secs * 60.0 + v;
        }
        return Ok((sign, Pos::Seconds(secs)));
    }
    if let Ok(secs) = rest.parse::<f64>() {
        if secs < 0.0 {
            return Err(format!("'{rest}' is negative; put the sign in front"));
        }
        return Ok((sign, Pos::Seconds(secs)));
    }
    // unit form: h then m then s, each at most once.
    const UNITS: [(char, f64); 3] = [('h', 3600.0), ('m', 60.0), ('s', 1.0)];
    let mut secs = 0.0f64;
    let mut next_unit = 0usize;
    let mut digits = String::new();
    let mut saw_any = false;
    for ch in rest.chars() {
        if ch.is_ascii_digit() || ch == '.' {
            digits.push(ch);
            continue;
        }
        let Some(idx) = UNITS.iter().position(|(u, _)| *u == ch) else {
            return Err(format!("'{ch}' is not a time unit; use h, m or s"));
        };
        if idx < next_unit {
            return Err(format!("'{rest}': units must run h, m, s in that order"));
        }
        if digits.is_empty() {
            return Err(format!("'{rest}': {ch} has no number in front of it"));
        }
        let v: f64 = digits
            .parse()
            .map_err(|_| format!("'{digits}' is not a number"))?;
        secs += v * UNITS[idx].1;
        digits.clear();
        next_unit = idx + 1;
        saw_any = true;
    }
    if !digits.is_empty() {
        return Err(format!("'{rest}': {digits} has no unit"));
    }
    if !saw_any {
        return Err(format!("'{text}' is not a time"));
    }
    Ok((sign, Pos::Seconds(secs)))
}

fn parse_level(text: &str) -> Result<Level, String> {
    let (sign, pos) = parse_time(text)?;
    let value = match pos {
        Pos::Percent(p) => p,
        // a bare number is a percent here, since that is the only
        // thing a volume could mean.
        Pos::Seconds(s) => s,
    };
    Ok(match sign {
        Some('-') => Level::Delta(-value),
        Some(_) => Level::Delta(value),
        None => Level::Absolute(value),
    })
}

// -------------------------------------------------------------- console

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// a command the user submitted, echoed back.
    Command,
    Note,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub kind: LineKind,
    pub text: String,
}

/// What a keypress did to the console, for the host's input loop.
#[derive(Debug)]
pub enum ConsoleAction {
    None,
    Close,
    Run(Box<Program>),
}

const MAX_SCROLLBACK: usize = 200;
const MAX_HISTORY: usize = 200;

/// Console state: the input line being edited, the scrollback, and the
/// persisted command history. Rendering lives in `ui::shellsong`.
#[derive(Debug, Clone, Default)]
pub struct Console {
    pub input: String,
    /// byte index into `input`.
    pub cursor: usize,
    pub lines: Vec<Line>,
    pub error: Option<String>,
    history: Vec<String>,
    /// position in `history` while recalling; `None` while editing a
    /// fresh line.
    recall: Option<usize>,
    /// the fresh line, parked while recalling through history.
    draft: String,
}

impl Console {
    /// Loads the persisted history; the only I/O in this module.
    pub fn new() -> Console {
        Console {
            history: load_history(),
            ..Console::default()
        }
    }

    /// No-I/O constructor, for tests.
    pub fn empty() -> Console {
        Console::default()
    }

    pub fn push(&mut self, kind: LineKind, text: impl Into<String>) {
        self.lines.push(Line {
            kind,
            text: text.into(),
        });
        if self.lines.len() > MAX_SCROLLBACK {
            let overflow = self.lines.len() - MAX_SCROLLBACK;
            self.lines.drain(..overflow);
        }
    }

    pub fn note(&mut self, text: impl Into<String>) {
        self.push(LineKind::Note, text);
    }

    pub fn fail(&mut self, text: impl Into<String>) {
        self.push(LineKind::Error, text);
    }

    /// Recent commands, newest last; backs `show(history)`.
    pub fn history(&self) -> &[String] {
        &self.history
    }

    pub fn handle_key(&mut self, key: KeyCode) -> ConsoleAction {
        match key {
            KeyCode::Esc => return ConsoleAction::Close,
            KeyCode::Enter => return self.submit(),
            KeyCode::Char(c) => {
                self.error = None;
                edit_insert(&mut self.input, &mut self.cursor, c);
            }
            KeyCode::Backspace => {
                self.error = None;
                edit_backspace(&mut self.input, &mut self.cursor);
            }
            KeyCode::Delete => {
                if let Some(next) = next_boundary(&self.input, self.cursor) {
                    self.input.replace_range(self.cursor..next, "");
                }
            }
            KeyCode::Left => {
                if let Some(prev) = prev_boundary(&self.input, self.cursor) {
                    self.cursor = prev;
                }
            }
            KeyCode::Right => {
                if let Some(next) = next_boundary(&self.input, self.cursor) {
                    self.cursor = next;
                }
            }
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.input.len(),
            KeyCode::Up => self.recall_back(),
            KeyCode::Down => self.recall_forward(),
            KeyCode::Tab => self.complete(),
            _ => {}
        }
        ConsoleAction::None
    }

    /// Bracketed paste; goes in literally, with no auto-pairing (a
    /// pasted `[` already has its own `]`).
    pub fn paste(&mut self, text: &str) {
        let text: String = text.chars().filter(|c| *c != '\n' && *c != '\r').collect();
        self.input.insert_str(self.cursor, &text);
        self.cursor += text.len();
    }

    fn submit(&mut self) -> ConsoleAction {
        let line = self.input.trim().to_string();
        if line.is_empty() {
            return ConsoleAction::Close;
        }
        self.push(LineKind::Command, format!(": {line}"));
        match Program::parse(&line) {
            Ok(program) => {
                self.remember(line);
                self.input.clear();
                self.cursor = 0;
                self.error = None;
                ConsoleAction::Run(Box::new(program))
            }
            Err(e) => {
                let text = e.to_string();
                self.fail(text.clone());
                self.error = Some(text);
                ConsoleAction::None
            }
        }
    }

    fn remember(&mut self, line: String) {
        self.recall = None;
        self.draft.clear();
        if self.history.last() == Some(&line) {
            return;
        }
        self.history.push(line);
        if self.history.len() > MAX_HISTORY {
            let overflow = self.history.len() - MAX_HISTORY;
            self.history.drain(..overflow);
        }
        save_history(&self.history);
    }

    fn recall_back(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let next = match self.recall {
            None => {
                self.draft = self.input.clone();
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.recall = Some(next);
        self.input = self.history[next].clone();
        self.cursor = self.input.len();
    }

    fn recall_forward(&mut self) {
        let Some(i) = self.recall else { return };
        if i + 1 < self.history.len() {
            self.recall = Some(i + 1);
            self.input = self.history[i + 1].clone();
        } else {
            self.recall = None;
            self.input = std::mem::take(&mut self.draft);
        }
        self.cursor = self.input.len();
    }

    /// Completes the token under the cursor: command names outside
    /// parens, and whatever keyword set the enclosing command's
    /// parameter declares inside them. Both come from `COMMANDS`, so a
    /// completion that appears always parses.
    fn complete(&mut self) {
        let (start, prefix) = token_at(&self.input, self.cursor);
        let candidates = self.candidates(start, &prefix);
        let hits: Vec<&str> = candidates
            .into_iter()
            .filter(|c| c.starts_with(&prefix))
            .collect();
        if hits.is_empty() {
            return;
        }
        let common = longest_common_prefix(&hits);
        if common.len() > prefix.len() {
            let insert = &common[prefix.len()..];
            self.input.insert_str(self.cursor, insert);
            self.cursor += insert.len();
        }
        if hits.len() > 1 {
            self.note(hits.join("  "));
        } else if !self.input[self.cursor..].starts_with('(')
            && !self.input[..self.cursor].contains('(')
        {
            // a unique command name gets its parens, since every
            // command needs them.
            edit_insert(&mut self.input, &mut self.cursor, '(');
        }
    }

    fn candidates(&self, start: usize, prefix: &str) -> Vec<&'static str> {
        match enclosing(&self.input, start) {
            None => every_name(),
            Some((name, arg)) => match lookup(name) {
                Some((c, _)) => c
                    .params
                    .get(arg)
                    .map(|p| p.ty.options())
                    .unwrap_or_default(),
                // a nested call argument completes command names.
                None if prefix.is_empty() => Vec::new(),
                None => every_name(),
            },
        }
    }
}

/// `(start, text)` of the word the cursor sits in; words break on
/// anything that is not a name character.
fn token_at(input: &str, cursor: usize) -> (usize, String) {
    let upto = &input[..cursor.min(input.len())];
    let start = upto
        .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .map(|i| i + 1)
        .unwrap_or(0);
    (start, upto[start..].to_string())
}

/// `(command name, argument index)` of the call the position `at` sits
/// inside, or `None` when it is at statement level.
fn enclosing(input: &str, at: usize) -> Option<(&str, usize)> {
    let before = &input[..at.min(input.len())];
    let mut depth = 0i32;
    let mut open: Option<usize> = None;
    let mut commas = 0usize;
    let mut quoted = false;
    for (i, c) in before.char_indices() {
        match c {
            '"' => quoted = !quoted,
            '(' if !quoted => {
                depth += 1;
                if depth == 1 {
                    open = Some(i);
                    commas = 0;
                }
            }
            ')' if !quoted => depth -= 1,
            ',' if !quoted && depth == 1 => commas += 1,
            _ => {}
        }
    }
    if depth < 1 {
        return None;
    }
    let lp = open?;
    let name_start = before[..lp]
        .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .map(|i| i + 1)
        .unwrap_or(0);
    Some((&before[name_start..lp], commas))
}

fn longest_common_prefix(items: &[&str]) -> String {
    let Some(first) = items.first() else {
        return String::new();
    };
    let mut len = first.len();
    for item in &items[1..] {
        len = len.min(
            first
                .bytes()
                .zip(item.bytes())
                .take_while(|(a, b)| a == b)
                .count(),
        );
    }
    first[..len].to_string()
}

// --------------------------------------------------------- editor pairs

/// Openers that auto-close. One line per pair is the whole rule.
const PAIRS: &[(char, char)] = &[('(', ')'), ('[', ']')];

fn closer(open: char) -> Option<char> {
    PAIRS.iter().find(|(o, _)| *o == open).map(|(_, c)| *c)
}

fn is_closer(ch: char) -> bool {
    PAIRS.iter().any(|(_, c)| *c == ch)
}

/// True when `cursor` sits inside a quoted string, where brackets are
/// literal text and must not be paired.
fn in_string(buf: &str, cursor: usize) -> bool {
    buf[..cursor.min(buf.len())].matches('"').count() % 2 == 1
}

/// Inserts `ch`, auto-closing an opener and stepping over a closer that
/// is already there. Pure over (buffer, cursor, key), which is how it
/// is tested.
pub fn edit_insert(buf: &mut String, cursor: &mut usize, ch: char) {
    let quoted = in_string(buf, *cursor);
    if !quoted {
        if let Some(close) = closer(ch) {
            buf.insert(*cursor, close);
            buf.insert(*cursor, ch);
            *cursor += ch.len_utf8();
            return;
        }
        if is_closer(ch) && buf[*cursor..].starts_with(ch) {
            *cursor += ch.len_utf8();
            return;
        }
    }
    buf.insert(*cursor, ch);
    *cursor += ch.len_utf8();
}

/// Deletes backwards, taking an empty pair out in one go so backspacing
/// an auto-inserted `(` does not leave its `)` behind.
pub fn edit_backspace(buf: &mut String, cursor: &mut usize) {
    let Some(prev) = prev_boundary(buf, *cursor) else {
        return;
    };
    let before = buf[prev..*cursor].chars().next();
    let after = buf[*cursor..].chars().next();
    let empty_pair = match (before, after) {
        (Some(o), Some(c)) => closer(o) == Some(c),
        _ => false,
    };
    if empty_pair {
        let end = next_boundary(buf, *cursor).unwrap_or(*cursor);
        buf.replace_range(prev..end, "");
    } else {
        buf.replace_range(prev..*cursor, "");
    }
    *cursor = prev;
}

fn prev_boundary(buf: &str, cursor: usize) -> Option<usize> {
    if cursor == 0 {
        return None;
    }
    buf[..cursor].char_indices().next_back().map(|(i, _)| i)
}

fn next_boundary(buf: &str, cursor: usize) -> Option<usize> {
    buf[cursor.min(buf.len())..]
        .chars()
        .next()
        .map(|c| cursor + c.len_utf8())
}

// ------------------------------------------------------------- history

/// Alongside `persistence.rs`'s state file, but a plain newline-
/// delimited list rather than toml: it is a shell history, appended to
/// and read back in order, and nothing else needs to parse it.
fn history_path() -> Option<PathBuf> {
    let dir = dirs::state_dir().or_else(dirs::data_dir)?;
    Some(dir.join("wim").join("shellsong_history"))
}

fn load_history() -> Vec<String> {
    let Some(path) = history_path() else {
        return Vec::new();
    };
    fs::read_to_string(path)
        .map(|raw| {
            raw.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Best-effort, same leniency as every other state file here: a
/// read-only state dir costs you history, not your session.
fn save_history(history: &[String]) {
    let Some(path) = history_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(path, history.join("\n"));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs a program to completion against a fake world, standing in
    /// for the host: a search is "applied" by mirroring the query back
    /// into `Env`, and results are whatever the test says they are.
    fn drive(input: &str, results: Vec<Kind>) -> (Vec<Op>, Vec<String>) {
        let mut program = Program::parse(input).expect("should parse");
        let mut env = Env {
            search_settled: true,
            browsing: true,
            queue_len: 10,
            results,
            ..Env::default()
        };
        let mut ops = Vec::new();
        let mut notes = Vec::new();
        for _ in 0..1024 {
            match program.step(&env) {
                Step::Emit(Op::Search(q)) => {
                    env.query = q.clone();
                    ops.push(Op::Search(q));
                }
                Step::Emit(op) => ops.push(op),
                Step::Note(text) => notes.push(text),
                Step::Done => break,
                other => panic!("unexpected step {other:?}"),
            }
        }
        (ops, notes)
    }

    fn ops(input: &str) -> Vec<Op> {
        drive(input, vec![Kind::Song]).0
    }

    fn notes(input: &str) -> Vec<String> {
        drive(input, vec![Kind::Song]).1
    }

    /// Only the ops that acted on a resolved result, so a test about
    /// kind preference does not have to spell out the searches.
    fn resolve_with(input: &str, results: Vec<Kind>) -> Vec<Op> {
        drive(input, results)
            .0
            .into_iter()
            .filter(|op| !matches!(op, Op::Search(_)))
            .collect()
    }

    fn error(input: &str) -> ParseError {
        Program::parse(input).expect_err("should not parse")
    }

    // ------------------------------------------------------- the table

    /// Drives straight off `COMMANDS`, so a command added without a
    /// lowering, or a synonym that collides with another command's
    /// name, cannot pass.
    #[test]
    fn every_name_in_the_table_resolves_to_exactly_one_command() {
        let mut seen: Vec<&str> = Vec::new();
        for c in COMMANDS {
            for name in std::iter::once(c.name).chain(c.aliases.iter().copied()) {
                assert!(!seen.contains(&name), "'{name}' is declared twice");
                seen.push(name);
                let (found, kind) = lookup(name).unwrap_or_else(|| panic!("'{name}' not found"));
                assert_eq!(found.name, c.name, "'{name}' resolved to the wrong command");
                assert_eq!(kind, None);
            }
        }
    }

    /// Every command must lower to *something*; a table entry with no
    /// lowering would parse and then silently do nothing.
    #[test]
    fn every_command_lowers_to_at_least_one_node() {
        for c in COMMANDS {
            let input = sample_call(c);
            let program =
                Program::parse(&input).unwrap_or_else(|e| panic!("{} did not parse: {e}", input));
            assert!(
                !program.nodes.is_empty(),
                "{input} parsed but lowered to nothing"
            );
        }
    }

    /// A syntactically valid invocation of `c`, built from its own
    /// parameter list so this stays correct as the table grows.
    fn sample_call(c: &Command) -> String {
        let args: Vec<String> = c
            .params
            .iter()
            .filter(|p| p.required)
            .map(|p| sample_arg(p.ty))
            .collect();
        format!("{}({})", c.name, args.join(", "))
    }

    fn sample_arg(ty: Ty) -> String {
        match ty {
            Ty::Terms => "crest".into(),
            Ty::Name => "crest".into(),
            Ty::Pos => "30s".into(),
            Ty::Offset => "+30s".into(),
            Ty::Level => "50%".into(),
            Ty::Count => "2".into(),
            Ty::Index => "1".into(),
            Ty::Indices => "[1, 2]".into(),
            Ty::Speed => "1.5".into(),
            Ty::Duration => "30m".into(),
            Ty::Words(w) => w.options()[0].to_string(),
            Ty::Call => "skip()".into(),
        }
    }

    #[test]
    fn help_is_generated_from_the_same_table_the_parser_reads() {
        let lines = help_lines();
        assert_eq!(lines.len(), COMMANDS.len());
        for (line, c) in lines.iter().zip(COMMANDS) {
            assert!(
                line.starts_with(&c.signature()),
                "help line for {} is {line}",
                c.name
            );
            for alias in c.aliases {
                assert!(line.contains(alias), "{} is missing alias {alias}", c.name);
            }
        }
        // canonical names only in the first column (R3)
        assert!(lines.iter().all(|l| !l.starts_with("fav(")));
    }

    #[test]
    fn help_with_a_name_reports_just_that_command() {
        let out = notes("help(shuffle)");
        assert_eq!(out.len(), 1);
        assert!(out[0].starts_with("shuffle(state?)"), "{out:?}");
    }

    // ---------------------------------------------------- bare words

    #[test]
    fn a_bare_word_says_to_write_the_parens() {
        let e = error("pause");
        assert!(
            e.message.contains("write pause()"),
            "unhelpful: {}",
            e.message
        );
        assert_eq!(e.column, 1);
    }

    #[test]
    fn the_old_bareword_chain_is_no_longer_a_program() {
        assert!(Program::parse("search crest go go").is_err());
        assert!(Program::parse("forward2").is_err());
    }

    #[test]
    fn an_unknown_command_suggests_the_closest_known_one() {
        assert!(error("shufle()").message.contains("did you mean 'shuffle'"));
        assert!(error("pasue()").message.contains("did you mean 'pause'"));
        assert!(error("quue([x])").message.contains("did you mean 'queue'"));
    }

    #[test]
    fn a_name_with_no_near_neighbour_just_says_unknown() {
        let e = error("xyzzyplover()");
        assert!(e.message.contains("unknown command"), "{}", e.message);
        assert!(!e.message.contains("did you mean"), "{}", e.message);
    }

    // ---------------------------------------------------- zero-arg ops

    #[test]
    fn zero_argument_commands_need_empty_parens_and_nothing_else() {
        assert_eq!(ops("pause()"), vec![Op::TogglePause]);
        assert_eq!(ops("skip()"), vec![Op::SkipNext]);
        assert_eq!(ops("back()"), vec![Op::SkipBack]);
        assert_eq!(ops("clear_queue()"), vec![Op::ClearQueue]);
        assert_eq!(ops("shuffle()"), vec![Op::ToggleShuffle]);
        assert_eq!(ops("crossfade()"), vec![Op::ToggleCrossfade]);
        assert_eq!(ops("quit()"), vec![Op::Quit]);
    }

    #[test]
    fn transport_synonyms_all_reach_the_same_op() {
        for name in ["skip", "next", "forward", "fwd"] {
            assert_eq!(ops(&format!("{name}()")), vec![Op::SkipNext], "{name}");
        }
        for name in ["back", "prev", "previous"] {
            assert_eq!(ops(&format!("{name}()")), vec![Op::SkipBack], "{name}");
        }
        for name in ["pause", "toggle", "play_pause"] {
            assert_eq!(ops(&format!("{name}()")), vec![Op::TogglePause], "{name}");
        }
    }

    #[test]
    fn the_whole_favourites_synonym_set_collapses_to_two_ops() {
        for name in ["star", "favorite", "fav", "like"] {
            assert_eq!(
                ops(&format!("{name}()")),
                vec![Op::StarSelection(true)],
                "{name}"
            );
        }
        for name in ["unstar", "unfavorite", "unfav", "unlike"] {
            assert_eq!(
                ops(&format!("{name}()")),
                vec![Op::StarSelection(false)],
                "{name}"
            );
        }
    }

    #[test]
    fn modes_cycle_bare_and_set_with_a_keyword() {
        assert_eq!(ops("loop()"), vec![Op::CycleLoop]);
        assert_eq!(ops("loop(one)"), vec![Op::SetLoop(LoopMode::One)]);
        assert_eq!(ops("loop(all)"), vec![Op::SetLoop(LoopMode::All)]);
        assert_eq!(ops("loop(off)"), vec![Op::SetLoop(LoopMode::Off)]);
        assert_eq!(ops("shuffle(on)"), vec![Op::SetShuffle(true)]);
        assert_eq!(ops("shuffle(off)"), vec![Op::SetShuffle(false)]);
        assert_eq!(ops("speed(up)"), vec![Op::SpeedUp]);
        assert_eq!(ops("speed(down)"), vec![Op::SpeedDown]);
        assert_eq!(ops("speed(1.5)"), vec![Op::SetSpeed(1.5)]);
        assert_eq!(ops("speed(reset)"), vec![Op::SetSpeed(1.0)]);
    }

    #[test]
    fn eq_opens_the_menu_bare_and_applies_a_named_preset() {
        assert_eq!(ops("eq()"), vec![Op::OpenEqMenu]);
        assert_eq!(ops("equalizer()"), vec![Op::OpenEqMenu]);
        let rock = eq::index_of("rock").unwrap();
        assert_eq!(ops("eq(rock)"), vec![Op::SetEqPreset(rock)]);
        assert_eq!(ops("eq(flat)"), vec![Op::SetEqPreset(0)]);
        assert!(error("eq(disco)").message.contains("flat"));
    }

    // ------------------------------------------------------ statements

    #[test]
    fn statements_separate_on_semicolons_and_newlines_and_drop_comments() {
        assert_eq!(ops("pause(); skip()"), vec![Op::TogglePause, Op::SkipNext]);
        assert_eq!(
            ops("pause()\nskip()\n"),
            vec![Op::TogglePause, Op::SkipNext]
        );
        assert_eq!(
            ops("pause() # this is a comment\nskip()"),
            vec![Op::TogglePause, Op::SkipNext]
        );
        assert_eq!(ops("# nothing but a comment"), Vec::<Op>::new());
    }

    #[test]
    fn a_call_may_span_lines_because_splitting_is_depth_aware() {
        assert_eq!(terms_of("queue([hex,\n  crest])"), ["hex", "crest"]);
    }

    /// The search terms a program lowered to, in order.
    fn terms_of(input: &str) -> Vec<String> {
        Program::parse(input)
            .unwrap()
            .nodes
            .iter()
            .filter_map(|n| match n {
                Node::Search(q) => Some(q.clone()),
                _ => None,
            })
            .collect()
    }

    // ----------------------------------------------------- higher order

    #[test]
    fn repeat_expands_statically() {
        assert_eq!(
            ops("repeat(3, skip())"),
            vec![Op::SkipNext, Op::SkipNext, Op::SkipNext]
        );
        assert_eq!(
            ops("repeat(2, seek(+10s))"),
            vec![Op::SeekRelative(10.0), Op::SeekRelative(10.0)]
        );
    }

    #[test]
    fn repeat_refuses_a_count_that_would_do_nothing_or_too_much() {
        assert!(error("repeat(0, skip())")
            .message
            .contains("0 would do nothing"));
        assert!(error("repeat(9999, skip())").message.contains("refused"));
        assert!(error("repeat(two, skip())")
            .message
            .contains("positive whole number"));
    }

    #[test]
    fn repeat_nests_inside_dry_and_dry_emits_nothing() {
        assert!(
            ops("dry(repeat(2, seek(+10s)))").is_empty(),
            "dry must not emit ops"
        );
        let out = notes("dry(repeat(2, seek(+10s)))");
        assert_eq!(out[0], "would run: repeat(2, seek(+10s))");
        assert_eq!(out.len(), 3, "the header plus one line per planned op");
    }

    #[test]
    fn dry_reports_the_plan_of_a_searching_call_without_searching() {
        assert!(ops("dry(queue([hex, crest]))").is_empty());
        let out = notes("dry(queue([hex, crest]))");
        assert!(out.iter().any(|l| l.contains("search 'hex'")), "{out:?}");
        assert!(
            out.iter()
                .any(|l| l.contains("queued the best song match for 'crest'")),
            "{out:?}"
        );
    }

    /// R6: a suffix is sugar for `kind=` and nothing else, and `dry`
    /// has to be able to prove it.
    #[test]
    fn a_kind_suffix_desugars_observably_to_a_keyword_argument() {
        let suffixed = notes("dry(queue_aa([crest]))");
        let explicit = notes("dry(queue([crest], kind=album))");
        assert_eq!(suffixed, explicit);
        assert_eq!(suffixed[0], "would run: queue(crest, kind=album)");
    }

    #[test]
    fn explain_is_a_synonym_for_dry() {
        assert_eq!(notes("explain(skip())"), notes("dry(skip())"));
    }

    // ------------------------------------------------------ term lists

    #[test]
    fn bracket_items_split_only_on_commas_so_spaces_stay_inside_an_item() {
        assert_eq!(
            terms_of("queue([hex, 4 star crest])"),
            ["hex", "4 star crest"]
        );
    }

    #[test]
    fn a_single_value_is_interchangeable_with_a_one_element_list() {
        for (single, list) in [
            ("queue(flatline)", "queue([flatline])"),
            ("star(flatline)", "star([flatline])"),
            ("play(flatline)", "play([flatline])"),
            ("download(flatline)", "download([flatline])"),
            ("remove(2)", "remove([2])"),
        ] {
            assert_eq!(
                Program::parse(single).unwrap().nodes,
                Program::parse(list).unwrap().nodes,
                "{single} vs {list}"
            );
        }
    }

    #[test]
    fn quotes_are_optional_and_only_needed_for_a_literal_comma() {
        assert_eq!(
            terms_of(r#"queue(["hex, really", crest])"#),
            ["hex, really", "crest"]
        );
    }

    #[test]
    fn a_trailing_comma_is_fine_but_an_empty_list_is_not() {
        assert_eq!(
            Program::parse("queue([hex, crest,])").unwrap().nodes,
            Program::parse("queue([hex, crest])").unwrap().nodes
        );
        assert!(error("queue([])").message.contains("empty []"));
        assert!(error("queue([hex,,crest])").message.contains("empty term"));
    }

    #[test]
    fn malformed_calls_report_where_they_broke() {
        assert!(error("queue([hex").message.contains("never closed"));
        assert!(error("queue(hex) junk")
            .message
            .contains("after the closing"));
        let e = error("pause() ; QUEUE(hex)");
        assert!(e.message.contains("snake_case"), "{}", e.message);
        assert_eq!(e.column, 11, "column should point at the offending name");
    }

    // --------------------------------------------------- arity + types

    #[test]
    fn a_missing_required_argument_names_the_command_and_the_type() {
        let e = error("add_to_queue()");
        assert!(e.message.contains("add_to_queue"), "{}", e.message);
        assert!(e.message.contains("terms"), "{}", e.message);
        let e = error("moveto()");
        assert!(e.message.contains("30s"), "{}", e.message);
    }

    #[test]
    fn too_many_arguments_report_the_signature() {
        let e = error("skip(2)");
        assert!(e.message.contains("skip()"), "{}", e.message);
    }

    #[test]
    fn an_unknown_position_keyword_names_the_set_it_expected() {
        let e = error("add_to_queue(sideways, flatline)");
        assert!(e.message.contains("front or back"), "{}", e.message);
    }

    #[test]
    fn an_omitted_optional_position_falls_back_to_the_default() {
        let back = Program::parse("add_to_queue(flatline)").unwrap();
        let explicit = Program::parse("add_to_queue(back, flatline)").unwrap();
        assert_eq!(back.nodes, explicit.nodes);
        let front = Program::parse("add_to_queue(front, flatline)").unwrap();
        assert_ne!(front.nodes, back.nodes);
    }

    #[test]
    fn an_unknown_keyword_argument_is_refused_by_name() {
        assert!(error("queue([x], sort=asc)")
            .message
            .contains("no argument called 'sort'"));
        assert!(error("pause(kind=album)")
            .message
            .contains("does not search"));
        assert!(error("queue([x], kind=mixtape)")
            .message
            .contains("kind= expects"));
    }

    // ---------------------------------------------- kind preferences

    /// The ambiguous case the author described: "crest" matching both.
    /// `queue` prefers songs and never falls through to the album;
    /// `queue_aa` still prefers the song when one exists.
    #[test]
    fn queue_prefers_a_song_when_a_term_matches_both_a_song_and_an_album() {
        let both = vec![Kind::Album, Kind::Song];
        assert_eq!(
            resolve_with("queue(crest)", both.clone()),
            vec![Op::EnqueueIndex {
                index: 1,
                front: false
            }]
        );
        assert_eq!(
            resolve_with("queue_aa(crest)", both),
            vec![Op::EnqueueIndex {
                index: 1,
                front: false
            }],
            "a song is still the best match even with albums allowed"
        );
    }

    #[test]
    fn queue_declines_an_album_but_queue_aa_takes_it() {
        let albums_only = vec![Kind::Album, Kind::Artist];
        assert!(
            resolve_with("queue(crest)", albums_only.clone()).is_empty(),
            "a plain queue must not build itself out of albums"
        );
        assert_eq!(
            resolve_with("queue_aa(crest)", albums_only.clone()),
            vec![Op::EnqueueIndex {
                index: 0,
                front: false
            }]
        );
        assert!(
            resolve_with("queue_aa(crest)", vec![Kind::Artist]).is_empty(),
            "_aa widens to albums, not to artists"
        );
        assert_eq!(
            resolve_with("queue_ar(crest)", vec![Kind::Artist]),
            vec![Op::EnqueueIndex {
                index: 0,
                front: false
            }],
            "_ar is the rung that allows artists"
        );
    }

    /// R7's deliberate asymmetry: play is wider than queue.
    #[test]
    fn play_accepts_an_album_where_queue_would_not() {
        assert_eq!(
            resolve_with("play(crest)", vec![Kind::Album]),
            vec![Op::PlayIndex(0)]
        );
        assert_eq!(
            resolve_with("play(crest)", vec![Kind::Song, Kind::Album]),
            vec![Op::PlayIndex(0)]
        );
        assert!(resolve_with("play(crest)", vec![Kind::Artist]).is_empty());
    }

    #[test]
    fn play_with_several_terms_plays_the_first_and_queues_the_rest() {
        assert_eq!(
            resolve_with("play([a, b, c])", vec![Kind::Song]),
            vec![
                Op::PlayIndex(0),
                Op::EnqueueIndex {
                    index: 0,
                    front: false
                },
                Op::EnqueueIndex {
                    index: 0,
                    front: false
                }
            ]
        );
    }

    #[test]
    fn a_term_that_matches_nothing_is_reported_and_the_rest_continue() {
        let mut program = Program::parse("queue([a, b, c])").unwrap();
        // only the middle term finds a song; the others see an
        // artist-only result list.
        let mut emitted = Vec::new();
        let mut reported = Vec::new();
        let mut term = 0usize;
        let mut query = String::new();
        for _ in 0..64 {
            let env = Env {
                search_settled: true,
                query: query.clone(),
                results: if term == 2 {
                    vec![Kind::Song]
                } else {
                    vec![Kind::Artist]
                },
                ..Env::default()
            };
            match program.step(&env) {
                Step::Emit(Op::Search(q)) => {
                    query = q;
                    term += 1;
                }
                Step::Emit(op) => emitted.push(op),
                Step::Note(text) => reported.push(text),
                Step::Done => break,
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(emitted.len(), 1, "only the matching term acts");
        assert_eq!(reported.len(), 3, "two misses plus the tally");
        assert!(reported.iter().any(|r| r.contains("no song match for 'a'")));
        assert!(reported.iter().any(|r| r.contains("no song match for 'c'")));
        assert_eq!(reported.last().unwrap(), "queued 1 of 3");
    }

    // --------------------------------------------------- await/resume

    #[test]
    fn a_resolve_waits_for_the_search_it_follows() {
        let mut program = Program::parse("queue(flatline)").unwrap();
        let mut env = Env::default();
        assert_eq!(
            program.step(&env),
            Step::Emit(Op::Search("flatline".to_string()))
        );
        // the host has not applied the query yet
        assert_eq!(
            program.step(&env),
            Step::AwaitResults {
                query: "flatline".to_string()
            }
        );
        // query applied, remote search still outstanding
        env.query = "flatline".to_string();
        env.results = vec![Kind::Song];
        assert_eq!(
            program.step(&env),
            Step::AwaitResults {
                query: "flatline".to_string()
            },
            "must not act on a half-filled result list"
        );
        // settled: now it resolves
        env.search_settled = true;
        assert_eq!(
            program.step(&env),
            Step::Emit(Op::EnqueueIndex {
                index: 0,
                front: false
            })
        );
        assert_eq!(program.step(&env), Step::Done);
        assert!(program.is_done());
    }

    #[test]
    fn a_result_that_never_arrives_times_out_instead_of_hanging() {
        let mut program = Program::parse("queue(flatline)").unwrap();
        let mut env = Env::default();
        assert!(matches!(program.step(&env), Step::Emit(Op::Search(_))));
        for _ in 0..200 {
            env.now += Duration::from_millis(100);
            if let Step::Failed(e) = program.step(&env) {
                assert_eq!(e, RuntimeError::TimedOut("flatline".to_string()));
                assert!(program.is_done(), "a failed program must stop");
                assert_eq!(program.step(&env), Step::Done);
                return;
            }
        }
        panic!("the await never timed out");
    }

    #[test]
    fn the_await_resumes_as_soon_as_the_host_says_it_settled() {
        let mut program = Program::parse("play(crest)").unwrap();
        let mut env = Env::default();
        assert!(matches!(program.step(&env), Step::Emit(Op::Search(_))));
        for _ in 0..5 {
            env.now += Duration::from_millis(100);
            assert!(matches!(program.step(&env), Step::AwaitResults { .. }));
        }
        env.query = "crest".to_string();
        env.results = vec![Kind::Album];
        env.search_settled = true;
        assert_eq!(program.step(&env), Step::Emit(Op::PlayIndex(0)));
    }

    // -------------------------------------------------------- guards

    #[test]
    fn go_with_nothing_to_act_on_fails_rather_than_emitting() {
        let mut program = Program::parse("go()").unwrap();
        let env = Env::default();
        assert_eq!(
            program.step(&env),
            Step::Failed(RuntimeError::NothingSelected)
        );
    }

    #[test]
    fn go_is_fine_whenever_there_is_a_selection_or_a_browse_view() {
        let env = Env {
            results: vec![Kind::Song],
            ..Env::default()
        };
        assert_eq!(
            Program::parse("go()").unwrap().step(&env),
            Step::Emit(Op::Go)
        );
        let browsing = Env {
            browsing: true,
            ..Env::default()
        };
        assert_eq!(
            Program::parse("enter()").unwrap().step(&browsing),
            Step::Emit(Op::Go)
        );
    }

    #[test]
    fn queue_positions_are_checked_against_the_real_queue() {
        let env = Env {
            queue_len: 3,
            ..Env::default()
        };
        assert_eq!(
            Program::parse("goto(4)").unwrap().step(&env),
            Step::Failed(RuntimeError::OutOfRange {
                what: "queue position",
                index: 4,
                len: 3
            })
        );
        assert_eq!(
            Program::parse("goto(2)").unwrap().step(&env),
            Step::Emit(Op::GotoQueue(2))
        );
        assert!(error("goto(0)").message.contains("start at 1"));
    }

    #[test]
    fn remove_hands_back_descending_deduped_positions() {
        let env = Env {
            queue_len: 9,
            ..Env::default()
        };
        assert_eq!(
            Program::parse("remove([2, 5, 2])").unwrap().step(&env),
            Step::Emit(Op::RemoveFromQueue(vec![5, 2]))
        );
    }

    // ---------------------------------------------- time and position

    #[test]
    fn time_literals_normalise_to_seconds_or_a_percent() {
        assert_eq!(
            ops("moveto(10%)"),
            vec![Op::SeekAbsolute(Pos::Percent(10.0))]
        );
        assert_eq!(
            ops("moveto(12.5%)"),
            vec![Op::SeekAbsolute(Pos::Percent(12.5))]
        );
        assert_eq!(
            ops("moveto(10s)"),
            vec![Op::SeekAbsolute(Pos::Seconds(10.0))]
        );
        assert_eq!(
            ops("moveto(1m20s)"),
            vec![Op::SeekAbsolute(Pos::Seconds(80.0))]
        );
        assert_eq!(
            ops("moveto(2m)"),
            vec![Op::SeekAbsolute(Pos::Seconds(120.0))]
        );
        assert_eq!(
            ops("moveto(1h2m3s)"),
            vec![Op::SeekAbsolute(Pos::Seconds(3723.0))]
        );
        assert_eq!(
            ops("moveto(90)"),
            vec![Op::SeekAbsolute(Pos::Seconds(90.0))]
        );
        assert_eq!(
            ops("moveto(1:20)"),
            vec![Op::SeekAbsolute(Pos::Seconds(80.0))]
        );
        assert_eq!(
            ops("moveto(1:02:03)"),
            vec![Op::SeekAbsolute(Pos::Seconds(3723.0))]
        );
        assert_eq!(ops("move(30s)"), vec![Op::SeekAbsolute(Pos::Seconds(30.0))]);
    }

    #[test]
    fn nonsense_time_literals_say_what_they_got_and_what_they_wanted() {
        assert!(error("moveto(1m2h)")
            .message
            .contains("h, m, s in that order"));
        assert!(error("moveto(10x)").message.contains("not a time unit"));
        assert!(error("moveto(110%)").message.contains("outside 0-100"));
        assert!(error("moveto(m)").message.contains("no number in front"));
        assert!(error("moveto(10m20)").message.contains("has no unit"));
    }

    /// R8: the sign is what distinguishes relative from absolute, in
    /// both directions, and neither reading is ever guessed at.
    #[test]
    fn sign_enforcement_runs_both_ways() {
        assert_eq!(ops("seek(+30s)"), vec![Op::SeekRelative(30.0)]);
        assert_eq!(ops("seek(-10s)"), vec![Op::SeekRelative(-10.0)]);
        let e = error("seek(30s)");
        assert!(e.message.contains("seek(+30s)"), "{}", e.message);
        assert!(e.message.contains("moveto(30s)"), "{}", e.message);
        let e = error("moveto(+30s)");
        assert!(e.message.contains("absolute"), "{}", e.message);
        assert!(error("seek(+50%)").message.contains("not a percent"));
    }

    #[test]
    fn volume_takes_an_absolute_level_or_a_nudge() {
        assert_eq!(
            ops("volume(50%)"),
            vec![Op::SetVolume(Level::Absolute(50.0))]
        );
        assert_eq!(
            ops("volume(50)"),
            vec![Op::SetVolume(Level::Absolute(50.0))]
        );
        assert_eq!(ops("volume(+10%)"), vec![Op::SetVolume(Level::Delta(10.0))]);
        assert_eq!(
            ops("volume(-10%)"),
            vec![Op::SetVolume(Level::Delta(-10.0))]
        );
        assert_eq!(ops("vol(0%)"), vec![Op::SetVolume(Level::Absolute(0.0))]);
        assert!(error("volume(101%)").message.contains("outside 0-100"));
    }

    #[test]
    fn sleep_takes_a_duration_or_cancels() {
        assert_eq!(ops("sleep(30m)"), vec![Op::Sleep(Some(1800.0))]);
        assert_eq!(ops("sleep(off)"), vec![Op::Sleep(None)]);
        assert_eq!(
            ops("fade(out, 10s)"),
            vec![Op::Fade {
                into: false,
                secs: 10.0
            }]
        );
        assert_eq!(
            ops("fade(in)"),
            vec![Op::Fade {
                into: true,
                secs: DEFAULT_FADE_SECS
            }]
        );
    }

    #[test]
    fn show_targets_come_from_the_same_table_completion_offers() {
        assert_eq!(ops("show(queue)"), vec![Op::Show(ShowTarget::Queue)]);
        assert_eq!(ops("ls(now)"), vec![Op::Show(ShowTarget::Now)]);
        for (name, target) in SHOW_TARGETS {
            assert_eq!(ops(&format!("show({name})")), vec![Op::Show(*target)]);
        }
        assert!(error("show(everything)").message.contains("queue or now"));
    }

    #[test]
    fn every_op_describes_itself_for_the_scrollback() {
        // R9: nothing is silent, so no op may describe itself as "".
        for c in COMMANDS {
            for op in ops(&sample_call(c)) {
                assert!(!op.describe().is_empty(), "{:?} has no description", op);
            }
        }
    }

    // ---------------------------------------------------- editor pairs

    fn typed(keys: &str) -> (String, usize) {
        let mut buf = String::new();
        let mut cursor = 0usize;
        for ch in keys.chars() {
            edit_insert(&mut buf, &mut cursor, ch);
        }
        (buf, cursor)
    }

    #[test]
    fn an_opening_bracket_inserts_its_partner_and_parks_the_cursor_inside() {
        assert_eq!(typed("["), ("[]".to_string(), 1));
        assert_eq!(typed("("), ("()".to_string(), 1));
        assert_eq!(typed("queue("), ("queue()".to_string(), 6));
        assert_eq!(typed("queue(["), ("queue([])".to_string(), 7));
    }

    #[test]
    fn typing_a_closer_over_its_own_partner_steps_past_it() {
        let (buf, cursor) = typed("queue([hex])");
        assert_eq!(buf, "queue([hex])", "no duplicated brackets");
        assert_eq!(cursor, buf.len());
    }

    #[test]
    fn backspace_takes_an_empty_pair_out_in_one_go() {
        let mut buf = "queue()".to_string();
        let mut cursor = 6usize;
        edit_backspace(&mut buf, &mut cursor);
        assert_eq!(buf, "queue");
        assert_eq!(cursor, 5);
    }

    #[test]
    fn backspace_on_a_non_empty_pair_only_removes_one_character() {
        let mut buf = "queue(hex)".to_string();
        let mut cursor = 9usize;
        edit_backspace(&mut buf, &mut cursor);
        assert_eq!(buf, "queue(he)");
        assert_eq!(cursor, 8);
    }

    #[test]
    fn brackets_inside_a_quoted_string_are_plain_text() {
        let (buf, cursor) = typed("queue([\"a[b\"");
        assert_eq!(buf, "queue([\"a[b\"])");
        assert_eq!(cursor, 12);
        // and a closer inside a string does not step over anything
        let mut buf = "x(\")".to_string();
        let mut cursor = 3usize;
        edit_insert(&mut buf, &mut cursor, ')');
        assert_eq!(buf, "x(\"))");
    }

    #[test]
    fn multibyte_input_never_slices_mid_character() {
        let (buf, cursor) = typed("queue([日本");
        assert_eq!(buf, "queue([日本])");
        assert_eq!(cursor, 13);
        let mut buf = buf;
        let mut cursor = cursor;
        edit_backspace(&mut buf, &mut cursor);
        assert_eq!(buf, "queue([日])");
    }

    // -------------------------------------------------------- console

    fn console_with(input: &str) -> Console {
        let mut c = Console::empty();
        for ch in input.chars() {
            c.handle_key(KeyCode::Char(ch));
        }
        c
    }

    #[test]
    fn submitting_a_good_line_clears_the_input_and_returns_a_program() {
        let mut c = console_with("pause()");
        // the auto-paired ')' was stepped over, so the buffer is exact
        assert_eq!(c.input, "pause()");
        match c.handle_key(KeyCode::Enter) {
            ConsoleAction::Run(_) => {}
            other => panic!("expected a program, got {other:?}"),
        }
        assert!(c.input.is_empty());
        assert!(c.error.is_none());
        assert_eq!(c.history(), ["pause()"]);
    }

    #[test]
    fn submitting_a_bad_line_keeps_it_and_shows_the_error() {
        let mut c = console_with("shufle()");
        assert!(matches!(c.handle_key(KeyCode::Enter), ConsoleAction::None));
        assert_eq!(c.input, "shufle()", "a bad line stays so it can be fixed");
        let error = c.error.clone().expect("an error should be shown");
        assert!(error.contains("did you mean 'shuffle'"), "{error}");
        assert!(error.starts_with("col 1:"), "{error}");
        assert!(c.history().is_empty(), "a bad line is not remembered");
        // the next keystroke clears the error so it does not linger
        c.handle_key(KeyCode::Backspace);
        assert!(c.error.is_none());
    }

    #[test]
    fn esc_closes_and_so_does_enter_on_an_empty_line() {
        let mut c = Console::empty();
        assert!(matches!(c.handle_key(KeyCode::Esc), ConsoleAction::Close));
        assert!(matches!(c.handle_key(KeyCode::Enter), ConsoleAction::Close));
    }

    #[test]
    fn up_and_down_walk_the_history_and_restore_the_draft() {
        let mut c = Console::empty();
        c.history = vec!["pause()".into(), "skip()".into()];
        for ch in "half".chars() {
            c.handle_key(KeyCode::Char(ch));
        }
        c.handle_key(KeyCode::Up);
        assert_eq!(c.input, "skip()");
        c.handle_key(KeyCode::Up);
        assert_eq!(c.input, "pause()");
        c.handle_key(KeyCode::Up);
        assert_eq!(c.input, "pause()", "clamps at the oldest entry");
        c.handle_key(KeyCode::Down);
        assert_eq!(c.input, "skip()");
        c.handle_key(KeyCode::Down);
        assert_eq!(c.input, "half", "the half-typed line comes back");
    }

    #[test]
    fn tab_completes_a_unique_command_and_adds_its_parens() {
        let mut c = console_with("crossf");
        c.handle_key(KeyCode::Tab);
        assert_eq!(c.input, "crossfade()");
        assert_eq!(c.cursor, 10);
    }

    #[test]
    fn tab_on_an_ambiguous_prefix_fills_the_common_part_and_lists() {
        let mut c = console_with("s");
        c.handle_key(KeyCode::Tab);
        // every s-name shares nothing past "s", so the listing is the
        // useful part
        assert!(c
            .lines
            .last()
            .is_some_and(|l| l.text.contains("skip") && l.text.contains("star")));
    }

    #[test]
    fn tab_inside_a_call_completes_that_parameters_keyword_set() {
        let mut c = console_with("eq(ro");
        c.handle_key(KeyCode::Tab);
        assert_eq!(c.input, "eq(rock)");
        let mut c = console_with("show(q");
        c.handle_key(KeyCode::Tab);
        assert_eq!(c.input, "show(queue)");
        let mut c = console_with("loop(o");
        c.handle_key(KeyCode::Tab);
        assert!(c.lines.last().is_some_and(|l| l.text.contains("one")));
    }

    #[test]
    fn the_scrollback_is_capped_so_a_long_session_cannot_grow_forever() {
        let mut c = Console::empty();
        for i in 0..(MAX_SCROLLBACK + 50) {
            c.note(format!("line {i}"));
        }
        assert_eq!(c.lines.len(), MAX_SCROLLBACK);
        assert_eq!(
            c.lines.last().unwrap().text,
            format!("line {}", MAX_SCROLLBACK + 49)
        );
    }

    #[test]
    fn paste_goes_in_literally_without_pairing_or_newlines() {
        let mut c = Console::empty();
        c.paste("queue([a, b])\n");
        assert_eq!(c.input, "queue([a, b])");
        assert_eq!(c.cursor, c.input.len());
    }
}
