//! shellsong: a command language for the player. everything is a call
//! (`pause()`, `queue([hex, 4 star crest])`), lowered here into `Op`s
//! the host applies through the same paths its keybinds already use, so
//! behaviour can never drift from the keyboard's. search is
//! asynchronous, so `Program` is a resumable state machine rather than
//! a loop. the full language reference is docs/shellsong.md.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::fs;
use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::{Deserialize, Serialize};

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
    /// Never resolved from a search term (no silence is in the
    /// corpus); reachable only by asking a queue entry what it is.
    Silence,
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
            SearchItem::Silence { .. } => Kind::Silence,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Kind::Song => "song",
            Kind::Album => "album",
            Kind::Artist => "artist",
            Kind::Playlist => "playlist",
            Kind::Video => "video",
            Kind::Silence => "silence",
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
/// **Load-bearing host contract, do not break:** an `Index` is live
/// only until the next thing that rebuilds the results list, which can
/// be the very next op of the same program. It survives today because
/// the host applies each `Step::Emit` before stepping again and nothing
/// holds one across that; see docs/shellsong.md, "the index contract".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// index into `Env::results`, resolved by the executor from a term.
    Index(usize),
    /// whatever is highlighted right now.
    Selection,
}

/// Which identity a per-identity setting hangs off. Resolution runs
/// track, then album, then artist, **per setting**: a track-level eq
/// and an album-level volume both apply, because masking one with the
/// other would make a single remembered eq silently drop a remembered
/// volume. See docs/shellsong.md.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    Track,
    Album,
    Artist,
}

impl Tier {
    pub fn label(self) -> &'static str {
        match self {
            Tier::Track => "track",
            Tier::Album => "album",
            Tier::Artist => "artist",
        }
    }

    fn of(word: &str) -> Option<Tier> {
        [Tier::Track, Tier::Album, Tier::Artist]
            .into_iter()
            .find(|t| t.label() == word)
    }
}

/// Which setting a binding is about, named for the command that sets
/// it so there is no second vocabulary to keep in step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SettingKind {
    Eq,
    Volume,
    Speed,
}

impl SettingKind {
    pub fn label(self) -> &'static str {
        match self {
            SettingKind::Eq => "eq",
            SettingKind::Volume => "volume",
            SettingKind::Speed => "speed",
        }
    }

    fn of(word: &str) -> Option<SettingKind> {
        [SettingKind::Eq, SettingKind::Volume, SettingKind::Speed]
            .into_iter()
            .find(|k| k.label() == word)
    }
}

/// A remembered setting and its value. Built from the inner call of
/// `remember(tier, call)`, so the value went through that command's own
/// parameter type and nothing here re-validates it.
#[derive(Debug, Clone, PartialEq)]
pub enum Setting {
    Eq(String),
    /// Absolute only: a nudge is a change, not a value to remember.
    Volume(f64),
    Speed(f64),
}

impl Setting {
    pub fn kind(&self) -> SettingKind {
        match self {
            Setting::Eq(_) => SettingKind::Eq,
            Setting::Volume(_) => SettingKind::Volume,
            Setting::Speed(_) => SettingKind::Speed,
        }
    }

    /// One line naming the value, for the listing and the reports.
    pub fn value(&self) -> String {
        match self {
            Setting::Eq(name) => name.clone(),
            Setting::Volume(v) => format!("{v}%"),
            Setting::Speed(s) => format!("{s}x"),
        }
    }

    /// The op that applies it. The binding therefore goes through the
    /// very same host arm `eq(night)` typed into the console does.
    pub fn op(&self) -> Op {
        match self {
            Setting::Eq(name) => Op::SetEqPreset(name.clone()),
            Setting::Volume(v) => Op::SetVolume(Level::Absolute(*v)),
            Setting::Speed(s) => Op::SetSpeed(*s),
        }
    }

    /// What `remember(tier, call)` accepts, read off the inner call.
    /// `Err` says what it would have taken, since this is the one place
    /// that knows.
    fn of_call(call: &Call) -> Result<Setting, String> {
        match call.simple_op() {
            Some(Op::SetEqPreset(name)) => Ok(Setting::Eq(name)),
            Some(Op::SetVolume(Level::Absolute(v))) => Ok(Setting::Volume(v)),
            Some(Op::SetVolume(Level::Delta(_))) => Err(
                "remember takes a volume to settle on, not a nudge: write volume(40%)".to_string(),
            ),
            Some(Op::SetSpeed(s)) => Ok(Setting::Speed(s)),
            Some(Op::SpeedUp) | Some(Op::SpeedDown) => {
                Err("remember takes a speed to settle on, not a step: write speed(1.1)".to_string())
            }
            _ => Err(format!(
                "remember takes {}; '{}' is not one of them",
                SETTINGS.join(", "),
                call.command.name
            )),
        }
    }
}

/// The settable things a binding can hold, in the order `help` and the
/// error messages list them.
const SETTINGS: &[&str] = &["eq(...)", "volume(...)", "speed(...)"];

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
    /// The defined variables, listed as `let` lines.
    Vars,
    /// The per-identity settings, listed.
    Bindings,
    Eq,
    /// The undo tree. A screen, not a listing: the structure is the
    /// content, and a flat set of scrollback lines cannot show a fork.
    Undo,
    /// The command listing: the one target whose content is generated
    /// by the language itself (`help_lines`, off `COMMANDS`) rather than
    /// read from the app, so `view(help)` and `help()` cannot drift. It
    /// still lowers to this op like every other target; the host prints
    /// the lines, same as it does for the other listings.
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
                | ViewTarget::Vars
                | ViewTarget::Bindings
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
    view("vars", ViewTarget::Vars, true),
    view("variables", ViewTarget::Vars, false),
    view("bindings", ViewTarget::Bindings, true),
    view("eq", ViewTarget::Eq, true),
    view("equalizer", ViewTarget::Eq, false),
    view("undo", ViewTarget::Undo, true),
    view("tree", ViewTarget::Undo, false),
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
    SetEqPreset(String),
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
    /// Wipes the console's own scrollback. Console-local: the host
    /// clears `Console::lines` and nothing else moves, which is why it
    /// is an op rather than something the parser handles, so it still
    /// shows up in `dry` like every other command.
    ClearConsole,
    ClearQueue,
    /// 1-based queue positions, descending so the host can remove in
    /// order without reindexing.
    RemoveFromQueue(Vec<usize>),
    GotoQueue(usize),
    ShuffleQueue,
    /// Walk the undo tree. Classified `Ignored` in `undo::classify`,
    /// and that is load-bearing: recording an undo would make undoing
    /// it a new node instead of a redo, and the tree would grow a
    /// branch every time someone changed their mind.
    Undo,
    Redo,
    /// The pause between tracks. `None` turns it off, which is why
    /// this takes `Ty::Duration` where `silence` needed `Ty::Span`:
    /// here `off` means something.
    SetGap(Option<f64>),
    /// A timed silence in the queue. `front` inserts it next, matching
    /// `add_to_queue`'s position parameter rather than inventing a
    /// second way to say the same thing.
    Silence {
        secs: u64,
        front: bool,
    },
    SaveQueue(String),
    LoadQueue(String),
    /// bind a name to a list of search terms, persisted. The terms are
    /// already expanded (a variable used inside another is resolved
    /// while parsing), so the host only stores what it is given.
    Define {
        name: String,
        terms: Vec<String>,
    },
    Unset(String),
    /// Bind a setting to whatever is playing, at `tier`. The identity
    /// is read from the track by the host, since only it knows what is
    /// playing; the language only says which tier and which value.
    Remember {
        tier: Tier,
        setting: Setting,
    },
    /// Drop one setting's binding at `tier`, or every setting's.
    Forget {
        tier: Tier,
        setting: Option<SettingKind>,
    },
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
            Op::SetEqPreset(name) => format!("eq: {name}"),
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
            Op::ClearConsole => "scrollback cleared".into(),
            Op::ClearQueue => "queue cleared".into(),
            Op::RemoveFromQueue(v) => format!("removed {} from the queue", v.len()),
            Op::GotoQueue(n) => format!("jumped to queue {n}"),
            Op::ShuffleQueue => "queue shuffled".into(),
            Op::Undo => "undone".into(),
            Op::Redo => "redone".into(),
            Op::SetGap(Some(s)) => format!("gap {s}s between tracks"),
            Op::SetGap(None) => "gap off".into(),
            Op::Silence { secs, front } => format!(
                "{} a {} silence",
                if *front { "inserted next" } else { "queued" },
                crate::ui::progress::fmt_span(*secs)
            ),
            Op::SaveQueue(n) => format!("saved queue as '{n}'"),
            Op::LoadQueue(n) => format!("loaded queue '{n}'"),
            Op::Define { name, terms } => {
                format!("let {name} = [{}]", terms.join(", "))
            }
            Op::Unset(name) => format!("unset {name}"),
            Op::Remember { tier, setting } => format!(
                "remembered {} {} for this {}",
                setting.kind().label(),
                setting.value(),
                tier.label()
            ),
            Op::Forget {
                tier,
                setting: Some(kind),
            } => format!("forgot this {}'s {}", tier.label(), kind.label()),
            Op::Forget {
                tier,
                setting: None,
            } => {
                format!("forgot everything for this {}", tier.label())
            }
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
    /// the name of an eq curve, built-in or saved. A free name rather
    /// than a `Words` set because the saved half only exists at
    /// runtime; see `Completes::EqPresets`.
    EqName,
    /// a variable name. Completes from the defined ones even where a
    /// new name is being invented (`define`), because redefining is the
    /// common case: the name you want is usually already there.
    VarName,
    /// a `.shg` script: a bare name under the config dir, or a path.
    ScriptName,
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
    /// a positive length of time, with no `off`: a span that *is*
    /// something (a silence in the queue), where `Ty::Duration` is a
    /// timer that can be cancelled. sharing `Duration` would have made
    /// `silence(off)` parse and then do nothing at all, silently,
    /// since there is no silence of no length.
    Span,
    /// one of an enumerated keyword set.
    Words(Words),
    /// a nested call, for the higher-order commands.
    Call,
    /// a nested call that has to *be* a setting: `remember` can only
    /// settle on an eq curve, an absolute volume or a fixed speed, so
    /// offering it `clear_queue` would be offering something that does
    /// not parse (R12).
    SettingCall,
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
    /// which identity a `remember`/`forget` is about.
    Tier,
    /// which setting a `forget` drops.
    Setting,
}

impl Words {
    /// What completion offers and `help()` prints: canonical spellings
    /// only, so the list stays readable even where the parser accepts
    /// several words for one thing. Borrowed rather than built, because
    /// completion asks for a set on every keystroke.
    pub fn options(self) -> &'static [&'static str] {
        static EQ_PRESETS: LazyLock<Vec<&'static str>> =
            LazyLock::new(|| eq::PRESETS.iter().map(|p| p.name).collect());
        static VIEW_TARGETS: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
            VIEW_QUERY_WORDS[..1]
                .iter()
                .copied()
                .chain(VIEW_NAMES.iter().filter(|v| v.canonical).map(|v| v.word))
                .collect()
        });
        match self {
            Words::Position => &["front", "back"],
            Words::LoopMode => &["one", "all", "off"],
            Words::Switch => &["on", "off"],
            Words::EqPreset => &EQ_PRESETS,
            Words::ViewTarget => &VIEW_TARGETS,
            Words::FadeDir => &["in", "out"],
            Words::Tier => &["track", "album", "artist"],
            Words::Setting => &["eq", "volume", "speed"],
        }
    }

    /// Every spelling the parser accepts, which is a superset of
    /// `options`; validation reads this so an alias still parses even
    /// though completion does not advertise it.
    fn accepts(self) -> &'static [&'static str] {
        static VIEW_ACCEPTS: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
            VIEW_QUERY_WORDS
                .iter()
                .copied()
                .chain(VIEW_NAMES.iter().map(|v| v.word))
                .collect()
        });
        match self {
            Words::ViewTarget => &VIEW_ACCEPTS,
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
    /// the defined variables ahead of names from the host's local
    /// corpus. One class, because both are things that legitimately go
    /// in a terms slot; variables lead because naming one is the more
    /// specific intent.
    CorpusTerms,
    /// variable names only.
    Variables,
    /// the `.shg` scripts the host can see.
    Scripts,
    /// the three commands `remember` accepts, with their parens.
    SettingNames,
    /// command names.
    CommandNames,
    /// the host's saved queue names.
    SavedQueues,
    /// built-in eq curves plus whatever the user has saved. Not a
    /// `Words` set: the saved half only exists at runtime, which is the
    /// whole reason `eq()` could not reach it before.
    EqPresets,
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
            Ty::EqName => Completes::EqPresets,
            Ty::VarName => Completes::Variables,
            Ty::ScriptName => Completes::Scripts,
            Ty::CommandName | Ty::Call => Completes::CommandNames,
            Ty::SettingCall => Completes::SettingNames,
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
            Ty::Span => Completes::Hint("15m / 30s / 1h30m"),
        }
    }

    /// Keywords this type accepts. Unused: the parser's own error path
    /// reaches `Words::options` directly, and completion goes through
    /// `completes`. Kept because every other `Ty` accessor is here and
    /// a type that accepts keywords without a way to list them is the
    /// surprising half of the pair.
    #[allow(dead_code)]
    pub fn options(self) -> &'static [&'static str] {
        match self {
            Ty::Words(w) => w.options(),
            Ty::Speed => &["up", "down", "reset"],
            Ty::Duration => &["off"],
            _ => &[],
        }
    }

    fn describe(self) -> String {
        match self {
            Ty::Terms => "a term or [list of terms]".into(),
            Ty::Query => "something to search for".into(),
            Ty::NewName => "a name".into(),
            Ty::SavedName => "the name of a saved queue".into(),
            Ty::EqName => "an eq preset name".into(),
            Ty::VarName => "a variable name".into(),
            Ty::ScriptName => "the name of a .shg script, or a path to one".into(),
            Ty::CommandName => "a command name".into(),
            Ty::Pos => "an unsigned position like 30s, 1m20s or 50%".into(),
            Ty::Offset => "a signed offset like +30s or -10s".into(),
            Ty::Level => "a percent like 50% or +10%".into(),
            Ty::Count => "a positive whole number".into(),
            Ty::Span => "a length of time like 15m, 30s or 1h30m".into(),
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
            Ty::SettingCall => format!("one of {}", SETTINGS.join(", ")),
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
        &[req("preset", Ty::EqName)],
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
    cmd("undo", &[], NONE, &[], "step back through the undo tree"),
    cmd("redo", &[], NONE, &[], "step forward again"),
    cmd(
        "gap",
        &[],
        &[req("between", Ty::Duration)],
        &[],
        "pause between every track: gap(10s), gap(off)",
    ),
    cmd(
        "silence",
        // `wait` because that is what the maintainer reached for, and
        // it reads better in a queue ("wait 15s, then the next
        // track"). `silence` stays canonical, since it also names
        // what actually plays: a generated silent source, not a
        // suspended player.
        &["wait"],
        &[
            req("length", Ty::Span),
            opt("position", Ty::Words(Words::Position)),
        ],
        &[],
        "put a timed silence in the queue: silence(15m)",
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
        "clear",
        &["cls"],
        NONE,
        &[],
        "clear this console's scrollback",
    ),
    cmd(
        "clear_queue",
        &["qclear", "flush"],
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
    // variables. `define` is canonical and `let name = [...]` is sugar
    // for it, so R1 stays true of every lowered program; see
    // docs/shellsong.md.
    cmd(
        "define",
        &[],
        &[
            req_hint(
                "name",
                Ty::VarName,
                "let name = [a, b] is the short way to write this",
            ),
            req("terms", Ty::Terms),
        ],
        &[],
        "bind a name to a list of terms",
    ),
    cmd(
        "unset",
        &[],
        &[req("name", Ty::VarName)],
        &[],
        "drop a variable",
    ),
    // per-identity settings. Higher-order like `repeat` and `dry`: the
    // setting is the inner call, so there is no parallel vocabulary of
    // setting names and values to keep in step with the real commands.
    cmd(
        "remember",
        &[],
        &[
            req("tier", Ty::Words(Words::Tier)),
            req("call", Ty::SettingCall),
        ],
        &[],
        "bind a setting to what is playing, by track, album or artist",
    ),
    cmd(
        "forget",
        &[],
        &[
            req("tier", Ty::Words(Words::Tier)),
            opt("setting", Ty::Words(Words::Setting)),
        ],
        &[],
        "drop what was remembered, for one setting or all of them",
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
        "run",
        &[],
        &[req("script", Ty::ScriptName)],
        &[],
        "run a .shg script; dry(run(x)) previews it",
    ),
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

/// Every written name, suffixed spellings included, to the command it
/// resolves to and the `kind=` value the suffix desugars to. Built once:
/// `site` resolves the enclosing name on every keystroke, and a linear
/// scan of the table plus its alias lists was that whole cost.
///
/// A plain spelling is inserted before any suffixed one, so a direct
/// name still wins over a collision the way the scan it replaced did.
type Resolved = (&'static Command, Option<&'static str>);

fn by_name() -> &'static HashMap<String, Resolved> {
    static BY_NAME: LazyLock<HashMap<String, Resolved>> = LazyLock::new(|| {
        let mut map: HashMap<String, Resolved> = HashMap::new();
        for c in COMMANDS {
            for name in std::iter::once(&c.name).chain(c.aliases) {
                map.entry((*name).to_string()).or_insert((c, None));
            }
        }
        for c in COMMANDS.iter().filter(|c| widenable(c)) {
            for name in std::iter::once(&c.name).chain(c.aliases) {
                for (suffix, kind) in KIND_SUFFIXES {
                    map.entry(format!("{name}{suffix}"))
                        .or_insert((c, Some(*kind)));
                }
            }
        }
        map
    });
    &BY_NAME
}

/// Resolves a written name to a command plus the `kind=` value its
/// suffix desugars to, if any.
fn lookup(name: &str) -> Option<Resolved> {
    by_name().get(name).copied()
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
pub fn every_name() -> &'static [&'static str] {
    static NAMES: LazyLock<Vec<&'static str>> = LazyLock::new(|| {
        let mut names: Vec<&'static str> = Vec::new();
        for c in COMMANDS {
            names.push(c.name);
            names.extend_from_slice(c.aliases);
        }
        names.sort_unstable();
        names
    });
    &NAMES
}

/// Closest known name to `name`, for "did you mean".
fn did_you_mean(name: &str) -> Option<&'static str> {
    closest(name, every_name())
}

/// The "that is not a command" message. One special case: `let` is a
/// real statement form, so writing it as a call points at the binding
/// syntax rather than at whatever name happens to be nearest.
fn unknown_name(name: &str) -> String {
    if name == LET {
        return "let is not a call: write let name = [a, b]".to_string();
    }
    match did_you_mean(name) {
        Some(s) => format!("unknown command '{name}'; did you mean '{s}'?"),
        None => format!("unknown command '{name}'"),
    }
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

/// `name(a, b)` off the cache rather than rebuilt: `site` wants the
/// enclosing call's signature on every keystroke, and `signature`
/// formats and joins to produce it.
fn signature_of(c: &Command) -> String {
    static SIGS: LazyLock<HashMap<&'static str, String>> =
        LazyLock::new(|| COMMANDS.iter().map(|c| (c.name, c.signature())).collect());
    SIGS.get(c.name).cloned().unwrap_or_else(|| c.signature())
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

// ----------------------------------------------------------- variables

/// Longest variable name accepted. A name is typed often and listed in
/// one line, so it is a label rather than a sentence.
const MAX_VAR_NAME: usize = 32;

/// The variables a program is parsed against: a name standing for a
/// list of **search terms**, not for resolved tracks. That choice is
/// deliberate and documented in docs/shellsong.md: a variable is a vibe
/// that follows the library, and `save_queue` already covers the frozen
/// case.
///
/// Expansion happens while parsing, in `coerce`'s `Ty::Terms` arm, so
/// every parameter that takes terms gets it with no per-command code and
/// `dry()` prints the expanded form (the same property a kind suffix
/// has, R6). Expansion is not recursive: a variable's own terms are
/// expanded once, when it is defined, so composing variables cannot
/// cycle.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Scope {
    /// Sorted by name, which is the order the listing and the
    /// completion candidates both want.
    vars: BTreeMap<String, Vec<String>>,
}

impl Scope {
    /// Binds `name`, replacing any previous binding. Returns whether
    /// this replaced one, so the host can say which it did (R9).
    pub fn define(&mut self, name: &str, terms: Vec<String>) -> bool {
        self.vars.insert(name.to_string(), terms).is_some()
    }

    /// Drops `name`; false when there was nothing bound to it, which is
    /// what sends the host to `namehint::unknown`.
    pub fn remove(&mut self, name: &str) -> bool {
        self.vars.remove(name).is_some()
    }

    pub fn get(&self, name: &str) -> Option<&[String]> {
        self.vars.get(name).map(Vec::as_slice)
    }

    /// Alphabetical, for completion and for the listing.
    pub fn names(&self) -> Vec<&str> {
        self.vars.keys().map(String::as_str).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.vars.is_empty()
    }

    /// Replaces every item that names a variable with that variable's
    /// terms, leaving everything else alone. A term is not a namespace,
    /// so an item that matches nothing here is simply a search term;
    /// that is the price of having no sigil (R1 allows no prefixes), and
    /// `dry()` is how you check which happened.
    fn expand(&self, items: &[String]) -> Vec<String> {
        if self.vars.is_empty() {
            return items.to_vec();
        }
        items
            .iter()
            .flat_map(|item| match self.get(item) {
                Some(terms) => terms.to_vec(),
                None => vec![item.clone()],
            })
            .collect()
    }

    /// What `view(vars)` prints: one `let` line each, so the listing is
    /// also valid input you can paste into a `.shg` file.
    pub fn lines(&self) -> Vec<String> {
        if self.vars.is_empty() {
            return vec!["no variables defined; let name = [a, b] defines one".to_string()];
        }
        self.vars
            .iter()
            .map(|(name, terms)| format!("let {name} = [{}]", terms.join(", ")))
            .collect()
    }
}

/// Whether `name` is usable as a variable name, and why not if it is
/// not. Lowercase snake_case, matching R3's rule for command names, so
/// a bare identifier reads as a name rather than as the free text a
/// search term is.
fn check_var_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("a variable needs a name".into());
    }
    if name.chars().count() > MAX_VAR_NAME {
        return Err(format!("'{name}' is longer than {MAX_VAR_NAME} characters"));
    }
    if !name.starts_with(|c: char| c.is_ascii_lowercase()) {
        return Err(format!("'{name}' must start with a lowercase letter"));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        return Err(format!("'{name}' is not lowercase snake_case"));
    }
    if lookup(name).is_some() {
        return Err(format!("'{name}' is a command name; pick another"));
    }
    Ok(())
}

/// The empty scope, borrowed: `Candidates::scope` and `World::bare`
/// both need one that outlives the call.
fn empty_scope() -> &'static Scope {
    static EMPTY: LazyLock<Scope> = LazyLock::new(Scope::default);
    &EMPTY
}

// --------------------------------------------------------- the world

/// Reads one `.shg` script by name. Borrowed rather than owned so the
/// host can close over its own state and a test over a map.
pub type ScriptReader<'a> = &'a dyn Fn(&str) -> Result<String, String>;

/// What *parsing* may ask the host about: the variables a term might
/// name, and the contents of a `.shg` file `run(...)` names. `Env` is
/// the run-time counterpart; this one exists because both of these have
/// to be known while the program is still being built, which is what
/// lets `dry()` report a whole script's plan.
pub struct World<'a> {
    pub scope: &'a Scope,
    /// Reads one script. `Err` carries a ready-to-print reason, because
    /// only the host knows how a name resolves to a file and which
    /// scripts exist (so the "did you mean" is its to build).
    ///
    /// `None` is "this context has no scripts", which is the truth for
    /// a bare parse rather than a failure to look.
    pub read: Option<ScriptReader<'a>>,
}

impl World<'_> {
    /// No variables and no scripts.
    pub fn bare() -> World<'static> {
        World {
            scope: empty_scope(),
            read: None,
        }
    }

    fn read_script(&self, name: &str) -> Result<String, String> {
        match self.read {
            Some(read) => read(name),
            None => Err(format!("no scripts are available here ('{name}')")),
        }
    }
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
    /// A line of a script that did not parse. Reported and stepped
    /// past rather than failing the program: a script is a list of
    /// independent statements, and a typo on line 3 should not cost you
    /// the other twenty.
    Fail(String),
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
    /// the host should push this as an *error* line, and then carry on
    /// stepping: a script's bad line, which is reported without
    /// stopping the rest of the file.
    Warn(String),
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
    /// Parses against no variables. Every name therefore stays a search
    /// term, which is what the language's own tests want and what a
    /// host with nothing defined gets anyway; the host itself always
    /// has a scope, so it goes through `parse_in`.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn parse(input: &str) -> Result<Program, ParseError> {
        Program::parse_in(input, &World::bare())
    }

    /// Parses against `scope`, expanding any term that names a
    /// variable. Parse time rather than run time on purpose: `dry()`
    /// then reports the expanded plan, and the whole expansion is one
    /// arm of `coerce` instead of a second code path per command.
    pub fn parse_in(input: &str, world: &World) -> Result<Program, ParseError> {
        let mut lowering = Lowering::new(world);
        for (offset, text) in statements(input) {
            lowering.absorb(input, offset, text)?;
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

    /// A program that is nothing but these ops, for the host's own
    /// synthesised work (the per-identity settings a track change asks
    /// for). It goes through the same stepper and therefore the same
    /// `apply_shellsong_op`, which is what keeps a per-track eq from
    /// behaving differently from `eq(night)` typed into the console.
    pub fn of(ops: Vec<Op>) -> Program {
        Program {
            nodes: ops.into_iter().map(Node::Emit).collect(),
            pc: 0,
            pending: None,
            groups: Vec::new(),
            done: false,
        }
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
            Node::Fail(text) => {
                self.pc += 1;
                Step::Warn(text)
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

/// How many `run(...)` frames deep a parse may go. A script that runs
/// another is useful (a setup file calling a per-mood one); one that
/// reaches itself is not, and this is the cheap bound that makes the
/// difference without a visited set.
const MAX_SCRIPT_DEPTH: usize = 8;

struct Lowering<'a> {
    nodes: Vec<Node>,
    groups: usize,
    /// The variables as they stand *at this point in the program*: a
    /// `let` updates it, so a later statement sees a binding an earlier
    /// one made. Owned rather than borrowed precisely so it can grow;
    /// that is what makes a `.shg` file that defines a variable and
    /// then uses it work, and it does the same for `let a = [x]; play(a)`
    /// typed on one line.
    scope: Scope,
    read: Option<ScriptReader<'a>>,
    /// How many `run(...)` frames are open, for `MAX_SCRIPT_DEPTH`.
    depth: usize,
}

impl<'a> Lowering<'a> {
    fn new(world: &World<'a>) -> Lowering<'a> {
        Lowering {
            nodes: Vec::new(),
            groups: 0,
            scope: world.scope.clone(),
            read: world.read,
            depth: 0,
        }
    }

    fn nested(&self) -> Lowering<'a> {
        Lowering {
            nodes: Vec::new(),
            groups: 0,
            scope: self.scope.clone(),
            read: self.read,
            depth: self.depth,
        }
    }

    /// Parses one statement against the scope as it stands and lowers
    /// it. The one entry point for both a typed line and a script line,
    /// so they cannot diverge; the only difference between them is what
    /// the caller does with the `Err`.
    fn absorb(&mut self, input: &str, offset: usize, text: &str) -> Result<(), ParseError> {
        let call = {
            let world = World {
                scope: &self.scope,
                read: self.read,
            };
            parse_statement(input, offset, text, &world)?
        };
        self.track(&call);
        self.lower(&call, false);
        Ok(())
    }

    /// Keeps `scope` in step with the bindings the program makes, so
    /// the expansion a later statement gets matches what will actually
    /// be bound by the time it runs.
    fn track(&mut self, call: &Call) {
        match (call.command.name, call.args.first(), call.args.get(1)) {
            ("define", Some(Arg::Name(name)), Some(Arg::Terms(terms))) => {
                self.scope.define(name, terms.clone());
            }
            ("unset", Some(Arg::Name(name)), _) => {
                self.scope.remove(name);
            }
            _ => {}
        }
    }

    /// `dry` lowers the same shape and then turns every node into a
    /// note, so the report is generated from the real plan rather than
    /// a second description of it. A script goes through here too,
    /// which is the whole reason `dry(run(x))` can preview a file it
    /// never executes: the plan is the real lowered program.
    fn lower(&mut self, call: &Call, dry: bool) {
        if dry {
            let mut inner = self.nested();
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
            "run" => {
                if let Some(Arg::Script { name, source }) = call.args.first() {
                    self.lower_script(name, source);
                }
            }
            "remember" => {
                // the shape is already checked at parse time
                // (`check_remember`), so this cannot be reached with an
                // inner call that is not a setting.
                if let (Some(Arg::Word(w)), Some(Arg::Call(inner))) =
                    (call.args.first(), call.args.get(1))
                {
                    if let (Some(tier), Ok(setting)) = (Tier::of(w), Setting::of_call(inner)) {
                        self.nodes.push(Node::Emit(Op::Remember { tier, setting }));
                    }
                }
            }
            "help" => self.lower_help(call),
            _ => self.lower_plain(call),
        }
    }

    /// Inlines a `.shg` file: every line is parsed on its own and
    /// lowered into *this* program, so a script is run by the same
    /// stepper a typed line is and suspends on a search exactly the
    /// same way. One statement per line; a line that does not parse
    /// becomes a `Node::Fail` naming the file and the line, and the
    /// rest of the file carries on.
    ///
    /// The read already happened in `coerce`, so by here the contents
    /// are in hand; this only splits and lowers them.
    fn lower_script(&mut self, label: &str, source: &str) {
        if self.depth >= MAX_SCRIPT_DEPTH {
            self.nodes.push(Node::Fail(format!(
                "{label}: scripts nest more than {MAX_SCRIPT_DEPTH} deep; stopped here"
            )));
            return;
        }
        self.depth += 1;
        for (number, line) in source.lines().enumerate() {
            let number = number + 1;
            // `statements` strips the `#` comments and splits on `;`,
            // so a line may still hold several statements; what it may
            // not do is span lines, because continuing past a bad one
            // needs a line to blame.
            for (offset, text) in statements(line) {
                if let Err(e) = self.absorb(line, offset, text) {
                    self.nodes
                        .push(Node::Fail(format!("{label}:{number}: {e}")));
                }
            }
        }
        self.depth -= 1;
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
        // every target lowers the same way, `help` included: it reads
        // its lines from `help_lines()` rather than from the app, but
        // that is the host's business, not a second lowering.
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
        Node::Fail(text) => Some(format!("  (skipped) {text}")),
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
    /// A `.shg` script plus its contents, read while parsing so a
    /// missing file is a parse error the console can keep the line for,
    /// and so `dry(run(x))` has the file in hand to plan from.
    Script {
        name: String,
        source: String,
    },
    Pos(Pos),
    Offset(f64),
    Level(Level),
    Count(u32),
    Index(usize),
    Indices(Vec<usize>),
    Speed(SpeedArg),
    Duration(Option<f64>),
    Span(f64),
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
        Arg::Script { name, .. } => name.clone(),
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
        Arg::Span(s) => format!("{s}s"),
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
            "undo" => Op::Undo,
            "redo" => Op::Redo,
            "gap" => match first {
                Some(Arg::Duration(d)) => Op::SetGap(*d),
                _ => return None,
            },
            "silence" => match first {
                // rounded to whole seconds: the lavfi source takes an
                // integer and nobody queues a 1.5 second gap.
                Some(Arg::Span(s)) => Op::Silence {
                    secs: *s as u64,
                    front: self.keyword(Words::Position) == Some("front"),
                },
                _ => return None,
            },
            "speed" => match first {
                Some(Arg::Speed(SpeedArg::Set(s))) => Op::SetSpeed(*s),
                Some(Arg::Speed(SpeedArg::Up)) => Op::SpeedUp,
                Some(Arg::Speed(SpeedArg::Down)) => Op::SpeedDown,
                Some(Arg::Speed(SpeedArg::Reset)) => Op::SetSpeed(1.0),
                _ => return None,
            },
            // the name travels as typed and is resolved at apply
            // time against the live saved list, so a curve saved after
            // this line was typed still resolves and `dry` can still
            // print what was asked for.
            "eq" => match first {
                Some(Arg::Name(n)) => Op::SetEqPreset(n.clone()),
                _ => return None,
            },
            "crossfade" => Op::ToggleCrossfade,
            "search" => match first {
                Some(Arg::Name(q)) => Op::Search(q.clone()),
                _ => return None,
            },
            "clear" => Op::ClearConsole,
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
            "define" => match (first, self.args.get(1)) {
                (Some(Arg::Name(n)), Some(Arg::Terms(t))) => Op::Define {
                    name: n.clone(),
                    terms: t.clone(),
                },
                _ => return None,
            },
            "unset" => match first {
                Some(Arg::Name(n)) => Op::Unset(n.clone()),
                _ => return None,
            },
            "forget" => Op::Forget {
                tier: self.keyword(Words::Tier).and_then(Tier::of)?,
                setting: self.keyword(Words::Setting).and_then(SettingKind::of),
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

/// The one statement form that is not written as a call. Sugar for
/// `define`, desugared here so everything downstream only ever sees
/// calls; `dry(let x = [a])` prints the `define(x, a)` form, exactly as
/// a kind suffix prints its `kind=` form (R6).
const LET: &str = "let";

/// A statement: a call, or the `let` binding that desugars to one.
fn parse_statement(
    input: &str,
    offset: usize,
    text: &str,
    world: &World,
) -> Result<Call, ParseError> {
    match text
        .strip_prefix(LET)
        .filter(|rest| rest.starts_with(char::is_whitespace))
    {
        Some(rest) => parse_let(input, offset + LET.len(), rest, world),
        None => parse_call(input, offset, text, world),
    }
}

/// `let name = [a, b]`. Both halves go through `coerce` against
/// `define`'s own parameters, so the typechecking, the error messages
/// and the variable expansion are the ones the canonical call gets.
fn parse_let(input: &str, offset: usize, text: &str, world: &World) -> Result<Call, ParseError> {
    let lead = text.len() - text.trim_start().len();
    let (offset, body) = (offset + lead, text.trim());
    let Some(eq) = body.find('=') else {
        return Err(err(
            input,
            offset,
            body,
            "let needs an '=': write let name = [a, b]",
        ));
    };
    let command = lookup("define")
        .map(|(c, _)| c)
        .expect("define is in the table");
    let name = body[..eq].trim();
    let rest = &body[eq + 1..];
    let value_offset = offset + eq + 1 + (rest.len() - rest.trim_start().len());
    let value = rest.trim();
    if value.is_empty() {
        return Err(err(
            input,
            value_offset,
            "",
            missing(command, &command.params[1]),
        ));
    }
    let args = vec![
        coerce(input, command, &command.params[0], offset, name, world)?,
        coerce(
            input,
            command,
            &command.params[1],
            value_offset,
            value,
            world,
        )?,
    ];
    Ok(Call {
        command,
        args,
        kind: None,
    })
}

/// `name(args...)`. `input`/`offset` are only carried for error
/// columns; `text` is the already-trimmed statement or nested call.
fn parse_call(input: &str, offset: usize, text: &str, world: &World) -> Result<Call, ParseError> {
    let Some(lp) = text.find('(') else {
        let name = text.split_whitespace().next().unwrap_or(text);
        let message = if lookup(name).is_some() {
            format!("{name} is a command: write {name}()")
        } else {
            unknown_name(name)
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
    let (command, kind) =
        lookup(name).ok_or_else(|| err(input, offset, name, unknown_name(name)))?;

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

    let args = bind(input, command, &positional, world)?;
    if command.name == "view" {
        check_view(input, &args, &positional)?;
    }
    if command.name == "remember" {
        check_remember(input, &args, &positional)?;
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

/// `remember`'s inner call has to be one the host can actually settle
/// on later: a named eq curve, an absolute volume, a fixed speed.
/// Checked here rather than in `bind`, which only ever validates one
/// parameter against its own type and cannot see that a `Call` is the
/// wrong *kind* of call.
fn check_remember(
    input: &str,
    args: &[Arg],
    positional: &[(usize, &str)],
) -> Result<(), ParseError> {
    let Some(Arg::Call(inner)) = args.get(1) else {
        return Ok(());
    };
    match Setting::of_call(inner) {
        Ok(_) => Ok(()),
        Err(why) => Err(err(
            input,
            positional.get(1).map(|(o, _)| *o).unwrap_or(0),
            positional.get(1).map(|(_, t)| *t).unwrap_or(""),
            why,
        )),
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
    world: &World,
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
        match coerce(input, command, param, *off, text, world) {
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
    world: &World,
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
            // the one place variables expand, which is why every
            // terms-taking command gets them with no code of its own and
            // why `dry()` reports the expanded plan.
            Ok(Arg::Terms(world.scope.expand(&items)))
        }
        Ty::VarName => {
            let name = unquote(text);
            check_var_name(name).map_err(|why| bad(format!("{}: {why}", command.name)))?;
            Ok(Arg::Name(name.to_string()))
        }
        // every free-text type parses identically; they differ only in
        // what they complete from (see `Ty::completes`).
        // read here rather than at lowering time so a name that
        // resolves to nothing is a parse error, which keeps the line in
        // the console for you to fix.
        Ty::ScriptName => {
            let name = unquote(text);
            let source = world.read_script(name).map_err(bad)?;
            Ok(Arg::Script {
                name: name.to_string(),
                source,
            })
        }
        Ty::Query | Ty::NewName | Ty::SavedName | Ty::EqName | Ty::CommandName => {
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
        Ty::Span => match parse_time(text) {
            // zero is refused here rather than at apply time: a
            // silence of no length is a typo, not an instruction.
            Ok((None, Pos::Seconds(s))) if s > 0.0 => Ok(Arg::Span(s)),
            Ok((None, Pos::Seconds(_))) => Err(bad("a silence needs a length".into())),
            Ok(_) => Err(expected()),
            Err(why) => Err(bad(format!("{}: {why}", command.name))),
        },
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
        Ty::Words(set) => match set.accepts().iter().copied().find(|o| *o == text) {
            Some(word) => Ok(Arg::Word(word)),
            None => Err(bad(match closest(text, set.options()) {
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
        Ty::Call | Ty::SettingCall => {
            if !text.contains('(') {
                return Err(expected());
            }
            parse_call(input, off, text, world).map(|c| Arg::Call(Box::new(c)))
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
/// Completion runs on every keystroke, so a candidate list that the
/// *user* or the corpus can grow is capped rather than however many a
/// wide query matches.
///
/// The lists the command table itself bounds (command names, a keyword
/// set) are deliberately **not** capped: there are more than 50 written
/// command names, so capping them made every name alphabetically past
/// "random" unreachable by tab from an empty token, `view` and `volume`
/// included. The overlay already ellipsis-truncates the row it draws
/// them in, so a long list costs nothing on screen.
const MAX_CANDIDATES: usize = 50;

/// What the console may ask the host about the world, behind a trait so
/// the console itself stays pure: tests supply a fake, the host supplies
/// the real corpus. Mostly completion, which is why it is named for
/// that; `scope` is the one member a submitted line reads rather than a
/// keystroke.
///
/// Every completion method is called per keystroke and must therefore be
/// local only. `corpus` must never trigger a remote search: typing would
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

    /// Names of the user's saved eq curves, which `eq()` can take
    /// alongside the built-ins.
    fn eq_presets(&self) -> Vec<String> {
        Vec::new()
    }

    /// The variables a submitted line is parsed against, and what the
    /// variable-name parameters complete from. Borrowed rather than
    /// cloned because completion reads it per keystroke.
    fn scope(&self) -> &Scope {
        empty_scope()
    }

    /// Names of the `.shg` scripts `run(...)` could take. Snapshotted
    /// by the host, since this is a directory read and completion runs
    /// per keystroke.
    fn scripts(&self) -> Vec<String> {
        Vec::new()
    }

    /// Reads one script for `run(...)`, at submit time rather than per
    /// keystroke. `Err` is a ready-to-print reason.
    fn read_script(&self, name: &str) -> Result<String, String> {
        Err(format!("no scripts are available here ('{name}')"))
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
            KeyCode::Enter => return self.submit(cand),
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

    /// Takes the provider for the same reason `handle_key` does: a
    /// submitted line is parsed against the live variables, so the scope
    /// has to come from the host rather than from `Console`.
    fn submit(&mut self, cand: &dyn Candidates) -> ConsoleAction {
        self.cycle = None;
        self.suggestion = Suggestion::default();
        let line = self.input.trim().to_string();
        if line.is_empty() {
            return ConsoleAction::Close;
        }
        self.push(LineKind::Command, format!(": {line}"));
        let read = |name: &str| cand.read_script(name);
        let world = World {
            scope: cand.scope(),
            read: Some(&read),
        };
        match Program::parse_in(&line, &world) {
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
        let mut seen = match here.completes {
            Completes::Hint(_) => return Vec::new(),
            // uncapped: bounded by the table, not by the library.
            Completes::Words(set) => {
                keep_extensions(&here.token, set.options().iter().copied(), usize::MAX)
            }
            Completes::CommandNames => {
                keep_extensions(&here.token, every_name().iter().copied(), usize::MAX)
            }
            // fuzzy discovery, so a subsequence or typo match surfaces
            // things a prefix scan would miss; the prefix filter below
            // then drops whatever could not be inserted without
            // overwriting what was typed.
            Completes::CorpusTerms => {
                let corpus = cand.corpus(&here.token);
                keep_extensions(
                    &here.token,
                    cand.scope()
                        .names()
                        .into_iter()
                        .chain(corpus.iter().map(String::as_str)),
                    MAX_CANDIDATES,
                )
            }
            Completes::Variables => keep_extensions(
                &here.token,
                cand.scope().names().into_iter(),
                MAX_CANDIDATES,
            ),
            Completes::Scripts => keep_extensions(
                &here.token,
                cand.scripts().iter().map(String::as_str),
                MAX_CANDIDATES,
            ),
            Completes::SettingNames => keep_extensions(
                &here.token,
                Words::Setting.options().iter().copied(),
                usize::MAX,
            ),
            Completes::SavedQueues => keep_extensions(
                &here.token,
                cand.saved_queues().iter().map(String::as_str),
                MAX_CANDIDATES,
            ),
            Completes::EqPresets => {
                let saved = cand.eq_presets();
                keep_extensions(
                    &here.token,
                    Words::EqPreset
                        .options()
                        .iter()
                        .copied()
                        .chain(saved.iter().map(String::as_str)),
                    MAX_CANDIDATES,
                )
            }
        };
        self.ranks.order(&mut seen);
        seen
    }

    /// The best past command line the whole input is a prefix of, as
    /// zsh-autosuggestions does it.
    fn history_extension(&self) -> Option<String> {
        // newest first, so recency is the order anything unranked
        // falls back to.
        // uncapped on purpose: the ranker picks the winner out of the
        // whole matching history, not out of the newest `n` of it.
        let mut hits = keep_extensions(
            &self.input,
            self.history.iter().rev().map(String::as_str),
            usize::MAX,
        );
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
            // a setting name is a command name too, so both classes
            // land the cursor inside the parens they need.
            if matches!(
                before.completes,
                Completes::CommandNames | Completes::SettingNames
            ) && !self.input[self.cursor..].starts_with('(')
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
                Some(param) => (param.ty.completes(), Some((signature_of(command), arg))),
                // past the last parameter: nothing left to offer, but
                // still show the signature so the arity is visible.
                None => (
                    Completes::Hint("no more arguments"),
                    Some((signature_of(command), arg)),
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
///
/// Borrowed, and on an ascii token (every real one) decided on bytes
/// with nothing allocated: this runs once per candidate per keystroke.
fn extension<'a>(token: &str, candidate: &'a str) -> Option<&'a str> {
    if token.is_ascii() {
        let keep = token.len();
        let bytes = candidate.as_bytes();
        // a candidate of at most `keep` bytes has at most `keep` chars,
        // so it is either too short or has no remainder to offer.
        if bytes.len() <= keep {
            return None;
        }
        // all-ascii up to `keep` makes `keep` a char boundary, and
        // makes the ascii comparison the same answer `to_lowercase`
        // would have given on both sides.
        if bytes[..keep].is_ascii() {
            return candidate[..keep]
                .eq_ignore_ascii_case(token)
                .then(|| &candidate[keep..]);
        }
    }
    let keep = token.chars().count();
    // `nth(keep)` landing means the head is exactly `keep` chars and
    // the remainder is not empty; missing means one or the other fails.
    let (at, _) = candidate.char_indices().nth(keep)?;
    (candidate[..at].to_lowercase() == token.to_lowercase()).then(|| &candidate[at..])
}

fn extends(token: &str, candidate: &str) -> bool {
    extension(token, candidate).is_some()
}

/// The candidates that extend `token`, in arrival order, deduped, up to
/// `cap`. Only a kept candidate is ever allocated, which is the whole
/// point: the lists coming in are static names or a corpus page, and
/// most of a long one is dropped by the prefix filter.
fn keep_extensions<'a>(
    token: &str,
    from: impl Iterator<Item = &'a str>,
    cap: usize,
) -> Vec<String> {
    let mut kept: Vec<String> = Vec::new();
    for candidate in from {
        if !extends(token, candidate) || kept.iter().any(|k| k == candidate) {
            continue;
        }
        kept.push(candidate.to_string());
        if kept.len() >= cap {
            break;
        }
    }
    kept
}

fn remainder(token: &str, candidate: &str) -> String {
    extension(token, candidate).unwrap_or_default().to_string()
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
pub(crate) fn state_path(file: &str) -> Option<PathBuf> {
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
    use std::time::Instant;

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

    // --------------------------------------------------- variables

    fn scope_of(pairs: &[(&str, &[&str])]) -> Scope {
        let mut scope = Scope::default();
        for (name, terms) in pairs {
            scope.define(name, terms.iter().map(|t| (*t).to_string()).collect());
        }
        scope
    }

    /// A parse context over `scope` and a fake set of scripts, so the
    /// language's own tests never touch the disk.
    fn world_of<'a>(scope: &'a Scope, read: ScriptReader<'a>) -> World<'a> {
        World {
            scope,
            read: Some(read),
        }
    }

    /// Scripts by name, for the `run(...)` tests.
    fn scripts_of(files: &[(&str, &str)]) -> impl Fn(&str) -> Result<String, String> {
        let files: Vec<(String, String)> = files
            .iter()
            .map(|(n, b)| ((*n).to_string(), (*b).to_string()))
            .collect();
        move |name: &str| {
            files
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, b)| b.clone())
                .ok_or_else(|| format!("no script '{name}'"))
        }
    }

    fn no_scripts(name: &str) -> Result<String, String> {
        Err(format!("no script '{name}'"))
    }

    /// The search terms a program lowered to, parsed against `scope`.
    fn terms_in(input: &str, scope: &Scope) -> Vec<String> {
        Program::parse_in(input, &world_of(scope, &no_scripts))
            .expect("should parse")
            .nodes
            .iter()
            .filter_map(|n| match n {
                Node::Search(q) => Some(q.clone()),
                _ => None,
            })
            .collect()
    }

    fn ops_in(input: &str, scope: &Scope) -> Vec<Op> {
        ops_of(&Program::parse_in(input, &world_of(scope, &no_scripts)).expect("should parse"))
    }

    /// Every op a program emits, stepping past the reports. `Warn` is
    /// a script's bad line, which by design does not stop the program.
    fn ops_of(program: &Program) -> Vec<Op> {
        let mut program = program.clone();
        let mut env = Env {
            results: vec![Kind::Song],
            ..SETTLED.clone()
        };
        let mut ops = Vec::new();
        for _ in 0..1024 {
            match program.step(&env) {
                Step::Emit(Op::Search(q)) => {
                    env.query = q.clone();
                    ops.push(Op::Search(q));
                }
                Step::Emit(op) => ops.push(op),
                Step::Note(_) | Step::Warn(_) => {}
                Step::Done => break,
                other => panic!("unexpected step {other:?}"),
            }
        }
        ops
    }

    fn notes_in(input: &str, scope: &Scope) -> Vec<String> {
        reports_of(&Program::parse_in(input, &world_of(scope, &no_scripts)).expect("should parse"))
            .into_iter()
            .map(|(_, text)| text)
            .collect()
    }

    /// Every reported line a program produces, tagged with whether it
    /// came out as a note or as an error (`Step::Warn`).
    fn reports_of(program: &Program) -> Vec<(bool, String)> {
        let mut program = program.clone();
        let mut env = Env {
            results: vec![Kind::Song],
            ..SETTLED.clone()
        };
        let mut out = Vec::new();
        for _ in 0..4096 {
            match program.step(&env) {
                Step::Note(text) => out.push((false, text)),
                Step::Warn(text) => out.push((true, text)),
                Step::Emit(Op::Search(q)) => env.query = q,
                Step::Emit(_) => {}
                Step::Done | Step::Failed(_) => break,
                Step::AwaitResults { .. } => break,
            }
        }
        out
    }

    fn error_in(input: &str, scope: &Scope) -> ParseError {
        Program::parse_in(input, &world_of(scope, &no_scripts)).expect_err("should not parse")
    }

    /// The maintainer's own example, end to end.
    #[test]
    fn a_variable_expands_to_its_terms_wherever_terms_are_accepted() {
        let scope = scope_of(&[("bladee_classics", &["crest", "icedancer", "gods"])]);
        assert_eq!(
            terms_in("play(bladee_classics)", &scope),
            vec!["crest", "icedancer", "gods"]
        );
        assert_eq!(
            terms_in("queue(bladee_classics)", &scope),
            vec!["crest", "icedancer", "gods"]
        );
        // and it composes with everything else a terms slot takes:
        // inside a list, and with a kind suffix.
        assert_eq!(
            terms_in("queue([hex, bladee_classics])", &scope),
            vec!["hex", "crest", "icedancer", "gods"]
        );
        assert_eq!(
            terms_in("star_aa(bladee_classics)", &scope),
            vec!["crest", "icedancer", "gods"]
        );
        // play keeps its own rule: the first term plays, the rest queue.
        let ops = ops_in("play(bladee_classics)", &scope);
        assert_eq!(
            ops.iter().filter(|o| matches!(o, Op::Play(_))).count(),
            1,
            "{ops:?}"
        );
        assert_eq!(
            ops.iter()
                .filter(|o| matches!(o, Op::Enqueue { .. }))
                .count(),
            2,
            "{ops:?}"
        );
    }

    /// The guard for the whole feature: a name is not a namespace, so
    /// anything undefined is still a plain search term. Reverting the
    /// expansion makes the first assertion pass and the second fail.
    #[test]
    fn an_undefined_name_is_still_a_plain_search_term() {
        let empty = Scope::default();
        assert_eq!(
            terms_in("play(bladee_classics)", &empty),
            vec!["bladee_classics"]
        );
        let scope = scope_of(&[("bladee_classics", &["crest"])]);
        assert_eq!(terms_in("play(bladee_classics)", &scope), vec!["crest"]);
        // an unrelated term is untouched either way.
        assert_eq!(terms_in("play(hex)", &scope), vec!["hex"]);
    }

    #[test]
    fn let_is_sugar_for_define_and_dry_prints_the_desugared_form() {
        let empty = Scope::default();
        let want = vec![Op::Define {
            name: "bladee_classics".to_string(),
            terms: vec!["crest".into(), "icedancer".into(), "gods".into()],
        }];
        assert_eq!(
            ops_in("let bladee_classics = [crest, icedancer, gods]", &empty),
            want
        );
        assert_eq!(
            ops_in("define(bladee_classics, [crest, icedancer, gods])", &empty),
            want
        );
        // `let` is statement-level, so `dry` (which takes a call) takes
        // the canonical spelling; its report is the same either way.
        let plan = notes_in("dry(define(mix, [hex]))", &empty);
        assert_eq!(plan[0], "would run: define(mix, hex)");
        assert!(
            plan.iter().any(|l| l.contains("let mix = [hex]")),
            "{plan:?}"
        );
        assert!(ops_in("dry(define(mix, [hex]))", &empty).is_empty());
        assert!(Program::parse_in("dry(let mix = [hex])", &World::bare()).is_err());
    }

    #[test]
    fn a_single_term_needs_no_brackets_in_a_binding() {
        let empty = Scope::default();
        assert_eq!(
            ops_in("let mix = hex", &empty),
            vec![Op::Define {
                name: "mix".to_string(),
                terms: vec!["hex".into()],
            }]
        );
    }

    #[test]
    fn a_binding_reports_where_it_broke() {
        let empty = Scope::default();
        assert!(error_in("let mix", &empty).message.contains("needs an '='"));
        assert!(error_in("let mix =", &empty)
            .message
            .contains("missing terms"));
        assert!(error_in("let  = [hex]", &empty)
            .message
            .contains("needs a name"));
        // and `let` written as a call points at the binding form rather
        // than at whatever command name happens to be nearest.
        assert!(error_in("let(mix, [hex])", &empty)
            .message
            .contains("let is not a call"));
    }

    #[test]
    fn a_variable_name_is_lowercase_snake_case_and_never_a_command() {
        let empty = Scope::default();
        for bad in ["4 star", "Mix", "my-mix", "_mix", "mix!"] {
            let e = error_in(&format!("let {bad} = [hex]"), &empty);
            assert!(
                e.message.contains("lowercase") || e.message.contains("start with"),
                "{bad}: {}",
                e.message
            );
        }
        // shadowing a command name would make the variable unusable,
        // since a bare `play` in a terms slot would expand instead.
        let e = error_in("let play = [hex]", &empty);
        assert!(e.message.contains("is a command name"), "{}", e.message);
        assert!(error_in("unset(play)", &empty)
            .message
            .contains("is a command name"));
    }

    /// Eager, which is why there is no recursion to bound and no cycle
    /// to detect: `let b = [a]` stores whatever `a` held at that moment.
    #[test]
    fn composing_variables_snapshots_the_components_when_defined() {
        let scope = scope_of(&[("a", &["crest", "hex"])]);
        assert_eq!(
            ops_in("let b = [a, gods]", &scope),
            vec![Op::Define {
                name: "b".to_string(),
                terms: vec!["crest".into(), "hex".into(), "gods".into()],
            }]
        );
    }

    #[test]
    fn unset_names_the_variable_it_drops() {
        let scope = scope_of(&[("mix", &["hex"])]);
        assert_eq!(
            ops_in("unset(mix)", &scope),
            vec![Op::Unset("mix".to_string())]
        );
        assert_eq!(Op::Unset("mix".to_string()).describe(), "unset mix");
    }

    /// The listing is `let` lines, so `view(vars)` output is also
    /// something you can paste back in or into a `.shg` file.
    #[test]
    fn the_variable_listing_is_itself_valid_input() {
        let scope = scope_of(&[
            ("bladee_classics", &["crest", "gods"]),
            ("quiet", &["mazzy star"]),
        ]);
        let lines = scope.lines();
        assert_eq!(
            lines,
            vec![
                "let bladee_classics = [crest, gods]".to_string(),
                "let quiet = [mazzy star]".to_string(),
            ]
        );
        let empty = Scope::default();
        for line in &lines {
            assert!(
                Program::parse_in(line, &world_of(&empty, &no_scripts)).is_ok(),
                "the listing printed something that does not parse: {line}"
            );
        }
        assert_eq!(
            Scope::default().lines().len(),
            1,
            "an empty scope still says so rather than printing nothing"
        );
    }

    #[test]
    fn variable_names_complete_in_a_terms_slot_and_in_their_own() {
        let mut fake = Fake::new(&["bladee bootleg"]);
        fake.vars = scope_of(&[("bladee_classics", &["crest"])]);
        // ahead of the corpus: naming a variable is the more specific
        // intent, and both are things a terms slot takes.
        let c = console_with(&fake, "play(bla");
        let here = site(&c.input, c.cursor);
        assert_eq!(
            c.candidates_at(&here, &fake),
            vec!["bladee_classics".to_string(), "bladee bootleg".to_string()]
        );
        // a variable-name parameter offers only variables.
        let c = console_with(&fake, "unset(bla");
        let here = site(&c.input, c.cursor);
        assert_eq!(here.completes, Completes::Variables);
        assert_eq!(
            c.candidates_at(&here, &fake),
            vec!["bladee_classics".to_string()]
        );
    }

    #[test]
    fn a_submitted_line_is_parsed_against_the_live_variables() {
        let mut fake = Fake::new(&[]);
        fake.vars = scope_of(&[("mix", &["crest", "hex"])]);
        let mut c = console_with(&fake, "queue(mix)");
        match press(&mut c, &fake, KeyCode::Enter) {
            ConsoleAction::Run(program) => {
                let searched: Vec<String> = program
                    .nodes
                    .iter()
                    .filter_map(|n| match n {
                        Node::Search(q) => Some(q.clone()),
                        _ => None,
                    })
                    .collect();
                assert_eq!(searched, vec!["crest", "hex"]);
            }
            other => panic!("expected a program, got {other:?}"),
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
        // one script in hand, since `run`'s own sample argument names
        // one and the read happens while parsing.
        let empty = Scope::default();
        let read = scripts_of(&[("sample", "pause()")]);
        let world = world_of(&empty, &read);
        for c in COMMANDS {
            let input = sample_call(c);
            let program = Program::parse_in(&input, &world)
                .unwrap_or_else(|e| panic!("{input} did not parse: {e}"));
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
            Ty::Query | Ty::NewName | Ty::SavedName | Ty::EqName => "crest".into(),
            Ty::VarName => "mix".into(),
            Ty::Span => "15m".into(),
            // the samples never touch the disk, so this one is a path
            // that `scripts_of` in the `run` tests resolves.
            Ty::ScriptName => "sample".into(),
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
            Ty::SettingCall => "speed(1.5)".into(),
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

    /// `view(help)` is an op like every other target (it used to lower
    /// to notes, which made `ViewTarget::Help` unconstructable and the
    /// host's arm for it dead code). The listing itself still comes from
    /// `help_lines()`, which is what keeps it identical to `help()`.
    #[test]
    fn view_help_is_an_op_like_every_other_target() {
        assert_eq!(ops("view(help)"), vec![Op::View(ViewTarget::Help)]);
        assert!(notes("view(help)").is_empty());
        assert_eq!(notes("help()"), help_lines());
        assert_eq!(help_lines().len(), COMMANDS.len());
        // and `dry` reports the plan rather than dumping the listing,
        // which is what it does for every other view target.
        assert_eq!(notes("dry(view(help))").len(), 2);
    }

    /// `help` is a listing, so the console stays open and the lines land
    /// in its scrollback; it is not a screen that would cover the
    /// console it was typed into.
    #[test]
    fn view_help_is_a_listing_not_a_screen() {
        assert!(!ViewTarget::Help.is_screen());
        assert!(!Op::View(ViewTarget::Help).closes_console());
    }

    /// No `ViewTarget` may be unreachable: `view(help)` lowering to
    /// notes instead of an op is exactly how `Help` became a variant no
    /// parser could construct, leaving every host with an arm for an op
    /// that never arrives. The match is exhaustive, so a new target has
    /// to name its spelling here, and the count check makes it have one
    /// in `VIEW_NAMES`.
    #[test]
    fn every_view_target_is_constructible_by_the_parser() {
        const ALL: &[ViewTarget] = &[
            ViewTarget::Queue,
            ViewTarget::Now,
            ViewTarget::Suggestions,
            ViewTarget::Settings,
            ViewTarget::Keybinds,
            ViewTarget::Lyrics,
            ViewTarget::Starred,
            ViewTarget::Index,
            ViewTarget::History,
            ViewTarget::Playlists,
            ViewTarget::Vars,
            ViewTarget::Bindings,
            ViewTarget::Eq,
            ViewTarget::Undo,
            ViewTarget::Help,
        ];
        fn spelling(t: ViewTarget) -> &'static str {
            match t {
                ViewTarget::Queue => "queue",
                ViewTarget::Now => "now",
                ViewTarget::Suggestions => "suggestions",
                ViewTarget::Settings => "settings",
                ViewTarget::Keybinds => "keybinds",
                ViewTarget::Lyrics => "lyrics",
                ViewTarget::Starred => "starred",
                ViewTarget::Index => "index",
                ViewTarget::History => "history",
                ViewTarget::Playlists => "playlists",
                ViewTarget::Vars => "vars",
                ViewTarget::Bindings => "bindings",
                ViewTarget::Eq => "eq",
                ViewTarget::Undo => "undo",
                ViewTarget::Help => "help",
            }
        }
        assert_eq!(
            ALL.len(),
            VIEW_NAMES.iter().filter(|v| v.canonical).count(),
            "a target is missing from ALL or from VIEW_NAMES"
        );
        for t in ALL {
            assert_eq!(view_name(*t), spelling(*t));
            assert_eq!(
                ops(&format!("view({})", spelling(*t))),
                vec![Op::View(*t)],
                "view({})",
                spelling(*t)
            );
        }
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
        // the name travels as typed now; resolution happens at apply
        // time, which is what lets a saved curve be named here at all.
        assert_eq!(ops("eq(rock)"), vec![Op::SetEqPreset("rock".into())]);
        assert_eq!(ops("eq(flat)"), vec![Op::SetEqPreset("flat".into())]);
        // the point of the change: a name that is not a built-in still
        // lowers, instead of failing to parse.
        assert_eq!(
            ops("eq(my bass curve)"),
            vec![Op::SetEqPreset("my bass curve".into())]
        );
        assert_eq!(ops("view(eq)"), vec![Op::View(ViewTarget::Eq)]);
        assert_eq!(ops("view(equalizer)"), vec![Op::View(ViewTarget::Eq)]);
        // a missing argument is still a parse error; the table says
        // the parameter is required and that does not need the saved
        // list to decide.
        let e = error("eq()");
        assert!(e.message.contains("missing preset"), "{}", e.message);
        // an *unknown* name is no longer a parse error: the parser
        // cannot see the user's saved curves, so "is this a real
        // preset" is answered at apply time. the message quality that
        // used to live here (listing the names, "did you mean") moved
        // with it, to `eq::resolve_failure`, which has its own tests.
        assert_eq!(ops("eq(disco)"), vec![Op::SetEqPreset("disco".into())]);
    }

    // ------------------------------------------------------------ view

    /// Every spelling in the table resolves, including the plural and
    /// singular forms the maintainer asked for.
    #[test]
    fn every_view_spelling_resolves_to_its_target() {
        for v in VIEW_NAMES {
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
            // the two documented exceptions, both because the terms are
            // not an operand. view's belong to its `q` form, and `q`
            // means "search for this", which has no selection reading;
            // define *stores* terms rather than acting on them, and the
            // selection is a resolved row, not a term.
            if c.name == "view" || c.name == "define" {
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

    /// The half of the index contract the language owns (see
    /// docs/shellsong.md, "the index contract"): an index is computed
    /// from the env of the step that emits it and never kept, so a
    /// results list rebuilt mid-program is resolved against afresh.
    #[test]
    fn a_resolved_index_comes_from_the_env_of_its_own_step() {
        let mut program = Program::parse("play(crest); queue(hex)").unwrap();
        let mut env = Env {
            results: vec![Kind::Album, Kind::Song],
            ..SETTLED.clone()
        };
        assert_eq!(
            program.step(&env),
            Step::Emit(Op::Search("crest".to_string()))
        );
        env.query = "crest".to_string();
        // play prefers songs, so the song at 1 wins over the album.
        assert_eq!(program.step(&env), Step::Emit(Op::Play(Target::Index(1))));
        assert_eq!(
            program.step(&env),
            Step::Emit(Op::Search("hex".to_string()))
        );
        // the host rebuilt the list under the program, which is what
        // `resync_results` does; index 1 is now the album, so a cached
        // resolution would queue the wrong row.
        env.query = "hex".to_string();
        env.results = vec![Kind::Song, Kind::Album];
        assert_eq!(program.step(&env), Step::Emit(queued(0)));
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
        assert_eq!(
            ops("silence(15m)"),
            vec![Op::Silence {
                secs: 900,
                front: false
            }]
        );
        assert_eq!(
            ops("silence(15m, front)"),
            vec![Op::Silence {
                secs: 900,
                front: true
            }]
        );
        assert_eq!(
            ops("silence(1h30m)"),
            vec![Op::Silence {
                secs: 5400,
                front: false
            }]
        );
        // the reason `silence` has its own `Ty::Span` instead of
        // sharing `sleep`'s `Ty::Duration`: `off` would have parsed
        // and then done nothing whatsoever.
        assert!(!error("silence(off)").message.is_empty());
        assert!(error("silence(0s)").message.contains("needs a length"));
        // `wait` is the same command: the alias resolves before
        // lowering, so there is no second arm to keep in step.
        assert_eq!(ops("wait(15s)"), ops("silence(15s)"));
        assert_eq!(
            ops("wait(15s, front)"),
            vec![Op::Silence {
                secs: 900 / 60,
                front: true
            }]
        );
        // `gap` is the contrast: it takes `Ty::Duration`, where `off`
        // is meaningful, because turning the gap off is a thing you
        // want to say. `silence(off)` is not.
        assert_eq!(ops("gap(10s)"), vec![Op::SetGap(Some(10.0))]);
        assert_eq!(ops("gap(off)"), vec![Op::SetGap(None)]);
        assert_eq!(ops("gap(1m30s)"), vec![Op::SetGap(Some(90.0))]);
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
        let empty = Scope::default();
        let read = scripts_of(&[("sample", "pause()")]);
        let world = world_of(&empty, &read);
        for c in COMMANDS {
            let program = Program::parse_in(&sample_call(c), &world).expect("should parse");
            for op in ops_of(&program) {
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
        assert!(!Op::View(ViewTarget::Help).closes_console());
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
            Ty::EqName,
            Ty::VarName,
            Ty::ScriptName,
            Ty::CommandName,
            Ty::Pos,
            Ty::Offset,
            Ty::Level,
            Ty::Count,
            Ty::Index,
            Ty::Indices,
            Ty::Speed,
            Ty::Duration,
            Ty::Span,
            Ty::Call,
            Ty::SettingCall,
            Ty::Words(Words::Position),
            Ty::Words(Words::LoopMode),
            Ty::Words(Words::Switch),
            Ty::Words(Words::EqPreset),
            Ty::Words(Words::ViewTarget),
            Ty::Words(Words::FadeDir),
            Ty::Words(Words::Tier),
            Ty::Words(Words::Setting),
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

    /// The command-name list is bounded by the table, not by the
    /// library, so capping it only ever hides real commands: with more
    /// than `MAX_CANDIDATES` written names, every one alphabetically
    /// past the cut became unreachable by tab from an empty token,
    /// `view` and `volume` among them.
    #[test]
    fn every_command_name_is_reachable_by_tab_from_an_empty_token() {
        let fake = Fake::new(&[]);
        let c = Console::empty();
        let here = site("", 0);
        let offered = c.candidates_at(&here, &fake);
        assert!(
            every_name().len() > MAX_CANDIDATES,
            "the table no longer exceeds the cap, so this test is about nothing"
        );
        for name in every_name() {
            assert!(
                offered.contains(&(*name).to_string()),
                "'{name}' cannot be reached by tab"
            );
        }
        // the corpus is the half that still has to be capped: it is as
        // long as the library.
        let wide = Fake::new(&["flatline"]);
        let here = site("play(", 5);
        assert!(c.candidates_at(&here, &wide).len() <= MAX_CANDIDATES);
    }

    #[test]
    fn a_keyword_set_only_ever_offers_something_that_parses() {
        for c in COMMANDS {
            for (i, p) in c.params.iter().enumerate() {
                let Completes::Words(set) = p.ty.completes() else {
                    continue;
                };
                for option in set.options().iter().copied() {
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
                    // whatever is still required after it, or the call
                    // would be short an argument rather than wrong
                    // about the keyword, which is not what this pins.
                    for p in c.params.iter().skip(i + 1).filter(|p| p.required) {
                        args.push(sample_arg(p.ty));
                    }
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
        vars: Scope,
    }

    impl Fake {
        fn new(names: &[&'static str]) -> Fake {
            Fake {
                names: names.to_vec(),
                queues: Vec::new(),
                vars: Scope::default(),
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
        fn scope(&self) -> &Scope {
            &self.vars
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
            vars: Scope::default(),
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

    // ------------------------------------------------- the published api

    /// Calls every form docs/shellsong.md's "the api a host calls" block
    /// publishes, spelled exactly as the doc spells it, with each result
    /// bound to its documented type. A changed signature breaks this
    /// build rather than leaving the published list quietly wrong, which
    /// is how `paste(&str)` outlived the `cand` argument.
    #[test]
    fn the_published_api_is_callable_as_documented() {
        let fake = Fake::new(&["flatline"]);

        let _: KeyCode = OPEN_KEY;

        let mut console: Console = Console::empty();
        let _: ConsoleAction =
            console.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE), &fake);
        console.paste("ause()", &fake);
        console.note("a note");
        console.fail("a failure");
        console.push(LineKind::Command, ": pause()");
        let _: &[String] = console.history();
        let _: Option<&Cycle> = console.cycle();
        let _: &String = &console.input;
        let _: usize = console.cursor;
        let _: &Vec<Line> = &console.lines;
        let _: &Option<String> = &console.error;
        let _: &Suggestion = &console.suggestion;

        let mut program: Program = Program::parse("pause()").expect("should parse");
        let _: Step = program.step(&SETTLED);
        let _: bool = program.is_done();
        let _: ParseError = Program::parse("pasue()").expect_err("should not parse");

        let _: String = Op::TogglePause.describe();
        let _: bool = Op::TogglePause.closes_console();
        let _: bool = ViewTarget::Help.is_screen();
        let _: Vec<String> = help_lines();
        let _: Kind = Kind::of(&SearchItem::LocalVideo {
            path: PathBuf::from("/video/x.mkv"),
            title: "x".to_string(),
        });

        let _: Vec<String> = Candidates::corpus(&fake, "fl");
        let _: Vec<String> = Candidates::saved_queues(&fake);
        let _: &Scope = Candidates::scope(&fake);

        let scope = Scope::default();
        let _: World = World::bare();
        let _: Program = Program::parse_in("pause()", &World::bare()).expect("should parse");
        let _: Vec<&str> = scope.names();
        let _: Vec<String> = scope.lines();
    }

    #[test]
    fn clear_is_the_console_and_clear_queue_is_the_queue() {
        assert_eq!(ops("clear()"), vec![Op::ClearConsole]);
        assert_eq!(ops("cls()"), vec![Op::ClearConsole]);
        assert_eq!(ops("clear_queue()"), vec![Op::ClearQueue]);
        assert_eq!(ops("qclear()"), vec![Op::ClearQueue]);
        assert_eq!(ops("flush()"), vec![Op::ClearQueue]);
    }

    #[test]
    fn clear_no_longer_empties_the_queue() {
        // it used to be an alias of clear_queue; anyone typing clear()
        // expecting the terminal behaviour would have wiped the queue.
        assert_ne!(ops("clear()"), vec![Op::ClearQueue]);
    }

    // -------------------------------------------- bound settings

    #[test]
    fn remember_takes_the_setting_as_a_call_so_there_is_no_second_vocabulary() {
        let empty = Scope::default();
        assert_eq!(
            ops_in("remember(track, eq(night))", &empty),
            vec![Op::Remember {
                tier: Tier::Track,
                setting: Setting::Eq("night".to_string()),
            }]
        );
        assert_eq!(
            ops_in("remember(album, volume(40%))", &empty),
            vec![Op::Remember {
                tier: Tier::Album,
                setting: Setting::Volume(40.0),
            }]
        );
        assert_eq!(
            ops_in("remember(artist, speed(1.1))", &empty),
            vec![Op::Remember {
                tier: Tier::Artist,
                setting: Setting::Speed(1.1),
            }]
        );
    }

    /// The value has to be one the host can settle on later, so a nudge
    /// and a step are both refused at parse time with the call they
    /// meant; otherwise `remember(track, volume(+10%))` would bind
    /// "louder than whatever it happens to be", which is not a setting.
    #[test]
    fn remember_refuses_a_relative_value_and_a_call_that_is_not_a_setting() {
        let empty = Scope::default();
        let e = error_in("remember(track, volume(+10%))", &empty);
        assert!(e.message.contains("volume(40%)"), "{}", e.message);
        let e = error_in("remember(track, speed(up))", &empty);
        assert!(e.message.contains("speed(1.1)"), "{}", e.message);
        for bad in ["skip()", "clear_queue()", "play(crest)", "quit()"] {
            let e = error_in(&format!("remember(track, {bad})"), &empty);
            assert!(e.message.contains("remember takes"), "{bad}: {}", e.message);
            assert!(e.message.contains("eq(...)"), "{bad}: {}", e.message);
        }
    }

    #[test]
    fn forget_drops_one_setting_or_all_of_them() {
        let empty = Scope::default();
        assert_eq!(
            ops_in("forget(track)", &empty),
            vec![Op::Forget {
                tier: Tier::Track,
                setting: None,
            }]
        );
        assert_eq!(
            ops_in("forget(album, eq)", &empty),
            vec![Op::Forget {
                tier: Tier::Album,
                setting: Some(SettingKind::Eq),
            }]
        );
        let e = error_in("forget(track, loudness)", &empty);
        assert!(e.message.contains("eq, volume, speed"), "{}", e.message);
    }

    #[test]
    fn a_remembered_setting_describes_itself_and_dry_plans_it() {
        let empty = Scope::default();
        assert_eq!(
            Op::Remember {
                tier: Tier::Track,
                setting: Setting::Eq("night".to_string()),
            }
            .describe(),
            "remembered eq night for this track"
        );
        let plan = notes_in("dry(remember(track, eq(night)))", &empty);
        assert_eq!(plan[0], "would run: remember(track, eq(night))");
        assert!(
            plan.iter().any(|l| l.contains("remembered eq night")),
            "{plan:?}"
        );
        assert!(ops_in("dry(remember(track, eq(night)))", &empty).is_empty());
    }

    /// The op a binding applies is the op the typed command lowers to,
    /// which is what keeps a per-track eq from drifting from `eq(x)`.
    #[test]
    fn a_settings_op_is_the_same_op_the_typed_command_lowers_to() {
        let empty = Scope::default();
        assert_eq!(
            Setting::Eq("night".to_string()).op(),
            ops_in("eq(night)", &empty)[0]
        );
        assert_eq!(Setting::Volume(40.0).op(), ops_in("volume(40%)", &empty)[0]);
        assert_eq!(Setting::Speed(1.1).op(), ops_in("speed(1.1)", &empty)[0]);
    }

    /// Synthesised ops go through the same stepper, so the host has one
    /// dispatch point rather than a second table for its own work.
    #[test]
    fn a_host_built_program_steps_like_any_other() {
        let mut program = Program::of(vec![Op::SetSpeed(1.1), Op::Mute(true)]);
        assert_eq!(program.step(&SETTLED), Step::Emit(Op::SetSpeed(1.1)));
        assert_eq!(program.step(&SETTLED), Step::Emit(Op::Mute(true)));
        assert_eq!(program.step(&SETTLED), Step::Done);
        assert!(program.is_done());
        assert!(Program::of(Vec::new()).is_done());
    }

    // ----------------------------------------------------- scripts

    const SCRIPT: &str = "\
# a morning routine
let morning = [mazzy star, boards of canada]

eq(flat)
volume(40%)
queue(morning)
";

    fn run_script(line: &str, files: &[(&str, &str)]) -> Program {
        let empty = Scope::default();
        let read = scripts_of(files);
        Program::parse_in(line, &world_of(&empty, &read)).expect("should parse")
    }

    /// A `.shg` file is lowered into the very same program a typed line
    /// becomes, which is the whole point: one interpreter, one stepper.
    #[test]
    fn a_script_lowers_into_the_same_program_its_lines_would() {
        let program = run_script("run(morning)", &[("morning", SCRIPT)]);
        let typed = Program::parse_in(
            "eq(flat); volume(40%); queue([mazzy star, boards of canada])",
            &World::bare(),
        )
        .expect("should parse");
        // the `let` line is the one extra statement; everything after
        // it is identical, expansion included.
        let script_ops = ops_of(&program);
        assert_eq!(
            script_ops.first(),
            Some(&Op::Define {
                name: "morning".to_string(),
                terms: vec!["mazzy star".into(), "boards of canada".into()],
            })
        );
        assert_eq!(&script_ops[1..], &ops_of(&typed)[..]);
    }

    /// A script that searches has to suspend and resume exactly as a
    /// typed line does, since it *is* one.
    #[test]
    fn a_script_that_searches_suspends_and_resumes_like_a_typed_line() {
        let mut program = run_script("run(s)", &[("s", "play(crest)\nskip()\n")]);
        let unsettled = Env {
            search_settled: false,
            results: vec![Kind::Song],
            ..SETTLED.clone()
        };
        assert_eq!(
            program.step(&unsettled),
            Step::Emit(Op::Search("crest".to_string()))
        );
        // blocked, and blocked again, until the host says it settled.
        assert_eq!(
            program.step(&Env {
                query: "crest".to_string(),
                ..unsettled.clone()
            }),
            Step::AwaitResults {
                query: "crest".to_string()
            }
        );
        let settled = Env {
            query: "crest".to_string(),
            results: vec![Kind::Song],
            ..SETTLED.clone()
        };
        assert_eq!(
            program.step(&settled),
            Step::Emit(Op::Play(Target::Index(0)))
        );
        assert_eq!(program.step(&settled), Step::Emit(Op::SkipNext));
    }

    /// The guard for the whole stage: a line that does not parse is
    /// reported with its file and line number *and* the rest of the
    /// file still runs. Reverting either half fails this.
    #[test]
    fn a_bad_line_names_the_file_and_the_rest_of_the_script_still_runs() {
        let program = run_script(
            "run(mix)",
            &[(
                "mix",
                "pause()\nxyzzy()\nmute()\nvolume(nonsense)\nskip()\n",
            )],
        );
        let reports = reports_of(&program);
        let failures: Vec<&String> = reports
            .iter()
            .filter(|(is_error, _)| *is_error)
            .map(|(_, t)| t)
            .collect();
        assert_eq!(failures.len(), 2, "{reports:?}");
        assert!(failures[0].starts_with("mix:2: "), "{}", failures[0]);
        assert!(failures[0].contains("xyzzy"), "{}", failures[0]);
        assert!(failures[1].starts_with("mix:4: "), "{}", failures[1]);
        // and every good line still ran, in order, past both errors.
        assert_eq!(
            ops_of(&program),
            vec![Op::TogglePause, Op::Mute(true), Op::SkipNext]
        );
    }

    /// `dry(run(x))` matters more here than anywhere else: a file can
    /// `clear_queue` or `download`, so you have to be able to read what
    /// it will do before it does it.
    #[test]
    fn dry_previews_a_whole_script_and_runs_none_of_it() {
        let program = run_script(
            "dry(run(danger))",
            &[("danger", "clear_queue()\ndownload(crest)\n")],
        );
        assert!(
            ops_of(&program).is_empty(),
            "dry(run(...)) must emit nothing at all"
        );
        let plan: Vec<String> = reports_of(&program).into_iter().map(|(_, t)| t).collect();
        assert_eq!(plan[0], "would run: run(danger)");
        assert!(plan.iter().any(|l| l.contains("queue cleared")), "{plan:?}");
        assert!(
            plan.iter().any(|l| l.contains("search 'crest'")),
            "{plan:?}"
        );
        assert!(
            plan.iter().any(|l| l.contains("downloading the best")),
            "{plan:?}"
        );
    }

    #[test]
    fn a_dry_script_still_shows_which_lines_it_would_skip() {
        let program = run_script("dry(run(s))", &[("s", "pause()\nxyzzy()\n")]);
        let plan: Vec<String> = reports_of(&program).into_iter().map(|(_, t)| t).collect();
        assert!(
            plan.iter().any(|l| l.contains("(skipped) s:2:")),
            "{plan:?}"
        );
    }

    #[test]
    fn a_script_that_is_not_there_is_a_parse_error_naming_it() {
        let empty = Scope::default();
        let read = scripts_of(&[("morning", "pause()")]);
        let e = Program::parse_in("run(evening)", &world_of(&empty, &read))
            .expect_err("should not parse");
        assert!(e.message.contains("evening"), "{}", e.message);
        // a parse error keeps the line in the console, which is the
        // point of reading the file while parsing rather than later.
        assert_eq!(e.token, "evening");
    }

    #[test]
    fn a_context_with_no_scripts_says_so_rather_than_guessing() {
        let e = Program::parse_in("run(anything)", &World::bare()).expect_err("should not parse");
        assert!(e.message.contains("no scripts"), "{}", e.message);
    }

    /// A script running another is useful; one reaching itself is not.
    #[test]
    fn script_nesting_is_bounded_rather_than_recursing_forever() {
        let program = run_script("run(loop)", &[("loop", "pause()\nrun(loop)\n")]);
        let reports = reports_of(&program);
        assert!(
            reports
                .iter()
                .any(|(is_error, t)| *is_error && t.contains("nest more than")),
            "{reports:?}"
        );
        // and it stopped at the bound rather than growing without one.
        assert_eq!(
            ops_of(&program).len(),
            MAX_SCRIPT_DEPTH,
            "one pause() per frame, then the bound"
        );
    }

    #[test]
    fn a_script_line_may_hold_several_statements_but_not_span_lines() {
        let program = run_script("run(s)", &[("s", "pause(); mute()\n")]);
        assert_eq!(ops_of(&program), vec![Op::TogglePause, Op::Mute(true)]);
        // a call split across lines cannot work in a file, because
        // continuing past a bad line needs a line to blame; it reports
        // rather than silently doing half.
        let program = run_script("run(s)", &[("s", "queue([hex,\ncrest])\n")]);
        let reports = reports_of(&program);
        assert!(reports.iter().any(|(is_error, _)| *is_error), "{reports:?}");
    }

    /// Expansion happens while parsing, so a binding made earlier in
    /// the same program has to be visible later in it; otherwise the
    /// most obvious `.shg` file there is (define a vibe, then play it)
    /// would quietly search for the variable's own name.
    #[test]
    fn a_binding_is_visible_to_everything_after_it_in_the_same_program() {
        let empty = Scope::default();
        assert_eq!(
            terms_in("let a = [crest, hex]; play(a)", &empty),
            vec!["crest", "hex"]
        );
        // and `unset` takes it back out again, so what the expansion
        // assumed matches what will actually be bound by then.
        assert_eq!(
            terms_in("let a = [crest]; unset(a); play(a)", &empty),
            vec!["a"]
        );
        let program = run_script("run(s)", &[("s", "let a = [crest]\nplay(a)\n")]);
        assert_eq!(
            ops_of(&program)
                .into_iter()
                .filter_map(|op| match op {
                    Op::Search(q) => Some(q),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            vec!["crest".to_string()]
        );
    }

    #[test]
    fn a_script_completes_from_the_hosts_shg_files() {
        struct WithScripts;
        impl Candidates for WithScripts {
            fn corpus(&self, _token: &str) -> Vec<String> {
                Vec::new()
            }
            fn scripts(&self) -> Vec<String> {
                vec![
                    "init".to_string(),
                    "morning".to_string(),
                    "mood".to_string(),
                ]
            }
        }
        let cand = WithScripts;
        let c = console_with(&cand, "run(mo");
        let here = site(&c.input, c.cursor);
        assert_eq!(here.completes, Completes::Scripts);
        assert_eq!(
            c.candidates_at(&here, &cand),
            vec!["morning".to_string(), "mood".to_string()]
        );
    }

    // --------------------------------------------- pinned completion

    /// The corpus the pinned table below completes against.
    /// Deliberately mixed case and full of non-prefix fuzzy hits, since
    /// `Fake::corpus` ignores the token and hands back the lot: the
    /// prefix filter is what has to earn every line of the table. The
    /// repeated names are there so the dedup is load-bearing too; two
    /// albums really can share a track title.
    const PINNED_CORPUS: &[&str] = &[
        "flatline",
        "Flatline (Remix)",
        "floor",
        "flatline",
        "sulfur surfer",
        "4 star crest",
        "crest",
        "crest",
        "hex",
        "Icedancer",
        "Eversince",
    ];

    const PINNED_QUEUES: &[&str] = &["late-night", "late-bloom", "gym"];

    /// Two variables, so the table pins both the variable-name class
    /// and the ordering inside the terms class (variables, then corpus).
    const PINNED_VARS: &[(&str, &[&str])] = &[
        ("bladee_classics", &["crest", "icedancer"]),
        ("quiet_hours", &["mazzy star"]),
    ];

    const PINNED_INPUTS: &[&str] = &[
        "",
        "p",
        "pl",
        "play",
        "qu",
        "vol",
        "xyzzy",
        "view(",
        "view(q",
        "view(que",
        "view(sugg",
        "play(",
        "play(fl",
        "play(FL",
        "play(fl,",
        "queue([a, fl",
        "queue([4 star cre",
        "dry(queue([fl",
        "dry(",
        "repeat(",
        "repeat(3, sk",
        "volume(",
        "seek(",
        "moveto(",
        "speed(",
        "sleep(",
        "goto(",
        "remove(",
        "save_queue(",
        "load_queue(",
        "load_queue(late",
        "add_to_queue(",
        "add_to_queue(fr",
        "define(",
        "define(bla",
        "unset(",
        "unset(bla",
        "play(bla",
        "run(",
        "remember(",
        "remember(track, ",
        "forget(track, ",
        "loop(",
        "shuffle(",
        "eq(",
        "eq(ba",
        "fade(",
        "help(",
        "help(pl",
        "search(cre",
        "star(hex",
        "quit(",
        "nonexistent(fl",
    ];

    /// One row of the pinned table: typed keys, the buffer they produce
    /// (auto-pairing included), the ghost, the hint, the signature with
    /// its active parameter, and the whole candidate list in order.
    ///
    /// This is the guard an optimisation has to get past: it pins what
    /// the user actually sees for every class of parameter at once, so a
    /// faster candidate path that reorders, over-offers or drops a
    /// prefix match fails here rather than in the app.
    fn pin(
        keys: &str,
        buffer: &str,
        ghost: &str,
        hint: Option<&'static str>,
        signature: Option<(&str, usize)>,
        candidates: &[&str],
    ) {
        let mut fake = Fake::new(PINNED_CORPUS);
        fake.queues = PINNED_QUEUES.to_vec();
        fake.vars = scope_of(PINNED_VARS);
        let c = console_with(&fake, keys);
        let here = site(&c.input, c.cursor);
        assert_eq!(c.input, buffer, "buffer for {keys:?}");
        assert_eq!(c.suggestion.ghost, ghost, "ghost for {keys:?}");
        assert_eq!(c.suggestion.hint, hint, "hint for {keys:?}");
        assert_eq!(
            here.signature,
            signature.map(|(s, i)| (s.to_string(), i)),
            "signature for {keys:?}"
        );
        let want: Vec<String> = candidates.iter().map(|s| s.to_string()).collect();
        assert_eq!(
            c.candidates_at(&here, &fake),
            want,
            "candidates for {keys:?}"
        );
    }

    /// Captured off the implementation this pins, with `gen_pinned`.
    #[test]
    fn completion_output_is_pinned_across_every_parameter_class() {
        pin(
            "",
            "",
            "",
            None,
            None,
            &[
                "add_to_queue",
                "back",
                "bottom",
                "cache",
                "clear",
                "clear_queue",
                "cls",
                "crossfade",
                "define",
                "dl",
                "down",
                "download",
                "dry",
                "enqueue",
                "enter",
                "eq",
                "equalizer",
                "exit",
                "explain",
                "fade",
                "fav",
                "favorite",
                "find",
                "flush",
                "forget",
                "forward",
                "fwd",
                "gap",
                "go",
                "goto",
                "grep",
                "help",
                "like",
                "load_queue",
                "loop",
                "ls",
                "move",
                "moveto",
                "mute",
                "next",
                "offline",
                "open",
                "pause",
                "play",
                "play_pause",
                "prev",
                "previous",
                "qclear",
                "queue",
                "quit",
                "random",
                "randomize",
                "redo",
                "remember",
                "remove",
                "repeat",
                "repeat_mode",
                "rescan",
                "run",
                "save_queue",
                "search",
                "seek",
                "select",
                "show",
                "shuf",
                "shuffle",
                "shuffle_queue",
                "silence",
                "skip",
                "sleep",
                "speed",
                "star",
                "tempo",
                "toggle",
                "top",
                "undo",
                "unfav",
                "unfavorite",
                "unlike",
                "unmute",
                "unset",
                "unstar",
                "up",
                "view",
                "vol",
                "volume",
                "wait",
                "xfade",
            ],
        );
        pin(
            "p",
            "p",
            "ause",
            None,
            None,
            &["pause", "play", "play_pause", "prev", "previous"],
        );
        pin("pl", "pl", "ay", None, None, &["play", "play_pause"]);
        pin("play", "play", "_pause", None, None, &["play_pause"]);
        pin("qu", "qu", "eue", None, None, &["queue", "quit"]);
        pin("vol", "vol", "ume", None, None, &["volume"]);
        pin("xyzzy", "xyzzy", "", None, None, &[]);
        pin(
            "view(",
            "view()",
            "q",
            None,
            Some(("view(target, terms?)", 0)),
            &[
                "q",
                "queue",
                "now",
                "suggestions",
                "settings",
                "keybinds",
                "lyrics",
                "starred",
                "index",
                "history",
                "playlists",
                "vars",
                "bindings",
                "eq",
                "undo",
                "help",
            ],
        );
        pin(
            "view(q",
            "view(q)",
            "ueue",
            None,
            Some(("view(target, terms?)", 0)),
            &["queue"],
        );
        pin(
            "view(que",
            "view(que)",
            "ue",
            None,
            Some(("view(target, terms?)", 0)),
            &["queue"],
        );
        pin(
            "view(sugg",
            "view(sugg)",
            "estions",
            None,
            Some(("view(target, terms?)", 0)),
            &["suggestions"],
        );
        pin(
            "play(",
            "play()",
            "bladee_classics",
            None,
            Some(("play(terms?)", 0)),
            &[
                "bladee_classics",
                "quiet_hours",
                "flatline",
                "Flatline (Remix)",
                "floor",
                "sulfur surfer",
                "4 star crest",
                "crest",
                "hex",
                "Icedancer",
                "Eversince",
            ],
        );
        pin(
            "play(fl",
            "play(fl)",
            "atline",
            None,
            Some(("play(terms?)", 0)),
            &["flatline", "Flatline (Remix)", "floor"],
        );
        pin(
            "play(FL",
            "play(FL)",
            "atline",
            None,
            Some(("play(terms?)", 0)),
            &["flatline", "Flatline (Remix)", "floor"],
        );
        pin(
            "play(fl,",
            "play(fl,)",
            "",
            Some("no more arguments"),
            Some(("play(terms?)", 1)),
            &[],
        );
        pin(
            "queue([a, fl",
            "queue([a, fl])",
            "atline",
            None,
            Some(("queue(terms?)", 0)),
            &["flatline", "Flatline (Remix)", "floor"],
        );
        pin(
            "queue([4 star cre",
            "queue([4 star cre])",
            "st",
            None,
            Some(("queue(terms?)", 0)),
            &["4 star crest"],
        );
        pin(
            "dry(queue([fl",
            "dry(queue([fl]))",
            "atline",
            None,
            Some(("queue(terms?)", 0)),
            &["flatline", "Flatline (Remix)", "floor"],
        );
        pin(
            "dry(",
            "dry()",
            "add_to_queue",
            None,
            Some(("dry(call)", 0)),
            &[
                "add_to_queue",
                "back",
                "bottom",
                "cache",
                "clear",
                "clear_queue",
                "cls",
                "crossfade",
                "define",
                "dl",
                "down",
                "download",
                "dry",
                "enqueue",
                "enter",
                "eq",
                "equalizer",
                "exit",
                "explain",
                "fade",
                "fav",
                "favorite",
                "find",
                "flush",
                "forget",
                "forward",
                "fwd",
                "gap",
                "go",
                "goto",
                "grep",
                "help",
                "like",
                "load_queue",
                "loop",
                "ls",
                "move",
                "moveto",
                "mute",
                "next",
                "offline",
                "open",
                "pause",
                "play",
                "play_pause",
                "prev",
                "previous",
                "qclear",
                "queue",
                "quit",
                "random",
                "randomize",
                "redo",
                "remember",
                "remove",
                "repeat",
                "repeat_mode",
                "rescan",
                "run",
                "save_queue",
                "search",
                "seek",
                "select",
                "show",
                "shuf",
                "shuffle",
                "shuffle_queue",
                "silence",
                "skip",
                "sleep",
                "speed",
                "star",
                "tempo",
                "toggle",
                "top",
                "undo",
                "unfav",
                "unfavorite",
                "unlike",
                "unmute",
                "unset",
                "unstar",
                "up",
                "view",
                "vol",
                "volume",
                "wait",
                "xfade",
            ],
        );
        pin(
            "repeat(",
            "repeat()",
            "",
            Some("how many times, 1-256"),
            Some(("repeat(count, call)", 0)),
            &[],
        );
        pin(
            "repeat(3, sk",
            "repeat(3, sk)",
            "ip",
            None,
            Some(("repeat(count, call)", 1)),
            &["skip"],
        );
        pin(
            "volume(",
            "volume()",
            "",
            Some("0-100, or +N% / -N%"),
            Some(("volume(level)", 0)),
            &[],
        );
        pin(
            "seek(",
            "seek()",
            "",
            Some("+30s / -10s (sign required)"),
            Some(("seek(offset)", 0)),
            &[],
        );
        pin(
            "moveto(",
            "moveto()",
            "",
            Some("10% / 90 / 1m20s"),
            Some(("moveto(pos)", 0)),
            &[],
        );
        pin(
            "speed(",
            "speed()",
            "",
            Some("0.25-3.0, or up / down / reset"),
            Some(("speed(speed)", 0)),
            &[],
        );
        pin(
            "sleep(",
            "sleep()",
            "",
            Some("30m / off"),
            Some(("sleep(after)", 0)),
            &[],
        );
        pin(
            "goto(",
            "goto()",
            "",
            Some("a queue position, 1 or higher"),
            Some(("goto(index)", 0)),
            &[],
        );
        pin(
            "remove(",
            "remove()",
            "",
            Some("1, or [1, 3, 5]"),
            Some(("remove(indices)", 0)),
            &[],
        );
        pin(
            "save_queue(",
            "save_queue()",
            "",
            Some("a name to save it under"),
            Some(("save_queue(name)", 0)),
            &[],
        );
        pin(
            "load_queue(",
            "load_queue()",
            "late-night",
            None,
            Some(("load_queue(name)", 0)),
            &["late-night", "late-bloom", "gym"],
        );
        pin(
            "load_queue(late",
            "load_queue(late)",
            "-night",
            None,
            Some(("load_queue(name)", 0)),
            &["late-night", "late-bloom"],
        );
        pin(
            "add_to_queue(",
            "add_to_queue()",
            "front",
            None,
            Some(("add_to_queue(position, terms?)", 0)),
            &["front", "back"],
        );
        pin(
            "add_to_queue(fr",
            "add_to_queue(fr)",
            "ont",
            None,
            Some(("add_to_queue(position, terms?)", 0)),
            &["front"],
        );
        pin(
            "define(",
            "define()",
            "bladee_classics",
            None,
            Some(("define(name, terms)", 0)),
            &["bladee_classics", "quiet_hours"],
        );
        pin(
            "define(bla",
            "define(bla)",
            "dee_classics",
            None,
            Some(("define(name, terms)", 0)),
            &["bladee_classics"],
        );
        pin(
            "unset(",
            "unset()",
            "bladee_classics",
            None,
            Some(("unset(name)", 0)),
            &["bladee_classics", "quiet_hours"],
        );
        pin(
            "unset(bla",
            "unset(bla)",
            "dee_classics",
            None,
            Some(("unset(name)", 0)),
            &["bladee_classics"],
        );
        pin(
            "play(bla",
            "play(bla)",
            "dee_classics",
            None,
            Some(("play(terms?)", 0)),
            &["bladee_classics"],
        );
        pin("run(", "run()", "", None, Some(("run(script)", 0)), &[]);
        pin(
            "remember(",
            "remember()",
            "track",
            None,
            Some(("remember(tier, call)", 0)),
            &["track", "album", "artist"],
        );
        pin(
            "remember(track, ",
            "remember(track, )",
            "eq",
            None,
            Some(("remember(tier, call)", 1)),
            &["eq", "volume", "speed"],
        );
        pin(
            "forget(track, ",
            "forget(track, )",
            "eq",
            None,
            Some(("forget(tier, setting?)", 1)),
            &["eq", "volume", "speed"],
        );
        pin(
            "loop(",
            "loop()",
            "one",
            None,
            Some(("loop(mode?)", 0)),
            &["one", "all", "off"],
        );
        pin(
            "shuffle(",
            "shuffle()",
            "on",
            None,
            Some(("shuffle(state?)", 0)),
            &["on", "off"],
        );
        pin(
            "eq(",
            "eq()",
            "flat",
            None,
            Some(("eq(preset)", 0)),
            &[
                "flat",
                "bass boost",
                "vocal boost",
                "treble boost",
                "rock",
                "classical",
            ],
        );
        pin(
            "eq(ba",
            "eq(ba)",
            "ss boost",
            None,
            Some(("eq(preset)", 0)),
            &["bass boost"],
        );
        pin(
            "fade(",
            "fade()",
            "in",
            None,
            Some(("fade(direction, over?)", 0)),
            &["in", "out"],
        );
        pin(
            "help(",
            "help()",
            "add_to_queue",
            None,
            Some(("help(name?)", 0)),
            &[
                "add_to_queue",
                "back",
                "bottom",
                "cache",
                "clear",
                "clear_queue",
                "cls",
                "crossfade",
                "define",
                "dl",
                "down",
                "download",
                "dry",
                "enqueue",
                "enter",
                "eq",
                "equalizer",
                "exit",
                "explain",
                "fade",
                "fav",
                "favorite",
                "find",
                "flush",
                "forget",
                "forward",
                "fwd",
                "gap",
                "go",
                "goto",
                "grep",
                "help",
                "like",
                "load_queue",
                "loop",
                "ls",
                "move",
                "moveto",
                "mute",
                "next",
                "offline",
                "open",
                "pause",
                "play",
                "play_pause",
                "prev",
                "previous",
                "qclear",
                "queue",
                "quit",
                "random",
                "randomize",
                "redo",
                "remember",
                "remove",
                "repeat",
                "repeat_mode",
                "rescan",
                "run",
                "save_queue",
                "search",
                "seek",
                "select",
                "show",
                "shuf",
                "shuffle",
                "shuffle_queue",
                "silence",
                "skip",
                "sleep",
                "speed",
                "star",
                "tempo",
                "toggle",
                "top",
                "undo",
                "unfav",
                "unfavorite",
                "unlike",
                "unmute",
                "unset",
                "unstar",
                "up",
                "view",
                "vol",
                "volume",
                "wait",
                "xfade",
            ],
        );
        pin(
            "help(pl",
            "help(pl)",
            "ay",
            None,
            Some(("help(name?)", 0)),
            &["play", "play_pause"],
        );
        pin(
            "search(cre",
            "search(cre)",
            "st",
            None,
            Some(("search(query)", 0)),
            &["crest"],
        );
        pin(
            "star(hex",
            "star(hex)",
            "",
            None,
            Some(("star(terms?)", 0)),
            &[],
        );
        pin(
            "quit(",
            "quit()",
            "",
            Some("no more arguments"),
            Some(("quit()", 0)),
            &[],
        );
        pin(
            "nonexistent(fl",
            "nonexistent(fl)",
            "ush",
            None,
            None,
            &["flush"],
        );
    }

    /// Prints the table above in source form, for when a deliberate
    /// behaviour change makes all 44 rows need updating:
    /// `cargo test --bin wim -- --ignored --nocapture gen_pinned`.
    #[test]
    #[ignore = "writes the pinned table, run manually"]
    fn gen_pinned() {
        let mut fake = Fake::new(PINNED_CORPUS);
        fake.queues = PINNED_QUEUES.to_vec();
        fake.vars = scope_of(PINNED_VARS);
        for input in PINNED_INPUTS {
            let c = console_with(&fake, input);
            let here = site(&c.input, c.cursor);
            let hits = c.candidates_at(&here, &fake);
            let sig = match &here.signature {
                None => "None".to_string(),
                Some((s, i)) => format!("Some((\"{s}\", {i}))"),
            };
            let hint = match here.completes {
                Completes::Hint(h) => format!("Some(\"{h}\")"),
                _ => "None".to_string(),
            };
            let cands = hits
                .iter()
                .map(|h| format!("\"{h}\""))
                .collect::<Vec<_>>()
                .join(", ");
            println!(
                "        pin(\"{}\", \"{}\", \"{}\", {}, {}, &[{}]);",
                input.escape_debug(),
                c.input.escape_debug(),
                c.suggestion.ghost.escape_debug(),
                hint,
                sig,
                cands
            );
        }
    }

    // ------------------------------------------------------- benchmarks

    /// A host-shaped provider: hands back exactly the 50 names the real
    /// `CorpusCandidates` caps at, so the console-side cost is measured
    /// against the list length it really sees.
    struct FiftyNames(Vec<String>);

    impl FiftyNames {
        fn new() -> FiftyNames {
            FiftyNames(
                (0..MAX_CANDIDATES)
                    .map(|i| format!("blade runner blues {i}"))
                    .collect(),
            )
        }
    }

    impl Candidates for FiftyNames {
        fn corpus(&self, _token: &str) -> Vec<String> {
            self.0.clone()
        }
    }

    /// Not an assertion: prints the per-keystroke wall time of the whole
    /// `handle_key` -> `refresh` path, which is what decides whether the
    /// prompt feels instant. Absolute numbers are machine-dependent.
    /// `cargo test --release --bin wim -- --ignored --nocapture bench_keystrokes`
    #[test]
    #[ignore = "timing benchmark, run manually"]
    fn bench_keystrokes() {
        let cand = FiftyNames::new();
        // a command name (every_name + the prefix filter), a corpus
        // term (the 50-name list), and a value parameter (hint only,
        // the cheap case) in one line each.
        for line in [
            "play(blade runner blues 1)",
            "queue([blade runner blues 1, blade runner blues 2])",
            "volume(50%)",
            "repeat(3, skip())",
            "add_to_queue(front, blade runner blues 7)",
        ] {
            let iters = 200u32;
            let mut worst = Duration::ZERO;
            let start = Instant::now();
            for _ in 0..iters {
                let mut console = Console::empty();
                for ch in line.chars() {
                    let key = KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE);
                    let one = Instant::now();
                    console.handle_key(key, &cand);
                    worst = worst.max(one.elapsed());
                }
            }
            let keys = line.chars().count() as u32;
            let per = start.elapsed() / (iters * keys);
            println!("{:<52} {per:>10.3?}/key  worst {worst:>10.3?}", line);
        }
    }
}
