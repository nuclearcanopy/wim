//! shellsong: a command language for the player. everything is a call
//! (`pause()`, `queue([hex, 4 star crest])`), lowered here into `Op`s
//! the host applies through the same paths its keybinds already use, so
//! behaviour can never drift from the keyboard's. search is
//! asynchronous, so `Program` is a resumable state machine rather than
//! a loop. the full language reference is docs/shellsong.md.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::app::LoopMode;
use crate::eq;
use crate::search::SearchItem;
use crate::ui::textfit;

/// The key that opens the console. Also listed in
/// `keymap::reserved_key_purpose` so no action can be bound onto it
/// and then silently shadowed by this.
pub const OPEN_KEY: KeyCode = KeyCode::Char(':');

/// How long a `Resolve` waits for the search it follows before giving
/// up: a backstop for a host whose searches never settle, so a wedged
/// search subsystem reports instead of hanging the program forever.
/// Unreachable against the current host, which settles within 1.2s and
/// instantly with no indexed server.
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
/// which `dry()` reports. One line per suffix. There is deliberately no
/// artist rung: an artist expands to a whole discography, which is
/// never a queue anyone asked for (`kind=any` is the explicit escape
/// hatch). See docs/shellsong.md.
const KIND_SUFFIXES: &[(&str, &str)] = &[("_aa", "album"), ("_any", "any")];

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

/// A volume request, in percent. `Absolute` is refused above 100 at
/// parse time; `Delta` applies to mpv's current volume and the host
/// clamps the result to 0-100. The ceiling is real: above 100 mpv
/// applies digital gain, which clips.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Level {
    Absolute(f64),
    Delta(f64),
}

/// What a term-taking command acts on. Omitting the terms means "the
/// current selection", uniformly across every such command, which is
/// what keeps one name to one meaning (docs/shellsong.md R11).
///
/// **Load-bearing host contract, do not break:** `Index` is an index
/// into `Env::results`, and `App::resync_results` reorders that list
/// every time a remote search outcome lands. The index is therefore
/// only valid until the next frame boundary. It is safe solely because
/// the host applies each `Step::Emit` synchronously before calling
/// `step` again, so nothing can reorder the list between resolving an
/// index and using it. A host that queued emitted ops to apply later,
/// or stepped once per frame while holding a resolved index, would act
/// on the wrong track intermittently. Surfaced by the wiring work;
/// nothing enforces it at the type level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// index into `Env::results`, resolved by the executor from a term.
    Index(usize),
    /// whatever is highlighted right now.
    Selection,
}

/// Which pane owns the keyboard. `go()` means "what enter does", and
/// what enter does depends on this, so the executor resolves it rather
/// than making the host re-derive it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Focus {
    #[default]
    Sidebar,
    Workspace,
    BottomBar,
}

/// The resolved meaning of `go()`, mirroring the host's own enter
/// precedence: a focused browse view claims enter, then the bottom
/// bar's focused field, otherwise the sidebar's selected result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoTarget {
    /// enter on the selected search result.
    Result,
    /// enter inside the open browse view.
    BrowseRow,
    /// enter on the focused bottom-bar field.
    BottomBarField,
}

/// What `view(target)` puts on screen. `view` is the one and only
/// screen-opening verb; every menu and listing is a target here rather
/// than its own command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewTarget {
    Queue,
    Now,
    Suggestions,
    Settings,
    Keybinds,
    Lyrics,
    Starred,
    Index,
    History,
    Playlists,
    Eq,
    /// The command listing. Unlike every other target this is generated
    /// here rather than read from the app, so it lowers to the same
    /// notes `help()` produces instead of to an op; `view` still means
    /// "put this on screen" either way.
    Help,
}

impl ViewTarget {
    /// True for the targets that take over the screen, as opposed to
    /// printing a listing into the scrollback. The console closes for
    /// the former, since it would sit on top of what just opened.
    pub fn is_screen(self) -> bool {
        !matches!(
            self,
            ViewTarget::Queue
                | ViewTarget::Index
                | ViewTarget::History
                | ViewTarget::Playlists
                | ViewTarget::Help
        )
    }
}

/// The literal that switches `view` into its search form, plus its
/// alias. Not a target, so it lives beside them rather than in
/// `ViewTarget`.
const VIEW_QUERY_WORDS: &[&str] = &["q", "query"];

/// Every spelling `view` accepts. `canonical` is what completion and
/// `help()` offer; the parser takes every row, which is how
/// `suggest` / `suggestion` / `suggestions` all land on one target.
struct ViewName {
    word: &'static str,
    target: ViewTarget,
    canonical: bool,
}

const fn view(word: &'static str, target: ViewTarget, canonical: bool) -> ViewName {
    ViewName {
        word,
        target,
        canonical,
    }
}

const VIEW_NAMES: &[ViewName] = &[
    view("queue", ViewTarget::Queue, true),
    view("now", ViewTarget::Now, true),
    view("player", ViewTarget::Now, false),
    view("suggestions", ViewTarget::Suggestions, true),
    view("suggestion", ViewTarget::Suggestions, false),
    view("suggest", ViewTarget::Suggestions, false),
    view("settings", ViewTarget::Settings, true),
    view("keybinds", ViewTarget::Keybinds, true),
    view("keybind", ViewTarget::Keybinds, false),
    view("lyrics", ViewTarget::Lyrics, true),
    view("starred", ViewTarget::Starred, true),
    view("favorites", ViewTarget::Starred, false),
    view("index", ViewTarget::Index, true),
    view("history", ViewTarget::History, true),
    view("playlists", ViewTarget::Playlists, true),
    view("eq", ViewTarget::Eq, true),
    view("equalizer", ViewTarget::Eq, false),
    view("help", ViewTarget::Help, true),
];

fn view_target(word: &str) -> Option<ViewTarget> {
    VIEW_NAMES.iter().find(|v| v.word == word).map(|v| v.target)
}

fn view_name(target: ViewTarget) -> &'static str {
    VIEW_NAMES
        .iter()
        .find(|v| v.target == target && v.canonical)
        .map(|v| v.word)
        .unwrap_or("?")
}

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
    /// index into `eq::PRESETS`.
    SetEqPreset(usize),
    ToggleCrossfade,
    /// replace the query and re-filter, exactly as typing it would.
    Search(String),
    MoveSelection(i32),
    SelectTop,
    SelectBottom,
    /// whatever enter would do, already resolved against the focus.
    Go(GoTarget),
    Play(Target),
    Enqueue {
        target: Target,
        front: bool,
    },
    Star {
        target: Target,
        on: bool,
    },
    Download(Target),
    /// open the page a result belongs to (an artist's or album's own
    /// view); `view(q, ...)` resolves to this.
    OpenPage(Target),
    ClearQueue,
    /// 1-based queue positions, descending so the host can remove in
    /// order without reindexing.
    RemoveFromQueue(Vec<usize>),
    GotoQueue(usize),
    ShuffleQueue,
    SaveQueue(String),
    LoadQueue(String),
    /// the one screen-opening op; `ViewTarget::is_screen` says whether
    /// it takes over the screen or prints a listing.
    View(ViewTarget),
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
            Op::SetEqPreset(i) => {
                format!("eq: {}", eq::PRESETS.get(*i).map(|p| p.name).unwrap_or("?"))
            }
            Op::ToggleCrossfade => "crossfade toggled".into(),
            Op::Search(q) => format!("search: {q}"),
            Op::MoveSelection(d) if *d < 0 => "up".into(),
            Op::MoveSelection(_) => "down".into(),
            Op::SelectTop => "top".into(),
            Op::SelectBottom => "bottom".into(),
            Op::Go(GoTarget::Result) => "opened the selected result".into(),
            Op::Go(GoTarget::BrowseRow) => "played from the selected row".into(),
            Op::Go(GoTarget::BottomBarField) => "tweaked the focused field".into(),
            Op::Play(_) => "playing".into(),
            Op::Enqueue { front: true, .. } => "queued next".into(),
            Op::Enqueue { front: false, .. } => "queued".into(),
            Op::Star { on: true, .. } => "starred".into(),
            Op::Star { on: false, .. } => "unstarred".into(),
            Op::Download(_) => "downloading".into(),
            Op::OpenPage(_) => "opened its page".into(),
            Op::ClearQueue => "queue cleared".into(),
            Op::RemoveFromQueue(v) => format!("removed {} from the queue", v.len()),
            Op::GotoQueue(n) => format!("jumped to queue {n}"),
            Op::ShuffleQueue => "queue shuffled".into(),
            Op::SaveQueue(n) => format!("saved queue as '{n}'"),
            Op::LoadQueue(n) => format!("loaded queue '{n}'"),
            Op::View(t) => format!("viewing {}", view_name(*t)),
            Op::Rescan => "rescanning the library".into(),
            Op::Sleep(Some(s)) => format!("sleep in {s}s"),
            Op::Sleep(None) => "sleep cancelled".into(),
            Op::Fade { into: true, secs } => format!("fading in over {secs}s"),
            Op::Fade { into: false, secs } => format!("fading out over {secs}s"),
            Op::Quit => "bye".into(),
        }
    }

    /// True for ops that put a view on screen. The console is a centred
    /// overlay, so it has to get out of the way before one is applied
    /// or it covers whatever was just opened; this is the one place
    /// that decides, rather than each view-opener remembering to.
    pub fn closes_console(&self) -> bool {
        match self {
            Op::View(t) => t.is_screen(),
            // opening a page replaces the workspace behind the console,
            // which is the point of asking for it.
            Op::OpenPage(_) | Op::Quit => true,
            _ => false,
        }
    }
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
    /// free text to search for; completes from the local corpus.
    Query,
    /// a name the user is inventing (a queue to save); nothing to
    /// complete from, by definition.
    NewName,
    /// the name of something already saved; completes from the host's
    /// saved queues.
    SavedName,
    /// the name of a command, for `help`.
    CommandName,
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
    /// every `view` target plus the `q` literal that switches it into
    /// its search form.
    ViewTarget,
    FadeDir,
}

impl Words {
    /// What completion offers and `help()` prints: canonical spellings
    /// only, so the list stays readable even where the parser accepts
    /// several words for one thing.
    pub fn options(self) -> Vec<&'static str> {
        match self {
            Words::Position => vec!["front", "back"],
            Words::LoopMode => vec!["one", "all", "off"],
            Words::Switch => vec!["on", "off"],
            Words::EqPreset => eq::PRESETS.iter().map(|p| p.name).collect(),
            Words::ViewTarget => VIEW_QUERY_WORDS[..1]
                .iter()
                .copied()
                .chain(VIEW_NAMES.iter().filter(|v| v.canonical).map(|v| v.word))
                .collect(),
            Words::FadeDir => vec!["in", "out"],
        }
    }

    /// Every spelling the parser accepts, which is a superset of
    /// `options`; validation reads this so an alias still parses even
    /// though completion does not advertise it.
    fn accepts(self) -> Vec<&'static str> {
        match self {
            Words::ViewTarget => VIEW_QUERY_WORDS
                .iter()
                .copied()
                .chain(VIEW_NAMES.iter().map(|v| v.word))
                .collect(),
            other => other.options(),
        }
    }
}

/// How a parameter completes. Every `Ty` resolves to exactly one of
/// these: either there is a candidate list worth inserting, or there is
/// not and the user gets a hint instead. Guessing `volume(5` into
/// `50%` is the absurdity this split exists to prevent; there is no
/// sane candidate list for a number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Completes {
    /// a fixed keyword set, from `COMMANDS` itself.
    Words(Words),
    /// names from the host's local corpus.
    CorpusTerms,
    /// command names.
    CommandNames,
    /// the host's saved queue names.
    SavedQueues,
    /// nothing insertable; show this instead, and never accept it.
    Hint(&'static str),
}

impl Completes {
    /// True for the classes that produce text the user can accept.
    pub fn insertable(self) -> bool {
        !matches!(self, Completes::Hint(_))
    }
}

impl Ty {
    /// The single source of truth for what this type completes to. A
    /// new `Ty` must land in exactly one class here; the coverage test
    /// fails otherwise rather than silently completing nothing.
    pub fn completes(self) -> Completes {
        match self {
            Ty::Terms | Ty::Query => Completes::CorpusTerms,
            Ty::SavedName => Completes::SavedQueues,
            Ty::CommandName | Ty::Call => Completes::CommandNames,
            Ty::Words(w) => Completes::Words(w),
            Ty::NewName => Completes::Hint("a name to save it under"),
            Ty::Pos => Completes::Hint("10% / 90 / 1m20s"),
            Ty::Offset => Completes::Hint("+30s / -10s (sign required)"),
            Ty::Level => Completes::Hint("0-100, or +N% / -N%"),
            Ty::Count => Completes::Hint("how many times, 1-256"),
            Ty::Index => Completes::Hint("a queue position, 1 or higher"),
            Ty::Indices => Completes::Hint("1, or [1, 3, 5]"),
            Ty::Speed => Completes::Hint("0.25-3.0, or up / down / reset"),
            Ty::Duration => Completes::Hint("30m / off"),
        }
    }

    /// Keywords this type accepts. Unused: the parser's own error path
    /// reaches `Words::options` directly, and completion goes through
    /// `completes`. Kept because every other `Ty` accessor is here and
    /// a type that accepts keywords without a way to list them is the
    /// surprising half of the pair.
    #[allow(dead_code)]
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
            Ty::Query => "something to search for".into(),
            Ty::NewName => "a name".into(),
            Ty::SavedName => "the name of a saved queue".into(),
            Ty::CommandName => "a command name".into(),
            Ty::Pos => "an unsigned position like 30s, 1m20s or 50%".into(),
            Ty::Offset => "a signed offset like +30s or -10s".into(),
            Ty::Level => "a percent like 50% or +10%".into(),
            Ty::Count => "a positive whole number".into(),
            Ty::Index => "a queue position (1 or higher)".into(),
            Ty::Indices => "one or more queue positions".into(),
            Ty::Speed => "a multiplier like 1.5, or up / down / reset".into(),
            Ty::Duration => "a duration like 30m, or off".into(),
            Ty::Words(w) => {
                let options = w.options();
                // two reads as a choice, more reads as a list.
                if options.len() <= 2 {
                    options.join(" or ")
                } else {
                    format!("one of: {}", options.join(", "))
                }
            }
            Ty::Call => "a call like skip()".into(),
        }
    }
}

#[derive(Debug)]
pub struct Param {
    pub name: &'static str,
    pub ty: Ty,
    pub required: bool,
    /// Appended to the "missing argument" message when the type alone
    /// does not explain which call the user probably wanted; empty for
    /// most parameters.
    pub hint: &'static str,
}

const fn req(name: &'static str, ty: Ty) -> Param {
    Param {
        name,
        ty,
        required: true,
        hint: "",
    }
}

const fn req_hint(name: &'static str, ty: Ty, hint: &'static str) -> Param {
    Param {
        name,
        ty,
        required: true,
        hint,
    }
}

const fn opt(name: &'static str, ty: Ty) -> Param {
    Param {
        name,
        ty,
        required: false,
        hint: "",
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
/// `view(q, ...)` wants the page a term belongs to, so it prefers the
/// kinds that *have* pages and takes a song last.
const VIEW: &[Kind] = &[Kind::Artist, Kind::Album, Kind::Playlist, Kind::Song];

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
        &[req("preset", Ty::Words(Words::EqPreset))],
        &[],
        "apply an eq preset; view(eq) opens the menu",
    ),
    cmd("crossfade", &["xfade"], NONE, &[], "toggle crossfade"),
    // search and playback
    cmd(
        "search",
        &["find", "grep"],
        &[req("query", Ty::Query)],
        &[],
        "run a search and leave the results up",
    ),
    cmd(
        "play",
        &[],
        &[opt("terms", Ty::Terms)],
        SONG_ALBUM,
        "play the first term, queue the rest",
    ),
    cmd(
        "queue",
        &["enqueue"],
        &[opt("terms", Ty::Terms)],
        SONG,
        "append each term to the queue",
    ),
    cmd(
        "add_to_queue",
        &[],
        &[
            req_hint(
                "position",
                Ty::Words(Words::Position),
                "front inserts next; queue(...) is how you append",
            ),
            opt("terms", Ty::Terms),
        ],
        SONG,
        "insert next (front) or append (back)",
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
        &[req("name", Ty::NewName)],
        &[],
        "save the queue as a local playlist",
    ),
    cmd(
        "load_queue",
        &[],
        &[req("name", Ty::SavedName)],
        &[],
        "replace the queue with a saved playlist",
    ),
    cmd(
        "star",
        &["favorite", "fav", "like"],
        &[opt("terms", Ty::Terms)],
        SONG_ALBUM,
        "star what each term finds",
    ),
    cmd(
        "unstar",
        &["unfavorite", "unfav", "unlike"],
        &[opt("terms", Ty::Terms)],
        SONG_ALBUM,
        "unstar what each term finds",
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
        "view",
        &["show", "ls"],
        &[
            req_hint(
                "target",
                Ty::Words(Words::ViewTarget),
                "or view(q, terms) to search and open the best match",
            ),
            opt("terms", Ty::Terms),
        ],
        VIEW,
        "put something on screen; the only screen-opening command",
    ),
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
        &[opt("name", Ty::CommandName)],
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
            if widenable(c) {
                return Some((c, Some(kind)));
            }
        }
    }
    None
}

/// The `kind=` ladder widens *from songs*, so only a song-rooted
/// command can take it. `view` prefers artists and albums by design,
/// which is a different order rather than a narrower one, so neither
/// `kind=` nor a suffix applies to it.
fn widenable(c: &Command) -> bool {
    c.prefs.first() == Some(&Kind::Song)
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

/// Closest known name to `name`, for "did you mean".
fn did_you_mean(name: &str) -> Option<&'static str> {
    closest(name, &every_name())
}

/// `strsim`, same dependency search.rs recovers typos with, but
/// Jaro-Winkler rather than a plain edit distance: a candidate is often
/// much longer than what was typed ("suggst" against "suggestions"),
/// where an edit-distance budget scaled to the typed length refuses
/// every real answer. Prefix weighting also suits a half-typed name.
const SUGGEST_FLOOR: u32 = 820;

fn closest(name: &str, among: &[&'static str]) -> Option<&'static str> {
    let mut hits: Vec<(u32, &'static str)> = among
        .iter()
        .map(|c| ((strsim::jaro_winkler(name, c) * 1000.0) as u32, *c))
        .filter(|(score, _)| *score >= SUGGEST_FLOOR)
        .collect();
    // closest, then shortest, then alphabetical: deterministic all the
    // way down.
    hits.sort_by_key(|(score, c)| (std::cmp::Reverse(*score), c.len(), *c));
    hits.first().map(|(_, c)| *c)
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
    /// which pane owns the keyboard; `go()` resolves against it.
    pub focus: Focus,
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
    OpenPage,
}

impl TermAction {
    fn op(self, target: Target) -> Op {
        match self {
            TermAction::Play => Op::Play(target),
            TermAction::Enqueue { front } => Op::Enqueue { target, front },
            TermAction::Star(on) => Op::Star { target, on },
            TermAction::Download => Op::Download(target),
            TermAction::OpenPage => Op::OpenPage(target),
        }
    }

    fn past(self) -> &'static str {
        match self {
            TermAction::Play => "played",
            TermAction::Enqueue { .. } => "queued",
            TermAction::Star(true) => "starred",
            TermAction::Star(false) => "unstarred",
            TermAction::Download => "downloading",
            TermAction::OpenPage => "opened",
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
    /// `go()`: resolved against `Env::focus` at run time, since what
    /// enter does depends on which pane owns the keyboard.
    Go,
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
            Node::Go => {
                self.pc += 1;
                // the host's own enter precedence: a focused browse
                // view claims it, then the bottom bar's field,
                // otherwise the selected search result.
                let target = match env.focus {
                    Focus::Workspace if env.browsing => GoTarget::BrowseRow,
                    Focus::BottomBar => GoTarget::BottomBarField,
                    _ => GoTarget::Result,
                };
                if target == GoTarget::Result && env.results.is_empty() {
                    return self.fail(RuntimeError::NothingSelected);
                }
                Step::Emit(Op::Go(target))
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
            // the zero-terms forms all act on the highlighted row, so
            // there has to be one.
            Op::Play(Target::Selection)
            | Op::Enqueue {
                target: Target::Selection,
                ..
            }
            | Op::Star {
                target: Target::Selection,
                ..
            }
            | Op::Download(Target::Selection)
            | Op::OpenPage(Target::Selection)
                if env.results.is_empty() && !env.browsing =>
            {
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
                Step::Emit(action.op(Target::Index(index)))
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

    /// The uniform rule (docs/shellsong.md R11): with terms, search for
    /// each and act on what it finds; without, act on the selection.
    /// One meaning, so the same code path serves both.
    fn lower_targeted(&mut self, call: &Call, action: TermAction) {
        match call.terms() {
            Some(terms) => self.lower_terms(call, terms, action),
            None => self.nodes.push(Node::Emit(action.op(Target::Selection))),
        }
    }

    fn lower_view(&mut self, call: &Call) {
        let querying =
            matches!(call.args.first(), Some(Arg::Word(w)) if VIEW_QUERY_WORDS.contains(w));
        if querying {
            self.lower_targeted(call, TermAction::OpenPage);
            return;
        }
        let Some(target) = call.keyword(Words::ViewTarget).and_then(view_target) else {
            return;
        };
        // the command listing is generated here, not read from the app,
        // so it lands in the scrollback exactly as `help()` does.
        if target == ViewTarget::Help {
            for line in help_lines() {
                self.nodes.push(Node::Note(line));
            }
            return;
        }
        self.nodes.push(Node::Emit(Op::View(target)));
    }

    fn lower_plain(&mut self, call: &Call) {
        let name = call.command.name;
        match name {
            "play" => self.lower_targeted(call, TermAction::Play),
            "queue" | "add_to_queue" => {
                let front = call.keyword(Words::Position) == Some("front");
                self.lower_targeted(call, TermAction::Enqueue { front });
            }
            "star" | "unstar" => {
                self.lower_targeted(call, TermAction::Star(name == "star"));
            }
            "download" => self.lower_targeted(call, TermAction::Download),
            "view" => self.lower_view(call),
            "go" => self.nodes.push(Node::Go),
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
        Node::Go => Some("  do what enter would do here".to_string()),
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
        let accepted = set.accepts();
        self.args.iter().find_map(|a| match a {
            Arg::Word(w) if accepted.contains(w) => Some(*w),
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
            "eq" => Op::SetEqPreset(self.keyword(Words::EqPreset).and_then(eq::index_of)?),
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
            "up" => Op::MoveSelection(-1),
            "down" => Op::MoveSelection(1),
            "top" => Op::SelectTop,
            "bottom" => Op::SelectBottom,
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
                if !widenable(command) {
                    let why = if command.prefs.is_empty() {
                        format!("{name} does not search, so kind= means nothing here")
                    } else {
                        format!("{name} has its own preference order and does not take kind=")
                    };
                    return Err(err(input, off, piece, why));
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
    if command.name == "view" {
        check_view(input, &args, &positional)?;
    }
    Ok(Call {
        command,
        args,
        kind,
    })
}

/// `view` is the one command whose two parameters constrain each other:
/// `q` is the search form and needs terms, every other target is a
/// screen and takes none. Checked here rather than in `bind`, which
/// only ever sees one parameter at a time.
fn check_view(input: &str, args: &[Arg], positional: &[(usize, &str)]) -> Result<(), ParseError> {
    let querying = matches!(args.first(), Some(Arg::Word(w)) if VIEW_QUERY_WORDS.contains(w));
    let terms = args.iter().any(|a| matches!(a, Arg::Terms(_)));
    let at = |i: usize| positional.get(i).map(|(o, _)| *o).unwrap_or(0);
    match (querying, terms) {
        (true, false) => Err(err(
            input,
            at(0),
            "q",
            "view(q, ...) needs something to search for",
        )),
        (false, true) => Err(err(
            input,
            at(1),
            positional.get(1).map(|(_, t)| *t).unwrap_or(""),
            "view(<target>) takes no terms; write view(q, terms) to search",
        )),
        _ => Ok(()),
    }
}

/// The "you left out a required argument" message. Carries the
/// parameter's own hint when the type alone would not tell the user
/// which call they actually wanted.
fn missing(command: &Command, param: &Param) -> String {
    let base = format!(
        "{}: missing {} ({})",
        command.name,
        param.name,
        param.ty.describe()
    );
    if param.hint.is_empty() {
        base
    } else {
        format!("{base}; {}", param.hint)
    }
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
                    missing(command, param),
                ));
            }
            continue;
        };
        match coerce(input, command, param, *off, text) {
            Ok(arg) => {
                args.push(arg);
                next += 1;
            }
            Err(mut e) => {
                if param.required || strict {
                    // the hint explains which call they probably wanted,
                    // which matters just as much for a value that does
                    // not typecheck as for one left out entirely.
                    if !param.hint.is_empty() {
                        e.message = format!("{}; {}", e.message, param.hint);
                    }
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
        // every free-text type parses identically; they differ only in
        // what they complete from (see `Ty::completes`).
        Ty::Query | Ty::NewName | Ty::SavedName | Ty::CommandName => {
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
        // every accepted spelling parses, but an unrecognised one is
        // measured against the *canonical* list so the suggestion is
        // something worth typing.
        Ty::Words(set) => match set.accepts().into_iter().find(|o| *o == text) {
            Some(word) => Ok(Arg::Word(word)),
            None => Err(bad(match closest(text, &set.options()) {
                Some(near) => format!(
                    "{}: unknown {} '{text}'; did you mean '{near}'?",
                    command.name, param.name
                ),
                None => format!(
                    "{}: {} is one of: {}",
                    command.name,
                    param.name,
                    set.options().join(", ")
                ),
            })),
        },
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
/// Completion runs on every keystroke, so the candidate list is capped
/// rather than however many a wide query matches.
const MAX_CANDIDATES: usize = 50;

/// What completion may ask the host about the world, behind a trait so
/// the completer itself stays pure: tests supply a fake, the host
/// supplies the real corpus.
///
/// Both methods are called per keystroke and must therefore be local
/// only. `corpus` must never trigger a remote search: typing would
/// spam the server.
pub trait Candidates {
    /// Primary names (song title, album name, artist name) of local
    /// corpus matches for `token`, best first and deduped. The host
    /// passes `Corpus::search`, which is local by construction.
    fn corpus(&self, token: &str) -> Vec<String>;

    /// Names of the saved queues `load_queue` could take.
    fn saved_queues(&self) -> Vec<String> {
        Vec::new()
    }
}

/// A provider that knows nothing: for the tests that do not care and
/// for the overlay's own render tests. The real host supplies
/// `shellsong_host::CorpusCandidates`.
#[cfg_attr(not(test), allow(dead_code))]
pub struct NoCandidates;

impl Candidates for NoCandidates {
    fn corpus(&self, _token: &str) -> Vec<String> {
        Vec::new()
    }
}

/// The live suggestion for the current input, recomputed on every
/// keystroke; the overlay renders straight off this.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Suggestion {
    /// Insertable text, drawn greyed out after the cursor. Always a
    /// pure extension of what was typed, never a replacement.
    pub ghost: String,
    /// Not insertable: the expected shape of a value. Rendered away
    /// from the input line precisely because it cannot be accepted.
    pub hint: Option<&'static str>,
    /// `name(a, b)` of the enclosing call plus the active parameter
    /// index, for the context line.
    pub signature: Option<(String, usize)>,
    /// `ghost` is the tail of a whole past command line rather than a
    /// token completion; only precedence and ranking care.
    pub from_history: bool,
}

/// An open tab cycle. Every item is a prefix-extension of `typed`, so
/// no amount of cycling can overwrite what was typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cycle {
    pub items: Vec<String>,
    pub at: usize,
    typed: String,
}

/// Console state: the input line being edited, the scrollback, the
/// persisted history and the completion state. Rendering lives in
/// `ui::shellsong`.
#[derive(Debug, Clone, Default)]
pub struct Console {
    pub input: String,
    /// byte index into `input`.
    pub cursor: usize,
    pub lines: Vec<Line>,
    pub error: Option<String>,
    pub suggestion: Suggestion,
    cycle: Option<Cycle>,
    history: Vec<String>,
    ranks: Ranker,
    /// position in `history` while recalling; `None` while editing a
    /// fresh line.
    recall: Option<usize>,
    /// the fresh line, parked while recalling through history.
    draft: String,
}

impl Console {
    /// Loads the persisted history and ranking; the only I/O here.
    pub fn new() -> Console {
        Console {
            history: load_history(),
            ranks: Ranker::load(),
            ..Console::default()
        }
    }

    /// No-I/O constructor, for tests.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn empty() -> Console {
        Console::default()
    }

    /// The open tab cycle, for the overlay to draw with its position
    /// indicator.
    pub fn cycle(&self) -> Option<&Cycle> {
        self.cycle.as_ref()
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

    pub fn handle_key(&mut self, key: KeyEvent, cand: &dyn Candidates) -> ConsoleAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => {
                // an open cycle swallows the first esc: cancelling the
                // menu is what you meant, not closing the console.
                if self.cycle.take().is_some() {
                    self.refresh(cand);
                } else {
                    return ConsoleAction::Close;
                }
            }
            KeyCode::Enter => return self.submit(),
            // ctrl+f accepts wherever the cursor is, which is what
            // makes it the unambiguous accept key.
            KeyCode::Char('f') if ctrl => self.accept(cand),
            KeyCode::Char(_) if ctrl => {}
            KeyCode::Char(c) => {
                self.error = None;
                self.cycle = None;
                edit_insert(&mut self.input, &mut self.cursor, c);
                self.refresh(cand);
            }
            KeyCode::Backspace => {
                self.error = None;
                self.cycle = None;
                edit_backspace(&mut self.input, &mut self.cursor);
                self.refresh(cand);
            }
            KeyCode::Delete => {
                self.cycle = None;
                if let Some(next) = next_boundary(&self.input, self.cursor) {
                    self.input.replace_range(self.cursor..next, "");
                }
                self.refresh(cand);
            }
            KeyCode::Left => {
                if let Some(prev) = prev_boundary(&self.input, self.cursor) {
                    self.cursor = prev;
                }
                self.refresh(cand);
            }
            // right accepts only at the end of the line, so it keeps
            // working as plain cursor movement mid-line.
            KeyCode::Right => {
                if self.cursor == self.input.len() && !self.suggestion.ghost.is_empty() {
                    self.accept(cand);
                } else {
                    if let Some(next) = next_boundary(&self.input, self.cursor) {
                        self.cursor = next;
                    }
                    self.refresh(cand);
                }
            }
            KeyCode::Home => {
                self.cursor = 0;
                self.refresh(cand);
            }
            KeyCode::End => {
                self.cursor = self.input.len();
                self.refresh(cand);
            }
            KeyCode::Up => {
                self.recall_back();
                self.refresh(cand);
            }
            KeyCode::Down => {
                self.recall_forward();
                self.refresh(cand);
            }
            KeyCode::Tab => self.spin(cand, 1),
            KeyCode::BackTab => self.spin(cand, -1),
            _ => {}
        }
        ConsoleAction::None
    }

    /// Bracketed paste; goes in literally, with no auto-pairing (a
    /// pasted `[` already has its own `]`).
    pub fn paste(&mut self, text: &str, cand: &dyn Candidates) {
        let text: String = text.chars().filter(|c| *c != '\n' && *c != '\r').collect();
        self.cycle = None;
        self.input.insert_str(self.cursor, &text);
        self.cursor += text.len();
        self.refresh(cand);
    }

    fn submit(&mut self) -> ConsoleAction {
        self.cycle = None;
        self.suggestion = Suggestion::default();
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
        // a line that was actually run is the strongest signal there
        // is, so it counts even when it repeats the previous one (which
        // the history list itself collapses).
        self.ranks.bump(&line);
        self.ranks.save();
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

    /// Recomputes the ghost, the hint and the context line for wherever
    /// the cursor now is. Cheap enough to run per keystroke: at worst
    /// one local corpus search, capped at `MAX_CANDIDATES`.
    fn refresh(&mut self, cand: &dyn Candidates) {
        let here = site(&self.input, self.cursor);
        let mut next = Suggestion {
            signature: here.signature.clone(),
            ..Suggestion::default()
        };
        // a value parameter gets a hint and nothing acceptable: there
        // is no sane candidate list for a number, and guessing one
        // (`volume(5` into `50%`) is worse than saying nothing.
        if let Completes::Hint(text) = here.completes {
            next.hint = Some(text);
            self.suggestion = next;
            return;
        }
        // a whole past command line wins over a token completion, but
        // only with the cursor at the end, where the rest of the line
        // is genuinely what comes next.
        if self.cursor == self.input.len() && !self.input.is_empty() {
            if let Some(rest) = self.history_extension() {
                next.ghost = rest;
                next.from_history = true;
                self.suggestion = next;
                return;
            }
        }
        next.ghost = self
            .candidates_at(&here, cand)
            .first()
            .map(|c| remainder(&here.token, c))
            .unwrap_or_default();
        self.suggestion = next;
    }

    /// Ranked, prefix-only candidates for the token under the cursor.
    fn candidates_at(&self, here: &Site, cand: &dyn Candidates) -> Vec<String> {
        let found: Vec<String> = match here.completes {
            Completes::Hint(_) => return Vec::new(),
            Completes::Words(set) => set.options().iter().map(|s| s.to_string()).collect(),
            Completes::CommandNames => every_name().iter().map(|s| s.to_string()).collect(),
            // fuzzy discovery, so a subsequence or typo match surfaces
            // things a prefix scan would miss; the prefix filter below
            // then drops whatever could not be inserted without
            // overwriting what was typed.
            Completes::CorpusTerms => cand.corpus(&here.token),
            Completes::SavedQueues => cand.saved_queues(),
        };
        let mut seen: Vec<String> = Vec::new();
        for c in found {
            if extend(&here.token, &c).is_some() && !seen.contains(&c) {
                seen.push(c);
                if seen.len() >= MAX_CANDIDATES {
                    break;
                }
            }
        }
        self.ranks.order(&mut seen);
        seen
    }

    /// The best past command line the whole input is a prefix of, as
    /// zsh-autosuggestions does it.
    fn history_extension(&self) -> Option<String> {
        let mut hits: Vec<String> = Vec::new();
        // newest first, so recency is the order anything unranked
        // falls back to.
        for past in self.history.iter().rev() {
            if extend(&self.input, past).is_some() && !hits.contains(past) {
                hits.push(past.clone());
            }
        }
        self.ranks.order(&mut hits);
        hits.first().map(|h| remainder(&self.input, h))
    }

    /// Inserts the current ghost. Only ever appends the candidate's
    /// remaining characters, so the typed prefix survives byte for
    /// byte, casing included.
    fn accept(&mut self, cand: &dyn Candidates) {
        if self.suggestion.ghost.is_empty() {
            return;
        }
        let ghost = std::mem::take(&mut self.suggestion.ghost);
        let from_history = self.suggestion.from_history;
        let before = site(&self.input, self.cursor);
        self.cycle = None;
        self.input.insert_str(self.cursor, &ghost);
        self.cursor += ghost.len();
        if from_history {
            let line = self.input.clone();
            self.ranks.bump(&line);
        } else {
            self.ranks.bump(&format!("{}{ghost}", before.token));
            // every command needs parens, so a completed command name
            // gets them and lands the cursor inside, reusing the same
            // auto-pairing an explicitly typed '(' goes through.
            if before.completes == Completes::CommandNames
                && !self.input[self.cursor..].starts_with('(')
            {
                edit_insert(&mut self.input, &mut self.cursor, '(');
            }
        }
        self.refresh(cand);
    }

    /// Tab (`dir` 1) and shift+tab (`dir` -1). Always cycles token
    /// candidates, never the history line: the two sources are
    /// reachable by different keys on purpose.
    fn spin(&mut self, cand: &dyn Candidates, dir: i32) {
        if self.cycle.is_none() {
            let here = site(&self.input, self.cursor);
            if !here.completes.insertable() {
                return;
            }
            let items = self.candidates_at(&here, cand);
            match items.len() {
                0 => return,
                // nothing to choose between, so tab just takes it.
                1 => {
                    self.suggestion.ghost = remainder(&here.token, &items[0]);
                    self.suggestion.from_history = false;
                    self.accept(cand);
                    return;
                }
                _ => {
                    self.cycle = Some(Cycle {
                        items,
                        at: 0,
                        typed: here.token,
                    });
                }
            }
        } else if let Some(cycle) = &mut self.cycle {
            let n = cycle.items.len() as i32;
            cycle.at = (((cycle.at as i32 + dir) % n + n) % n) as usize;
        }
        if let Some(cycle) = &self.cycle {
            self.suggestion.ghost = remainder(&cycle.typed, &cycle.items[cycle.at]);
            self.suggestion.from_history = false;
            self.suggestion.hint = None;
        }
    }
}

// ----------------------------------------------------------- completion

/// Where the cursor is and what the parameter there expects. Derived
/// entirely from `COMMANDS`, so a completion that appears always
/// parses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Site {
    /// byte offset the token starts at.
    pub start: usize,
    pub token: String,
    pub completes: Completes,
    /// `name(a, b)` of the enclosing call plus the active parameter
    /// index; `None` at statement level.
    pub signature: Option<(String, usize)>,
}

/// Classifies the cursor position: which call it is inside, which
/// parameter of it, and therefore what may be completed there.
pub fn site(input: &str, cursor: usize) -> Site {
    let (completes, signature) = match enclosing(input, cursor) {
        None => (Completes::CommandNames, None),
        Some((name, arg)) => match lookup(name) {
            Some((command, _)) => match command.params.get(arg) {
                Some(param) => (param.ty.completes(), Some((command.signature(), arg))),
                // past the last parameter: nothing left to offer, but
                // still show the signature so the arity is visible.
                None => (
                    Completes::Hint("no more arguments"),
                    Some((command.signature(), arg)),
                ),
            },
            // the enclosing name is not (yet) a command, so whatever is
            // being typed inside it is most likely a nested call.
            None => (Completes::CommandNames, None),
        },
    };
    let (start, token) = token_bounds(input, cursor);
    Site {
        start,
        token,
        completes,
        signature,
    }
}

/// `(start, text)` of the argument slice the cursor sits in: back to
/// the nearest unquoted `(`, `[`, `,` or `;`, then forward past the
/// whitespace. Slice-shaped rather than word-shaped because a term and
/// several keywords ("bass boost", "4 star crest") contain spaces, and
/// a word-shaped token could not complete them. This is also why
/// `queue([a, fl` completes `fl` and not the whole list.
fn token_bounds(input: &str, cursor: usize) -> (usize, String) {
    let upto = &input[..cursor.min(input.len())];
    let mut start = 0usize;
    let mut quoted = false;
    for (i, c) in upto.char_indices() {
        match c {
            '"' => quoted = !quoted,
            '(' | '[' | ',' | ';' if !quoted => start = i + c.len_utf8(),
            _ => {}
        }
    }
    let slice = &upto[start..];
    let lead = slice.len() - slice.trim_start().len();
    (start + lead, slice.trim_start().to_string())
}

/// One open bracket's worth of context while scanning.
enum Frame {
    /// a call's `(`, with the name before it and how many top-level
    /// commas have been seen inside it.
    Call { name: (usize, usize), commas: usize },
    /// a list's `[`; commas inside it separate items, not arguments,
    /// which is why it needs its own frame.
    List,
}

/// `(command name, argument index)` of the innermost call `at` sits
/// inside, or `None` at statement level. A stack rather than a depth
/// counter so `dry(queue([fl` reports `queue`, not `dry`.
fn enclosing(input: &str, at: usize) -> Option<(&str, usize)> {
    let before = &input[..at.min(input.len())];
    let mut stack: Vec<Frame> = Vec::new();
    let mut quoted = false;
    for (i, c) in before.char_indices() {
        match c {
            '"' => quoted = !quoted,
            _ if quoted => {}
            '(' => {
                let end = i;
                let start = before[..end]
                    .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .map(|j| j + 1)
                    .unwrap_or(0);
                stack.push(Frame::Call {
                    name: (start, end),
                    commas: 0,
                });
            }
            '[' => stack.push(Frame::List),
            ')' | ']' => {
                stack.pop();
            }
            ',' => {
                if let Some(Frame::Call { commas, .. }) = stack.last_mut() {
                    *commas += 1;
                }
            }
            _ => {}
        }
    }
    stack.iter().rev().find_map(|f| match f {
        Frame::Call { name, commas } => Some((&before[name.0..name.1], *commas)),
        Frame::List => None,
    })
}

/// `Some(remainder)` when `candidate` extends `token` case-
/// insensitively. The remainder is all that ever gets inserted, so the
/// typed prefix survives byte for byte including its casing (`fl` plus
/// `atline`, never `Flatline`); search is case-insensitive, so the
/// resulting term still resolves. A fuzzy hit that is not a prefix
/// extension returns `None` and is dropped: inserting it would replace
/// characters the user typed, which completion must never do.
fn extend(token: &str, candidate: &str) -> Option<String> {
    let keep = token.chars().count();
    let head: String = candidate.chars().take(keep).collect();
    if head.chars().count() < keep || head.to_lowercase() != token.to_lowercase() {
        return None;
    }
    let rest: String = candidate.chars().skip(keep).collect();
    (!rest.is_empty()).then_some(rest)
}

fn remainder(token: &str, candidate: &str) -> String {
    extend(token, candidate).unwrap_or_default()
}

// -------------------------------------------------------------- ranking

/// How many ranked entries to keep; past this the least recently used
/// go, since an entry nobody has touched in hundreds of commands is
/// not what anyone is reaching for.
const MAX_RANKS: usize = 500;

#[derive(Debug, Clone, Copy, Default)]
struct Stat {
    uses: u32,
    last: u64,
}

/// Recency-weighted frequency over the lines actually run and the
/// completions actually accepted, so what gets used floats to the top.
/// Frequency leads and recency breaks its ties; anything unseen keeps
/// whatever order it arrived in (relevance for corpus hits, alphabetical
/// for names), which is what keeps the result deterministic.
#[derive(Debug, Clone, Default)]
pub struct Ranker {
    entries: HashMap<String, Stat>,
    /// Stands in for a clock: the newest use simply has the highest
    /// tick, which is all recency needs to be, and it survives a
    /// restart without depending on the system time.
    tick: u64,
}

impl Ranker {
    fn bump(&mut self, what: &str) {
        if what.is_empty() {
            return;
        }
        self.tick += 1;
        let tick = self.tick;
        let entry = self.entries.entry(what.to_string()).or_default();
        entry.uses = entry.uses.saturating_add(1);
        entry.last = tick;
        if self.entries.len() > MAX_RANKS {
            self.prune();
        }
    }

    fn stat(&self, what: &str) -> Stat {
        self.entries.get(what).copied().unwrap_or_default()
    }

    /// Stable, which is the whole trick: unseen entries come out in
    /// the order they went in rather than reshuffled.
    fn order(&self, items: &mut [String]) {
        items.sort_by_key(|item| {
            let stat = self.stat(item);
            (std::cmp::Reverse(stat.uses), std::cmp::Reverse(stat.last))
        });
    }

    fn prune(&mut self) {
        let mut by_age: Vec<(String, u64)> = self
            .entries
            .iter()
            .map(|(k, v)| (k.clone(), v.last))
            .collect();
        by_age.sort_by_key(|(_, last)| *last);
        for (key, _) in by_age.iter().take(self.entries.len() - MAX_RANKS * 3 / 4) {
            self.entries.remove(key);
        }
    }

    /// Its own file beside the plain history list: this is a scored
    /// map, that is a line list, and nothing else parses either.
    fn load() -> Ranker {
        let Some(path) = state_path("shellsong_ranks") else {
            return Ranker::default();
        };
        let Ok(raw) = fs::read_to_string(path) else {
            return Ranker::default();
        };
        let mut ranker = Ranker::default();
        for line in raw.lines() {
            // `uses \t last \t name`; the name goes last because it is
            // the only field that can contain anything.
            let mut parts = line.splitn(3, '\t');
            let (Some(uses), Some(last), Some(name)) = (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            let (Ok(uses), Ok(last)) = (uses.parse::<u32>(), last.parse::<u64>()) else {
                continue;
            };
            if name.is_empty() {
                continue;
            }
            ranker.tick = ranker.tick.max(last);
            ranker.entries.insert(name.to_string(), Stat { uses, last });
        }
        ranker
    }

    /// Best-effort, same leniency as every other state file here.
    fn save(&self) {
        let Some(path) = state_path("shellsong_ranks") else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let body: String = self
            .entries
            .iter()
            .map(|(name, s)| format!("{}\t{}\t{}", s.uses, s.last, name))
            .collect::<Vec<_>>()
            .join("\n");
        let _ = fs::write(path, body);
    }
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
fn state_path(file: &str) -> Option<PathBuf> {
    let dir = dirs::state_dir().or_else(dirs::data_dir)?;
    Some(dir.join("wim").join(file))
}

fn load_history() -> Vec<String> {
    let Some(path) = state_path("shellsong_history") else {
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
    let Some(path) = state_path("shellsong_history") else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(path, history.join("\n"));
}

#[cfg(test)]
mod tests {
    use super::*;

    const SETTLED: Env = Env {
        now: Duration::ZERO,
        query: String::new(),
        results: Vec::new(),
        search_settled: true,
        queue_len: 10,
        browsing: true,
        focus: Focus::Sidebar,
    };

    /// Runs a program to completion against a fake world, standing in
    /// for the host: a search is "applied" by mirroring the query back
    /// into `Env`, and results are whatever the test says they are.
    fn drive(input: &str, results: Vec<Kind>) -> (Vec<Op>, Vec<String>) {
        let mut program = Program::parse(input).expect("should parse");
        let mut env = Env {
            results,
            ..SETTLED.clone()
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

    fn queued(index: usize) -> Op {
        Op::Enqueue {
            target: Target::Index(index),
            front: false,
        }
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
                Program::parse(&input).unwrap_or_else(|e| panic!("{input} did not parse: {e}"));
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
            // a real target, not the `q` literal, which would need a
            // term alongside it (see `check_view`).
            Ty::Words(Words::ViewTarget) => "queue".into(),
            Ty::Terms => "crest".into(),
            Ty::Query | Ty::NewName | Ty::SavedName => "crest".into(),
            Ty::CommandName => "pause".into(),
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

    /// `view(help)` and `help()` are the same listing; `view` means
    /// "put this on screen" and a scrollback listing satisfies that.
    #[test]
    fn view_help_is_the_same_listing_help_prints() {
        assert_eq!(notes("view(help)"), notes("help()"));
        assert_eq!(notes("view(help)").len(), COMMANDS.len());
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

    /// `eq` only ever applies a preset now; the menu is `view(eq)`,
    /// which is what removes the old arity overload.
    #[test]
    fn eq_applies_a_preset_and_the_menu_moved_to_view() {
        let rock = eq::index_of("rock").unwrap();
        assert_eq!(ops("eq(rock)"), vec![Op::SetEqPreset(rock)]);
        assert_eq!(ops("eq(flat)"), vec![Op::SetEqPreset(0)]);
        assert_eq!(ops("view(eq)"), vec![Op::View(ViewTarget::Eq)]);
        assert_eq!(ops("view(equalizer)"), vec![Op::View(ViewTarget::Eq)]);
        let e = error("eq()");
        assert!(e.message.contains("missing preset"), "{}", e.message);
        // nothing near "disco", so it lists what there is rather than
        // inventing a suggestion.
        let e = error("eq(disco)");
        assert!(e.message.contains("bass boost"), "{}", e.message);
        assert!(error("eq(rok)").message.contains("did you mean 'rock'"));
    }

    // ------------------------------------------------------------ view

    /// Every spelling in the table resolves, including the plural and
    /// singular forms the maintainer asked for.
    #[test]
    fn every_view_spelling_resolves_to_its_target() {
        for v in VIEW_NAMES {
            if v.target == ViewTarget::Help {
                continue; // a listing, asserted separately
            }
            assert_eq!(
                ops(&format!("view({})", v.word)),
                vec![Op::View(v.target)],
                "view({})",
                v.word
            );
        }
        for word in ["suggest", "suggestion", "suggestions"] {
            assert_eq!(
                ops(&format!("view({word})")),
                vec![Op::View(ViewTarget::Suggestions)],
                "{word}"
            );
        }
    }

    /// `show` is an alias of `view`, not a second verb with its own
    /// meaning (R10).
    #[test]
    fn show_and_ls_are_true_aliases_of_view() {
        for name in ["view", "show", "ls"] {
            assert_eq!(
                ops(&format!("{name}(queue)")),
                vec![Op::View(ViewTarget::Queue)],
                "{name}"
            );
        }
        assert!(COMMANDS.iter().all(|c| c.name != "show"));
    }

    /// The verbs that used to exist for one screen each are gone; the
    /// did-you-mean points at `view`.
    #[test]
    fn the_old_screen_opening_verbs_are_gone() {
        for name in ["settings", "keybinds", "lyrics", "starred", "suggest"] {
            let e = error(&format!("{name}()"));
            assert!(
                e.message.contains("unknown command"),
                "{name}() still parses: {}",
                e.message
            );
        }
        assert_eq!(ops("view(settings)"), vec![Op::View(ViewTarget::Settings)]);
        assert_eq!(ops("view(keybinds)"), vec![Op::View(ViewTarget::Keybinds)]);
        assert_eq!(ops("view(lyrics)"), vec![Op::View(ViewTarget::Lyrics)]);
        assert_eq!(ops("view(starred)"), vec![Op::View(ViewTarget::Starred)]);
    }

    #[test]
    fn an_unknown_view_target_suggests_a_real_one() {
        let e = error("view(suggst)");
        assert!(
            e.message.contains("did you mean 'suggestions'"),
            "{}",
            e.message
        );
        let e = error("view(setings)");
        assert!(
            e.message.contains("did you mean 'settings'"),
            "{}",
            e.message
        );
    }

    #[test]
    fn view_with_no_argument_lists_the_targets() {
        let e = error("view()");
        assert!(e.message.contains("missing target"), "{}", e.message);
        assert!(e.message.contains("queue"), "{}", e.message);
        assert!(e.message.contains("settings"), "{}", e.message);
        // and it points at the other form too
        assert!(e.message.contains("view(q, terms)"), "{}", e.message);
    }

    /// `view(q, bladee)` goes through the same search path `play` and
    /// `queue` use, and prefers the kinds that have pages.
    #[test]
    fn view_q_searches_and_opens_the_best_page() {
        assert_eq!(terms_of("view(q, bladee)"), ["bladee"]);
        assert_eq!(
            resolve_with("view(q, bladee)", vec![Kind::Song, Kind::Artist]),
            vec![Op::OpenPage(Target::Index(1))],
            "an artist outranks a song for view"
        );
        assert_eq!(
            resolve_with("view(q, crest)", vec![Kind::Song, Kind::Album]),
            vec![Op::OpenPage(Target::Index(1))],
            "an album outranks a song for view"
        );
        // a song still works: it opens the page it belongs to rather
        // than playing, because the verb is view.
        assert_eq!(
            resolve_with("view(q, hex)", vec![Kind::Song]),
            vec![Op::OpenPage(Target::Index(0))]
        );
        assert_eq!(ops("view(query, bladee)"), ops("view(q, bladee)"));
    }

    #[test]
    fn views_two_forms_cannot_be_mixed_up() {
        let e = error("view(q)");
        assert!(
            e.message.contains("needs something to search"),
            "{}",
            e.message
        );
        let e = error("view(settings, bladee)");
        assert!(e.message.contains("takes no terms"), "{}", e.message);
        assert!(e.message.contains("view(q, terms)"), "{}", e.message);
    }

    /// `view` prefers artists, which is a different order rather than a
    /// narrower one, so the song-rooted `kind=` ladder does not apply.
    #[test]
    fn view_does_not_take_the_kind_ladder() {
        assert!(Program::parse("view_aa(q, crest)").is_err());
        let e = error("view(q, crest, kind=album)");
        assert!(e.message.contains("does not take kind="), "{}", e.message);
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

    /// Deleted on purpose: an artist expands to a whole discography,
    /// which is never a queue anyone wanted.
    #[test]
    fn there_is_no_artist_rung_on_the_ladder() {
        assert!(Program::parse("queue_ar([crest])").is_err());
        assert!(error("queue([x], kind=artist)")
            .message
            .contains("kind= expects"));
        assert_eq!(
            KIND_SUFFIXES.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
            vec!["_aa", "_any"]
        );
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

    // --------------------------------------------- the selection rule

    /// R11: omitting the terms means "the current selection", the same
    /// way for every command that takes them. One meaning, so one code
    /// path and one op shape.
    #[test]
    fn omitting_the_terms_means_the_selection_everywhere() {
        assert_eq!(ops("play()"), vec![Op::Play(Target::Selection)]);
        assert_eq!(
            ops("queue()"),
            vec![Op::Enqueue {
                target: Target::Selection,
                front: false
            }]
        );
        assert_eq!(
            ops("add_to_queue(front)"),
            vec![Op::Enqueue {
                target: Target::Selection,
                front: true
            }]
        );
        assert_eq!(
            ops("star()"),
            vec![Op::Star {
                target: Target::Selection,
                on: true
            }]
        );
        assert_eq!(
            ops("unstar()"),
            vec![Op::Star {
                target: Target::Selection,
                on: false
            }]
        );
        assert_eq!(ops("download()"), vec![Op::Download(Target::Selection)]);
    }

    /// Driven off the table: every command with a `terms` parameter has
    /// to accept being called without it, or the rule is not uniform.
    #[test]
    fn every_terms_taking_command_accepts_no_terms_at_all() {
        for c in COMMANDS {
            let Some(terms) = c.params.iter().find(|p| p.ty == Ty::Terms) else {
                continue;
            };
            // the one documented exception: view's terms belong to its
            // `q` form rather than being an operand, and `q` means
            // "search for this", which has no selection reading.
            if c.name == "view" {
                continue;
            }
            assert!(
                !terms.required,
                "{}'s terms are required, so it cannot mean the selection",
                c.name
            );
            // build the call with every *other* required argument
            let args: Vec<String> = c
                .params
                .iter()
                .filter(|p| p.required && p.ty != Ty::Terms)
                .map(|p| sample_arg(p.ty))
                .collect();
            let call = format!("{}({})", c.name, args.join(", "));
            assert!(
                Program::parse(&call).is_ok(),
                "{call} should mean 'act on the selection'"
            );
        }
    }

    #[test]
    fn a_selection_form_with_nothing_selected_fails_rather_than_emitting() {
        let empty = Env {
            browsing: false,
            ..SETTLED.clone()
        };
        for input in ["play()", "queue()", "star()", "download()"] {
            assert_eq!(
                Program::parse(input).unwrap().step(&empty),
                Step::Failed(RuntimeError::NothingSelected),
                "{input}"
            );
        }
    }

    // --------------------------------------------------- arity + types

    /// `add_to_queue(back, x)` was just another spelling of `queue(x)`,
    /// so the position is required and the error points at the command
    /// that actually appends.
    #[test]
    fn add_to_queue_requires_its_position() {
        // whether the position is wrong or absent, the message names it
        // and points at the command that actually appends.
        for bad in ["add_to_queue(flatline)", "add_to_queue()"] {
            let e = error(bad);
            assert!(e.message.contains("position"), "{bad}: {}", e.message);
            assert!(e.message.contains("queue(...)"), "{bad}: {}", e.message);
        }
        assert_eq!(
            Program::parse("add_to_queue(back, flatline)")
                .unwrap()
                .nodes,
            Program::parse("queue(flatline)").unwrap().nodes,
            "back is still the same thing queue does"
        );
        assert_ne!(
            Program::parse("add_to_queue(front, flatline)")
                .unwrap()
                .nodes,
            Program::parse("queue(flatline)").unwrap().nodes
        );
    }

    #[test]
    fn a_missing_required_argument_names_the_command_and_the_type() {
        let e = error("moveto()");
        assert!(e.message.contains("30s"), "{}", e.message);
        let e = error("save_queue()");
        assert!(e.message.contains("save_queue"), "{}", e.message);
    }

    #[test]
    fn too_many_arguments_report_the_signature() {
        let e = error("skip(2)");
        assert!(e.message.contains("skip()"), "{}", e.message);
    }

    #[test]
    fn an_unknown_position_keyword_names_the_set_it_expected() {
        let e = error("add_to_queue(sideways, flatline)");
        assert!(e.message.contains("front"), "{}", e.message);
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
        assert_eq!(resolve_with("queue(crest)", both.clone()), vec![queued(1)]);
        assert_eq!(
            resolve_with("queue_aa(crest)", both),
            vec![queued(1)],
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
            resolve_with("queue_aa(crest)", albums_only),
            vec![queued(0)]
        );
        assert!(
            resolve_with("queue_aa(crest)", vec![Kind::Artist]).is_empty(),
            "_aa widens to albums, and stops there"
        );
        assert_eq!(
            resolve_with("queue_any(crest)", vec![Kind::Artist]),
            vec![queued(0)],
            "_any is the only rung that reaches an artist"
        );
    }

    /// R7's deliberate asymmetry: play is wider than queue.
    #[test]
    fn play_accepts_an_album_where_queue_would_not() {
        assert_eq!(
            resolve_with("play(crest)", vec![Kind::Album]),
            vec![Op::Play(Target::Index(0))]
        );
        assert_eq!(
            resolve_with("play(crest)", vec![Kind::Song, Kind::Album]),
            vec![Op::Play(Target::Index(0))]
        );
        assert!(resolve_with("play(crest)", vec![Kind::Artist]).is_empty());
    }

    #[test]
    fn play_with_several_terms_plays_the_first_and_queues_the_rest() {
        assert_eq!(
            resolve_with("play([a, b, c])", vec![Kind::Song]),
            vec![Op::Play(Target::Index(0)), queued(0), queued(0)]
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
                query: query.clone(),
                results: if term == 2 {
                    vec![Kind::Song]
                } else {
                    vec![Kind::Artist]
                },
                ..SETTLED.clone()
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
        assert_eq!(program.step(&env), Step::Emit(queued(0)));
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
        assert_eq!(program.step(&env), Step::Emit(Op::Play(Target::Index(0))));
    }

    // -------------------------------------------------------- go/focus

    /// `go()` means "what enter does", and what enter does depends on
    /// which pane owns the keyboard, so it resolves against the focus
    /// rather than making the host re-derive it.
    #[test]
    fn go_resolves_against_the_focus() {
        let base = Env {
            results: vec![Kind::Song],
            browsing: true,
            ..SETTLED.clone()
        };
        let cases = [
            (Focus::Sidebar, GoTarget::Result),
            (Focus::Workspace, GoTarget::BrowseRow),
            (Focus::BottomBar, GoTarget::BottomBarField),
        ];
        for (focus, want) in cases {
            let env = Env {
                focus,
                ..base.clone()
            };
            assert_eq!(
                Program::parse("go()").unwrap().step(&env),
                Step::Emit(Op::Go(want)),
                "{focus:?}"
            );
        }
    }

    /// Workspace focus with nothing open falls back to the sidebar's
    /// result, exactly as the host's own enter handling does.
    #[test]
    fn workspace_focus_with_no_browse_view_still_acts_on_the_result() {
        let env = Env {
            results: vec![Kind::Song],
            browsing: false,
            focus: Focus::Workspace,
            ..SETTLED.clone()
        };
        assert_eq!(
            Program::parse("open()").unwrap().step(&env),
            Step::Emit(Op::Go(GoTarget::Result))
        );
    }

    #[test]
    fn go_with_nothing_to_act_on_fails_rather_than_emitting() {
        let env = Env {
            browsing: false,
            ..SETTLED.clone()
        };
        assert_eq!(
            Program::parse("go()").unwrap().step(&env),
            Step::Failed(RuntimeError::NothingSelected)
        );
        // but the bottom bar always has a field to tweak
        let bar = Env {
            browsing: false,
            focus: Focus::BottomBar,
            ..SETTLED.clone()
        };
        assert_eq!(
            Program::parse("go()").unwrap().step(&bar),
            Step::Emit(Op::Go(GoTarget::BottomBarField))
        );
    }

    // -------------------------------------------------------- guards

    #[test]
    fn queue_positions_are_checked_against_the_real_queue() {
        let env = Env {
            queue_len: 3,
            ..SETTLED.clone()
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
            ..SETTLED.clone()
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

    /// An absolute level is capped at parse time; a delta is relative
    /// to mpv's current volume and the host clamps the result, which is
    /// why only one of the two is refused here.
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
        assert!(
            error("volume(+200%)").message.contains("outside 0-100"),
            "a nudge bigger than the whole range is nonsense too"
        );
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
    fn every_op_describes_itself_for_the_scrollback() {
        // R9: nothing is silent, so no op may describe itself as "".
        for c in COMMANDS {
            for op in ops(&sample_call(c)) {
                assert!(!op.describe().is_empty(), "{op:?} has no description");
            }
        }
    }

    /// The console is a centred overlay, so anything that puts a view
    /// on screen has to move it out of the way first.
    #[test]
    fn screen_opening_ops_close_the_console_and_listings_do_not() {
        assert!(Op::View(ViewTarget::Settings).closes_console());
        assert!(Op::View(ViewTarget::Suggestions).closes_console());
        assert!(Op::View(ViewTarget::Eq).closes_console());
        assert!(Op::OpenPage(Target::Selection).closes_console());
        assert!(Op::Quit.closes_console());
        assert!(!Op::View(ViewTarget::Queue).closes_console());
        assert!(!Op::View(ViewTarget::History).closes_console());
        assert!(!Op::TogglePause.closes_console());
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

    // ------------------------------------------------------- the site

    /// Every parameter type is covered by exactly one of the two
    /// classes: a candidate list, or a hint. A new `Ty` that declares
    /// neither cannot reach this assertion without failing it.
    #[test]
    fn every_parameter_type_offers_candidates_xor_a_hint() {
        let every: Vec<Ty> = vec![
            Ty::Terms,
            Ty::Query,
            Ty::NewName,
            Ty::SavedName,
            Ty::CommandName,
            Ty::Pos,
            Ty::Offset,
            Ty::Level,
            Ty::Count,
            Ty::Index,
            Ty::Indices,
            Ty::Speed,
            Ty::Duration,
            Ty::Call,
            Ty::Words(Words::Position),
            Ty::Words(Words::LoopMode),
            Ty::Words(Words::Switch),
            Ty::Words(Words::EqPreset),
            Ty::Words(Words::ViewTarget),
            Ty::Words(Words::FadeDir),
        ];
        for ty in &every {
            match ty.completes() {
                Completes::Hint(text) => {
                    assert!(!text.is_empty(), "{ty:?} declares an empty hint");
                    assert!(!ty.completes().insertable());
                }
                other => assert!(
                    other.insertable(),
                    "{ty:?} is neither insertable nor a hint"
                ),
            }
        }
        // and the table only ever uses types this test knows about, so
        // a new one cannot slip past unlisted.
        for c in COMMANDS {
            for p in c.params {
                assert!(
                    every.contains(&p.ty),
                    "{}'s {} has type {:?}, which this test does not cover",
                    c.name,
                    p.name,
                    p.ty
                );
            }
        }
    }

    #[test]
    fn a_keyword_set_only_ever_offers_something_that_parses() {
        for c in COMMANDS {
            for (i, p) in c.params.iter().enumerate() {
                let Completes::Words(set) = p.ty.completes() else {
                    continue;
                };
                for option in set.options() {
                    // every offered keyword has to coerce; the only
                    // exception is view's `q`, which needs a term too.
                    let extra = if c.name == "view" && option == "q" {
                        ", crest"
                    } else {
                        ""
                    };
                    let mut args: Vec<String> =
                        c.params.iter().take(i).map(|p| sample_arg(p.ty)).collect();
                    args.push(option.to_string());
                    let call = format!("{}({}{extra})", c.name, args.join(", "));
                    assert!(
                        Program::parse(&call).is_ok(),
                        "completion would offer {call}, which does not parse"
                    );
                }
            }
        }
    }

    #[test]
    fn the_site_knows_which_parameter_the_cursor_is_in() {
        let here = site("volume(", 7);
        assert_eq!(here.completes, Completes::Hint("0-100, or +N% / -N%"));
        assert_eq!(here.signature, Some(("volume(level)".to_string(), 0)));

        let here = site("queue(fl", 8);
        assert_eq!(here.completes, Completes::CorpusTerms);
        assert_eq!(here.token, "fl");

        let here = site("qu", 2);
        assert_eq!(here.completes, Completes::CommandNames);
        assert_eq!(here.signature, None);

        // a nested call completes command names, from the inner call
        let here = site("dry(sk", 6);
        assert_eq!(here.completes, Completes::CommandNames);
        assert_eq!(here.token, "sk");
    }

    /// The token is the argument slice, not the word, so an item with
    /// spaces can still be completed; and a list completes the item
    /// after the last comma rather than the whole list.
    #[test]
    fn the_token_is_the_argument_slice_after_the_last_separator() {
        assert_eq!(site("queue([a, fl", 12).token, "fl");
        assert_eq!(site("queue([4 star cre", 17).token, "4 star cre");
        assert_eq!(site("eq(bass bo", 10).token, "bass bo");
        assert_eq!(site("pause(); qu", 11).token, "qu");
        assert_eq!(site("add_to_queue(front, fl", 22).token, "fl");
    }

    /// A stack, not a depth counter: the innermost call is what the
    /// cursor is actually in.
    #[test]
    fn the_enclosing_call_is_the_innermost_one() {
        assert_eq!(enclosing("dry(queue([fl", 13), Some(("queue", 0)));
        assert_eq!(enclosing("repeat(2, skip(", 15), Some(("skip", 0)));
        // commas inside a list separate items, not arguments
        assert_eq!(enclosing("queue([a, b], ", 14), Some(("queue", 1)));
        assert_eq!(enclosing("queue([a, b", 11), Some(("queue", 0)));
        assert_eq!(enclosing("pause()", 7), None);
    }

    // --------------------------------------------------- completion

    /// A fake corpus. `corpus` deliberately returns fuzzy-style hits
    /// that are *not* all prefix extensions, which is what the prefix
    /// filter has to deal with.
    struct Fake {
        names: Vec<&'static str>,
        queues: Vec<&'static str>,
    }

    impl Fake {
        fn new(names: &[&'static str]) -> Fake {
            Fake {
                names: names.to_vec(),
                queues: Vec::new(),
            }
        }
    }

    impl Candidates for Fake {
        fn corpus(&self, _token: &str) -> Vec<String> {
            self.names.iter().map(|s| s.to_string()).collect()
        }
        fn saved_queues(&self) -> Vec<String> {
            self.queues.iter().map(|s| s.to_string()).collect()
        }
    }

    fn press(console: &mut Console, cand: &dyn Candidates, code: KeyCode) -> ConsoleAction {
        console.handle_key(KeyEvent::new(code, KeyModifiers::NONE), cand)
    }

    fn ctrl(console: &mut Console, cand: &dyn Candidates, c: char) {
        console.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL), cand);
    }

    fn type_in(console: &mut Console, cand: &dyn Candidates, text: &str) {
        for ch in text.chars() {
            press(console, cand, KeyCode::Char(ch));
        }
    }

    fn console_with(cand: &dyn Candidates, text: &str) -> Console {
        let mut c = Console::empty();
        type_in(&mut c, cand, text);
        c
    }

    /// The maintainer's example: `queue(fl` ghosts `atline`.
    #[test]
    fn a_corpus_term_ghosts_the_rest_of_the_match() {
        let fake = Fake::new(&["flatline", "floor"]);
        let c = console_with(&fake, "queue(fl");
        assert_eq!(c.suggestion.ghost, "atline");
        assert!(!c.suggestion.from_history);
        assert_eq!(c.suggestion.hint, None);
    }

    /// Fuzzy search *finds* candidates; only prefix extensions can be
    /// inserted, because anything else would replace typed characters.
    #[test]
    fn a_fuzzy_hit_that_is_not_a_prefix_extension_is_excluded() {
        // "sulfur" is a plausible fuzzy hit for "fl" (subsequence
        // f...l), and "fl" itself is an exact match with nothing left
        // to add; neither may be offered.
        let fake = Fake::new(&["sulfur surfer", "fl", "flatline"]);
        let c = console_with(&fake, "queue(fl");
        let here = site(&c.input, c.cursor);
        let hits = c.candidates_at(&here, &fake);
        assert_eq!(hits, vec!["flatline".to_string()]);
        assert_eq!(c.suggestion.ghost, "atline");
    }

    /// Accepting keeps the typed prefix byte for byte, casing included.
    #[test]
    fn accepting_never_rewrites_what_was_typed() {
        let fake = Fake::new(&["Flatline"]);
        let mut c = console_with(&fake, "queue(fl");
        assert_eq!(c.suggestion.ghost, "atline");
        ctrl(&mut c, &fake, 'f');
        assert_eq!(c.input, "queue(fl".to_string() + "atline)");
        assert!(c.input.contains("fl"), "the typed casing survived");
        assert!(!c.input.contains("Fl"));
    }

    #[test]
    fn tab_cycling_only_ever_extends_the_typed_prefix() {
        let fake = Fake::new(&["flatline", "floor", "flowers", "sulfur"]);
        let mut c = console_with(&fake, "queue(fl");
        let mut seen = Vec::new();
        for _ in 0..6 {
            press(&mut c, &fake, KeyCode::Tab);
            let cycle = c.cycle().expect("a cycle should be open");
            seen.push(cycle.items[cycle.at].clone());
            // every item, not just the current one, has to be an
            // extension of what was typed.
            for item in &cycle.items {
                assert!(
                    item.to_lowercase().starts_with("fl"),
                    "{item} would overwrite what was typed"
                );
            }
        }
        assert!(seen.contains(&"flatline".to_string()));
        assert!(seen.contains(&"floor".to_string()));
        assert!(seen.contains(&"flowers".to_string()));
        assert!(!seen.contains(&"sulfur".to_string()));
        // and it wraps rather than running off the end
        assert_eq!(c.cycle().unwrap().items.len(), 3);
    }

    #[test]
    fn shift_tab_cycles_backwards_and_esc_cancels_the_cycle() {
        let fake = Fake::new(&["flatline", "floor", "flowers"]);
        let mut c = console_with(&fake, "queue(fl");
        press(&mut c, &fake, KeyCode::Tab);
        assert_eq!(c.cycle().unwrap().at, 0);
        press(&mut c, &fake, KeyCode::BackTab);
        assert_eq!(c.cycle().unwrap().at, 2, "wraps to the end");
        // esc closes the menu, not the console
        assert!(matches!(
            press(&mut c, &fake, KeyCode::Esc),
            ConsoleAction::None
        ));
        assert!(c.cycle().is_none());
        // a second esc does close it
        assert!(matches!(
            press(&mut c, &fake, KeyCode::Esc),
            ConsoleAction::Close
        ));
    }

    #[test]
    fn typing_cancels_an_open_cycle() {
        let fake = Fake::new(&["flatline", "floor"]);
        let mut c = console_with(&fake, "queue(fl");
        press(&mut c, &fake, KeyCode::Tab);
        assert!(c.cycle().is_some());
        press(&mut c, &fake, KeyCode::Char('a'));
        assert!(c.cycle().is_none());
        assert_eq!(c.input, "queue(fla)");
    }

    #[test]
    fn tab_takes_a_lone_candidate_outright() {
        let fake = Fake::new(&["flatline"]);
        let mut c = console_with(&fake, "queue(fl");
        press(&mut c, &fake, KeyCode::Tab);
        assert!(c.cycle().is_none(), "nothing to choose between");
        assert_eq!(c.input, "queue(flatline)");
    }

    /// `qu` plus accept lands inside the parens, ready for arguments.
    #[test]
    fn accepting_a_command_name_inserts_its_parens_and_lands_inside() {
        let fake = Fake::new(&[]);
        let mut c = console_with(&fake, "qu");
        assert_eq!(c.suggestion.ghost, "eue");
        ctrl(&mut c, &fake, 'f');
        assert_eq!(c.input, "queue()");
        assert_eq!(c.cursor, 6, "the cursor sits between the parens");
    }

    #[test]
    fn a_zero_parameter_command_still_lands_inside_empty_parens() {
        let fake = Fake::new(&[]);
        let mut c = console_with(&fake, "crossf");
        ctrl(&mut c, &fake, 'f');
        assert_eq!(c.input, "crossfade()");
        assert_eq!(c.cursor, 10);
    }

    #[test]
    fn right_accepts_at_the_end_of_the_line_and_moves_the_cursor_inside_it() {
        let fake = Fake::new(&[]);
        let mut c = console_with(&fake, "qu");
        press(&mut c, &fake, KeyCode::Right);
        assert_eq!(c.input, "queue()", "right accepts at end of input");

        // mid-line, right is plain cursor movement
        let mut c = console_with(&fake, "pause()");
        c.cursor = 0;
        c.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE), &fake);
        let before = c.input.clone();
        press(&mut c, &fake, KeyCode::Right);
        assert_eq!(c.input, before, "mid-line right must not insert anything");
        assert_eq!(c.cursor, 1);
    }

    #[test]
    fn ctrl_f_accepts_mid_line_where_right_would_not() {
        let fake = Fake::new(&["flatline"]);
        let mut c = console_with(&fake, "queue(fl)");
        // step back so the cursor is inside the parens with text after
        c.cursor = 8;
        c.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE), &fake);
        assert_eq!(c.cursor, 7);
        c.cursor = 8;
        c.suggestion.ghost = "atline".to_string();
        ctrl(&mut c, &fake, 'f');
        assert_eq!(c.input, "queue(flatline)");
    }

    // ------------------------------------------------- value hints

    /// The absurdity this exists to prevent: completing `volume(5` to
    /// `50%`.
    #[test]
    fn a_value_parameter_offers_no_completion_only_a_hint() {
        let fake = Fake::new(&["50%", "50", "5 star crest"]);
        let c = console_with(&fake, "volume(5");
        assert_eq!(c.suggestion.ghost, "", "a number must not be guessed at");
        assert_eq!(c.suggestion.hint, Some("0-100, or +N% / -N%"));

        let c = console_with(&fake, "volume(");
        assert_eq!(c.suggestion.ghost, "");
        assert_eq!(c.suggestion.hint, Some("0-100, or +N% / -N%"));
        assert_eq!(
            c.suggestion.signature,
            Some(("volume(level)".to_string(), 0))
        );
    }

    #[test]
    fn tab_does_nothing_at_all_on_a_value_parameter() {
        let fake = Fake::new(&["50%", "flatline"]);
        let mut c = console_with(&fake, "volume(5");
        press(&mut c, &fake, KeyCode::Tab);
        assert!(c.cycle().is_none());
        assert_eq!(c.input, "volume(5)", "tab inserted nothing");
        assert_eq!(c.suggestion.ghost, "");
    }

    #[test]
    fn every_value_parameter_has_its_own_hint() {
        let cases = [
            ("seek(", "+30s / -10s (sign required)"),
            ("moveto(", "10% / 90 / 1m20s"),
            ("volume(", "0-100, or +N% / -N%"),
            ("speed(", "0.25-3.0, or up / down / reset"),
            ("sleep(", "30m / off"),
            ("repeat(", "how many times, 1-256"),
            ("goto(", "a queue position, 1 or higher"),
            ("remove(", "1, or [1, 3, 5]"),
            ("save_queue(", "a name to save it under"),
        ];
        let fake = Fake::new(&[]);
        for (input, want) in cases {
            let c = console_with(&fake, input);
            assert_eq!(c.suggestion.hint, Some(want), "{input}");
            assert_eq!(c.suggestion.ghost, "", "{input} should not ghost anything");
        }
    }

    #[test]
    fn a_keyword_parameter_completes_from_the_table() {
        let fake = Fake::new(&[]);
        let mut c = console_with(&fake, "view(sugg");
        assert_eq!(c.suggestion.ghost, "estions");
        ctrl(&mut c, &fake, 'f');
        assert_eq!(c.input, "view(suggestions)");

        let c = console_with(&fake, "eq(ro");
        assert_eq!(c.suggestion.ghost, "ck");

        let c = console_with(&fake, "loop(al");
        assert_eq!(c.suggestion.ghost, "l");
    }

    #[test]
    fn a_saved_name_completes_from_the_hosts_saved_queues() {
        let fake = Fake {
            names: vec!["flatline"],
            queues: vec!["gym", "gym 2024", "sleep"],
        };
        let c = console_with(&fake, "load_queue(gym ");
        assert_eq!(c.suggestion.ghost, "2024");
        // and a new name has nothing to complete from, by definition
        let c = console_with(&fake, "save_queue(gym");
        assert_eq!(c.suggestion.ghost, "");
        assert!(c.suggestion.hint.is_some());
    }

    // ------------------------------------------------ history source

    #[test]
    fn a_whole_past_line_is_suggested_once_the_input_is_its_prefix() {
        let fake = Fake::new(&[]);
        let mut c = Console::empty();
        c.history = vec!["queue([hex, crest])".into()];
        type_in(&mut c, &fake, "queue(");
        // the auto-paired ')' means the input is "queue()", which is
        // not a prefix of the past line, so step over it first
        c.cursor = c.input.len();
        c.input = "queue(".to_string();
        c.cursor = c.input.len();
        c.refresh(&fake);
        assert!(c.suggestion.from_history);
        assert_eq!(c.suggestion.ghost, "[hex, crest])");
        press(&mut c, &fake, KeyCode::Right);
        assert_eq!(c.input, "queue([hex, crest])");
    }

    /// Tab is for tokens and only tokens; the history line is reached
    /// with right arrow or ctrl+f.
    #[test]
    fn tab_cycles_tokens_rather_than_the_history_line() {
        let fake = Fake::new(&["flatline", "floor", "flowers"]);
        let mut c = Console::empty();
        c.history = vec!["queue(fl is a whole past line)".into()];
        c.input = "queue(fl".to_string();
        c.cursor = c.input.len();
        c.refresh(&fake);
        assert!(c.suggestion.from_history, "history wins the ghost");

        press(&mut c, &fake, KeyCode::Tab);
        let cycle = c.cycle().expect("tab opens a token cycle");
        assert!(
            cycle
                .items
                .iter()
                .all(|i| i.to_lowercase().starts_with("fl")),
            "{:?}",
            cycle.items
        );
        assert!(
            !cycle.items.iter().any(|i| i.contains("whole past line")),
            "tab must not cycle the history line"
        );
        assert!(!c.suggestion.from_history);
    }

    #[test]
    fn the_history_ghost_only_shows_with_the_cursor_at_the_end() {
        let fake = Fake::new(&[]);
        let mut c = Console::empty();
        c.history = vec!["pause(); skip()".into()];
        c.input = "pause()".to_string();
        c.cursor = c.input.len();
        c.refresh(&fake);
        assert!(c.suggestion.from_history);
        c.cursor = 3;
        c.refresh(&fake);
        assert!(
            !c.suggestion.from_history,
            "mid-line, the rest of a past line is not what comes next"
        );
    }

    #[test]
    fn up_and_down_walk_the_history_and_restore_the_draft() {
        let fake = Fake::new(&[]);
        let mut c = Console::empty();
        c.history = vec!["pause()".into(), "skip()".into()];
        c.input = "half".to_string();
        c.cursor = 4;
        press(&mut c, &fake, KeyCode::Up);
        assert_eq!(c.input, "skip()");
        press(&mut c, &fake, KeyCode::Up);
        assert_eq!(c.input, "pause()");
        press(&mut c, &fake, KeyCode::Up);
        assert_eq!(c.input, "pause()", "clamps at the oldest entry");
        press(&mut c, &fake, KeyCode::Down);
        assert_eq!(c.input, "skip()");
        press(&mut c, &fake, KeyCode::Down);
        assert_eq!(c.input, "half", "the half-typed line comes back");
    }

    // ---------------------------------------------------- ranking

    #[test]
    fn frequency_beats_alphabetical_and_recency_breaks_ties() {
        let mut ranks = Ranker::default();
        // "floor" used twice, "flowers" once, "flatline" never
        ranks.bump("flowers");
        ranks.bump("floor");
        ranks.bump("floor");
        let mut items = vec![
            "flatline".to_string(),
            "floor".to_string(),
            "flowers".to_string(),
        ];
        ranks.order(&mut items);
        assert_eq!(items, vec!["floor", "flowers", "flatline"]);

        // equal frequency: the more recent one leads
        let mut ranks = Ranker::default();
        ranks.bump("flowers");
        ranks.bump("floor");
        let mut items = vec!["flowers".to_string(), "floor".to_string()];
        ranks.order(&mut items);
        assert_eq!(items, vec!["floor", "flowers"]);
    }

    #[test]
    fn anything_unranked_keeps_the_order_it_arrived_in() {
        let ranks = Ranker::default();
        let original = vec!["zzz".to_string(), "aaa".to_string(), "mmm".to_string()];
        let mut items = original.clone();
        ranks.order(&mut items);
        assert_eq!(
            items, original,
            "relevance order from the corpus must survive"
        );
    }

    #[test]
    fn ranking_is_deterministic_across_repeat_runs() {
        let build = || {
            let mut ranks = Ranker::default();
            ranks.bump("floor");
            ranks.bump("flowers");
            ranks.bump("floor");
            ranks
        };
        let order = |ranks: &Ranker| {
            let mut items = vec![
                "flatline".to_string(),
                "floor".to_string(),
                "flowers".to_string(),
            ];
            ranks.order(&mut items);
            items
        };
        assert_eq!(order(&build()), order(&build()));
    }

    /// A line that was run outranks one that was not, so the whole-line
    /// suggestion learns from use rather than just recency.
    #[test]
    fn the_history_suggestion_follows_the_ranking() {
        let fake = Fake::new(&[]);
        let mut c = Console::empty();
        c.history = vec!["queue(hex)".into(), "queue(crest)".into()];
        // "queue(hex)" is older but used more
        c.ranks.bump("queue(hex)");
        c.ranks.bump("queue(hex)");
        c.ranks.bump("queue(crest)");
        c.input = "queue(".to_string();
        c.cursor = c.input.len();
        c.refresh(&fake);
        assert_eq!(c.suggestion.ghost, "hex)");
    }

    #[test]
    fn running_a_line_records_it_for_ranking() {
        let fake = Fake::new(&[]);
        let mut c = Console::empty();
        c.input = "pause()".to_string();
        c.cursor = c.input.len();
        assert!(matches!(
            press(&mut c, &fake, KeyCode::Enter),
            ConsoleAction::Run(_)
        ));
        assert_eq!(c.ranks.stat("pause()").uses, 1);
    }

    #[test]
    fn accepting_a_completion_records_the_word_not_the_line() {
        let fake = Fake::new(&["flatline"]);
        let mut c = console_with(&fake, "queue(fl");
        ctrl(&mut c, &fake, 'f');
        assert_eq!(c.ranks.stat("flatline").uses, 1);
        assert_eq!(c.ranks.stat("queue(flatline)").uses, 0);
    }

    #[test]
    fn the_rank_store_prunes_rather_than_growing_without_bound() {
        let mut ranks = Ranker::default();
        for i in 0..(MAX_RANKS + 40) {
            ranks.bump(&format!("entry {i}"));
        }
        assert!(
            ranks.entries.len() <= MAX_RANKS,
            "grew to {}",
            ranks.entries.len()
        );
        // the newest survive
        assert_eq!(ranks.stat(&format!("entry {}", MAX_RANKS + 39)).uses, 1);
    }

    // -------------------------------------------------------- console

    #[test]
    fn submitting_a_good_line_clears_the_input_and_returns_a_program() {
        let fake = Fake::new(&[]);
        let mut c = console_with(&fake, "pause()");
        assert_eq!(c.input, "pause()");
        match press(&mut c, &fake, KeyCode::Enter) {
            ConsoleAction::Run(_) => {}
            other => panic!("expected a program, got {other:?}"),
        }
        assert!(c.input.is_empty());
        assert!(c.error.is_none());
        assert_eq!(c.history(), ["pause()"]);
        assert_eq!(c.suggestion, Suggestion::default());
    }

    #[test]
    fn submitting_a_bad_line_keeps_it_and_shows_the_error() {
        let fake = Fake::new(&[]);
        let mut c = console_with(&fake, "shufle()");
        assert!(matches!(
            press(&mut c, &fake, KeyCode::Enter),
            ConsoleAction::None
        ));
        assert_eq!(c.input, "shufle()", "a bad line stays so it can be fixed");
        let error = c.error.clone().expect("an error should be shown");
        assert!(error.contains("did you mean 'shuffle'"), "{error}");
        assert!(error.starts_with("col 1:"), "{error}");
        assert!(c.history().is_empty(), "a bad line is not remembered");
        press(&mut c, &fake, KeyCode::Backspace);
        assert!(c.error.is_none());
    }

    #[test]
    fn esc_closes_and_so_does_enter_on_an_empty_line() {
        let fake = Fake::new(&[]);
        let mut c = Console::empty();
        assert!(matches!(
            press(&mut c, &fake, KeyCode::Esc),
            ConsoleAction::Close
        ));
        assert!(matches!(
            press(&mut c, &fake, KeyCode::Enter),
            ConsoleAction::Close
        ));
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
        let fake = Fake::new(&[]);
        let mut c = Console::empty();
        c.paste("queue([a, b])\n", &fake);
        assert_eq!(c.input, "queue([a, b])");
        assert_eq!(c.cursor, c.input.len());
    }

    #[test]
    fn a_control_chord_we_do_not_handle_inserts_nothing() {
        let fake = Fake::new(&[]);
        let mut c = Console::empty();
        ctrl(&mut c, &fake, 'x');
        assert!(c.input.is_empty(), "ctrl+x typed an 'x'");
    }
}
