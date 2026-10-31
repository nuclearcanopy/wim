use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::eq;
use crate::notify::{Notifications, Severity};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NavidromeConfig {
    pub url: String,
    pub username: String,
    /// Plaintext password, read directly from config.toml. Prefer
    /// `password_file` when the config is managed declaratively (e.g. via
    /// the home-manager module backed by agenix), since Nix store paths
    /// are world-readable.
    #[serde(default)]
    pub password: Option<String>,
    /// Path to a file containing the password (trailing newline trimmed).
    #[serde(default)]
    pub password_file: Option<String>,
    /// Server-side transcoding already happens by default per
    /// navidrome's own configured profile; these are optional
    /// client-side overrides (e.g. a bandwidth-constrained connection
    /// to this specific server) sent as `stream` request params.
    /// `None` means "don't override, use the server's default."
    #[serde(default)]
    pub max_bitrate_kbps: Option<u32>,
    #[serde(default)]
    pub transcode_format: Option<String>,
}

impl NavidromeConfig {
    pub fn resolve_password(&self) -> Result<String> {
        if let Some(p) = &self.password {
            return Ok(p.clone());
        }
        if let Some(path) = &self.password_file {
            let contents = fs::read_to_string(path)
                .with_context(|| format!("reading password_file at {path}"))?;
            return Ok(contents.trim_end_matches('\n').to_string());
        }
        anyhow::bail!("navidrome config has neither `password` nor `password_file` set")
    }
}

/// What the album-art panel should show, named after the result rather
/// than the protocol: `ascii-*` is wim's own block rasterizer (nothing
/// needed from the terminal), `full-*` a real inline graphics protocol
/// (sixel/kitty/iterm2), each in greyscale or colour. `auto` resolves
/// to the best available, `off` drops the panel.
///
/// The pre-rename values still parse, as serde aliases, so a config
/// written before this existed keeps loading: `blocks` was the
/// monochrome rasterizer, and naming a protocol outright
/// (`sixel`/`kitty`/`iterm2`) was an explicit ask for real art, which
/// is `full-color`. Serializing writes the new names.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum AlbumArtMode {
    #[default]
    Auto,
    #[serde(alias = "blocks")]
    AsciiBw,
    AsciiColor,
    FullBw,
    #[serde(alias = "sixel", alias = "kitty", alias = "iterm2")]
    FullColor,
    Off,
}

/// Which video output mpv is told to draw a fullscreen video with.
/// `tct` is true-color half-blocks and works in every terminal; `kitty`
/// is the kitty graphics protocol and needs a terminal that speaks it.
/// `auto` picks kitty only when the graphics probe actually reported
/// kitty, because mpv's kitty vo does no detection of its own: pointed
/// at a terminal that cannot decode it, it emits graphics escapes
/// anyway. See `video::drawing_vo` and `docs/video.md`.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum VideoOutput {
    #[default]
    Auto,
    Tct,
    Kitty,
    Off,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Multiple servers, all merged into one unified search corpus;
    /// consistent with "search spans everything" rather than requiring
    /// an "active server" concept to switch between. Add via the 'n'
    /// profile manager; adding never replaces an existing one.
    #[serde(default)]
    pub navidrome_servers: Vec<NavidromeConfig>,
    #[serde(default)]
    pub album_art: AlbumArtMode,
    /// Directories to scan for local music and video.
    #[serde(default)]
    pub library_paths: Vec<String>,
    /// Off by default: fades the end of one track into the start of
    /// the next rather than a hard cut. Separate from (and layered on
    /// top of) gapless playback, which stays on regardless. Not a
    /// true overlapping crossfade (one mpv instance, one file loaded
    /// at a time); a fade-out/fade-in volume ramp at the boundary.
    #[serde(default)]
    pub crossfade_enabled: bool,
    #[serde(default = "default_crossfade_seconds")]
    pub crossfade_seconds: f64,
    /// Seconds of silence between tracks; 0 (the default) is mpv's own
    /// gapless advance, untouched.
    ///
    /// Not implemented by putting a silence entry between every track:
    /// that doubles the queue's length and makes every index the user
    /// sees wrong. mpv holds at the end of each file instead and wim
    /// waits out the gap before advancing; see `core::BetweenTracks`.
    #[serde(default)]
    pub gap_seconds: f64,
    #[serde(default)]
    pub subtitles: SubtitleConfig,
    /// Neither set (the default): opensubtitles auto-fetch never runs.
    /// Manually-placed subtitle files alongside a video still work
    /// regardless (that's just mpv's own default behavior), this is
    /// purely the "go find one automatically" feature. Same
    /// plaintext-vs-file split as `NavidromeConfig::password`/
    /// `password_file`, same reason: Nix store paths are world-
    /// readable, so a literal key in config.toml via home-manager
    /// would leak to every local user.
    #[serde(default)]
    pub opensubtitles_api_key: Option<String>,
    #[serde(default)]
    pub opensubtitles_api_key_file: Option<String>,
    #[serde(default = "default_subtitle_language")]
    pub subtitle_language: String,
    /// Same plaintext-vs-file split as `NavidromeConfig::password`/
    /// `password_file`, same reason (world-readable nix store paths).
    /// Neither set (the default) means scrobbling never runs; the
    /// token comes from listenbrainz.org -> profile -> settings, not
    /// an account password. Settable via onboarding (first run) or
    /// the settings menu's "listenbrainz token" row.
    #[serde(default)]
    pub listenbrainz_user_token: Option<String>,
    #[serde(default)]
    pub listenbrainz_user_token_file: Option<String>,
    #[serde(default)]
    pub scrobble: ScrobbleConfig,
    /// Which of the generated `ui::logos::LOGOS` entries the home
    /// screen's banner shows, by name rather than index (an index
    /// would silently point at a different font if that generated
    /// list is ever regenerated in a different order). `None` is the
    /// permanent default (ANSI Shadow, `banner::LOGO`), not "whatever
    /// happens to be first".
    #[serde(default)]
    pub logo_style: Option<String>,
    /// Name of the active curve, either an `eq::PRESETS` entry or one
    /// of `eq_custom_presets` (restored by name, same reasoning as
    /// `logo_style`), or `None` while a nameless hand-tuned
    /// `eq_gains` is active instead. See `eq::restore`.
    #[serde(default = "default_eq_preset")]
    pub eq_preset: Option<String>,
    /// Always kept in sync with whatever's actually applied (the
    /// active curve's gains, or a hand-tuned set); read back on
    /// startup alongside `eq_preset`, so both a nameless custom eq
    /// and a named one nudged since survive a restart. `None` means
    /// nothing was ever applied, which is the declarative case: a
    /// config setting only `eq_preset` gets that curve exactly rather
    /// than reading as "that preset, modified to flat".
    #[serde(default)]
    pub eq_gains: Option<[f64; 10]>,
    /// The user's own named curves, saved from the eq menu and listed
    /// there alongside the built-ins. Holds *requested* gains; see
    /// `eq::CustomPreset`.
    #[serde(default)]
    pub eq_custom_presets: eq::CustomPresets,
    /// Action name (`keymap::KeyAction::name`) -> key name
    /// (`keymap::key_label`'s inverse); only non-default bindings
    /// need to appear here (see `Keymap::overrides`), so most users
    /// remapping one or two keys never see a giant table. Unknown
    /// action/key names are ignored rather than rejected, same
    /// leniency as every other best-effort field.
    #[serde(default)]
    pub keybinds: HashMap<String, String>,
    /// On how many *distinct calendar days* a remote song must have
    /// been played before it's automatically downloaded to the local
    /// cache (see `cache.rs`); playing it twice in one sitting is not
    /// a signal, coming back to it another day is. Tweakable live in
    /// the settings menu ('u'). The `alias` carries the value over
    /// from the old play-count field of the same default, so an
    /// existing config keeps working instead of erroring.
    #[serde(
        default = "default_cache_days_threshold",
        alias = "cache_listens_threshold"
    )]
    pub cache_days_threshold: u32,
    /// Panel sizes from resize mode ('r'), persisted so a layout you
    /// deliberately set survives a restart; clamped on load by
    /// `layout::PanelLayout::from_sizes`.
    #[serde(default = "default_sidebar_width")]
    pub sidebar_width: u16,
    #[serde(default = "default_bottom_bar_height")]
    pub bottom_bar_height: u16,
    /// How many days a cached track can go unplayed before it's
    /// evicted (the file deleted, not its play history) to free disk
    /// space. Also tweakable in the settings menu.
    #[serde(default = "default_cache_eviction_days")]
    pub cache_eviction_days: u64,
    /// Click to focus a pane or press a control, drag the scrubber,
    /// wheel to scroll a list. On by default, but still a switch:
    /// capture hands the terminal's own click-drag text selection to
    /// the app, so copying out of it then needs the shift bypass most
    /// terminals offer (see the README's config section).
    #[serde(default = "default_mouse")]
    pub mouse: bool,
    /// How 'v' (fullscreen video) draws. mpv draws it itself, straight
    /// to this terminal, and wim stops drawing while it does; nothing
    /// is rasterized on wim's side. `off` disables the key entirely for
    /// a terminal where neither output is wanted. Music is never
    /// affected: only a `LocalVideo` item ever gets a drawing vo.
    #[serde(default)]
    pub video_output: VideoOutput,
}

fn default_mouse() -> bool {
    true
}

fn default_subtitle_language() -> String {
    "en".to_string()
}

fn default_eq_preset() -> Option<String> {
    Some(eq::PRESETS[0].name.to_string())
}

fn default_sidebar_width() -> u16 {
    crate::layout::SIDEBAR_DEFAULT
}

fn default_bottom_bar_height() -> u16 {
    crate::layout::BOTTOM_BAR_DEFAULT
}

fn default_cache_days_threshold() -> u32 {
    2
}

fn default_cache_eviction_days() -> u64 {
    7
}

/// The fade span every reader agrees on: the range the settings menu
/// steps through, with a non-finite value (toml parses `nan` and
/// `inf`, and `Duration::from_secs_f64` aborts on both) falling back
/// to the default. The menu's steps, load-time validation and the
/// sleep-timer fade all go through here.
pub fn crossfade_seconds(secs: f64) -> f64 {
    if secs.is_finite() {
        secs.clamp(CROSSFADE_MIN, CROSSFADE_MAX)
    } else {
        default_crossfade_seconds()
    }
}

pub const CROSSFADE_MIN: f64 = 1.0;
pub const CROSSFADE_MAX: f64 = 15.0;

impl Config {
    /// `Ok(None)` means no key was configured at all; an `Err` means
    /// one was and the file would not read. Same shape as
    /// `NavidromeConfig::resolve_password`, and for the same reason:
    /// one typo'd path used to silently disable the feature forever,
    /// with the settings menu still showing the row.
    pub fn read_opensubtitles_api_key(&self) -> Result<Option<String>> {
        if let Some(k) = &self.opensubtitles_api_key {
            return Ok(Some(k.clone()));
        }
        let Some(path) = self.opensubtitles_api_key_file.as_ref() else {
            return Ok(None);
        };
        let contents = fs::read_to_string(path)
            .with_context(|| format!("reading opensubtitles_api_key_file at {path}"))?;
        Ok(Some(contents.trim_end_matches('\n').to_string()))
    }

    pub fn read_listenbrainz_user_token(&self) -> Result<Option<String>> {
        if let Some(t) = &self.listenbrainz_user_token {
            return Ok(Some(t.clone()));
        }
        let Some(path) = self.listenbrainz_user_token_file.as_ref() else {
            return Ok(None);
        };
        let contents = fs::read_to_string(path)
            .with_context(|| format!("reading listenbrainz_user_token_file at {path}"))?;
        Ok(Some(contents.trim_end_matches('\n').to_string()))
    }

    /// The same two reads as "is this feature on", for callers with
    /// nowhere to put an error; load time reports the failure once
    /// through `credential_warnings`, so this staying `Option` no
    /// longer loses it.
    pub fn resolve_opensubtitles_api_key(&self) -> Option<String> {
        self.read_opensubtitles_api_key().ok().flatten()
    }

    pub fn resolve_listenbrainz_user_token(&self) -> Option<String> {
        self.read_listenbrainz_user_token().ok().flatten()
    }

    /// Pulls every hand-editable number into the range its reader can
    /// act on, naming each correction: the settings menu only ever
    /// bounded its own steps, so a number typed into the file reached
    /// mpv and `Duration::from_secs_f64` unchecked. Panel sizes and eq
    /// gains are left to `layout::from_sizes`/`eq::clamp_gains`.
    fn sanitize(&mut self) -> Vec<String> {
        let mut w = Vec::new();
        let fade = crossfade_seconds(self.crossfade_seconds);
        if fade.to_bits() != self.crossfade_seconds.to_bits() {
            w.push(format!(
                "crossfade_seconds = {} is outside {CROSSFADE_MIN} to {CROSSFADE_MAX}; using {fade}",
                self.crossfade_seconds
            ));
            self.crossfade_seconds = fade;
        }
        fix_f64("gap_seconds", &mut self.gap_seconds, 0.0, 60.0, 0.0, &mut w);
        fix_f64(
            "scrobble.threshold_percent",
            &mut self.scrobble.threshold_percent,
            5.0,
            100.0,
            default_scrobble_threshold_percent(),
            &mut w,
        );
        fix_f64(
            "scrobble.threshold_seconds",
            &mut self.scrobble.threshold_seconds,
            15.0,
            600.0,
            default_scrobble_threshold_seconds(),
            &mut w,
        );
        let sub = &mut self.subtitles;
        fix_f64(
            "subtitles.delay_secs",
            &mut sub.delay_secs,
            -600.0,
            600.0,
            0.0,
            &mut w,
        );
        fix_f64(
            "subtitles.outline_size",
            &mut sub.outline_size,
            0.0,
            10.0,
            default_sub_outline_size(),
            &mut w,
        );
        fix_f64(
            "subtitles.background_opacity",
            &mut sub.background_opacity,
            0.0,
            1.0,
            0.0,
            &mut w,
        );
        fix_f64("subtitles.blur", &mut sub.blur, 0.0, 20.0, 0.0, &mut w);
        // the one f32 in the file; same rule, widened to share the
        // helper rather than growing a second copy of it.
        let mut level = f64::from(sub.color_level);
        fix_f64(
            "subtitles.color_level",
            &mut level,
            0.0,
            1.0,
            f64::from(default_sub_color_level()),
            &mut w,
        );
        sub.color_level = level as f32;
        fix_int("subtitles.font_size", &mut sub.font_size, 8, 400, &mut w);
        fix_int("subtitles.position", &mut sub.position, 0, 150, &mut w);
        fix_int(
            "cache_days_threshold",
            &mut self.cache_days_threshold,
            1,
            20,
            &mut w,
        );
        fix_int(
            "cache_eviction_days",
            &mut self.cache_eviction_days,
            1,
            90,
            &mut w,
        );
        w
    }

    /// Credential files that were configured and would not read, each
    /// named with the path and the io error. Separate from `sanitize`
    /// because this is the only part of validation that touches the
    /// disk.
    fn credential_warnings(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Err(e) = self.read_listenbrainz_user_token() {
            out.push(format!("scrobbling is off: {e:#}"));
        }
        if let Err(e) = self.read_opensubtitles_api_key() {
            out.push(format!("subtitle fetching is off: {e:#}"));
        }
        // a navidrome `password_file` that will not read is not listed
        // here: that one already surfaces at connect time, since
        // `resolve_password` returns the error to the caller.
        out
    }
}

/// Tunable scrobble thresholds; defaults match ListenBrainz's own
/// convention (half the track, or 4 minutes, whichever comes first).
/// See `listenbrainz::should_scrobble` for exactly how these combine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScrobbleConfig {
    #[serde(default = "default_scrobble_threshold_percent")]
    pub threshold_percent: f64,
    #[serde(default = "default_scrobble_threshold_seconds")]
    pub threshold_seconds: f64,
}

fn default_scrobble_threshold_percent() -> f64 {
    50.0
}
fn default_scrobble_threshold_seconds() -> f64 {
    240.0
}

impl Default for ScrobbleConfig {
    fn default() -> Self {
        ScrobbleConfig {
            threshold_percent: default_scrobble_threshold_percent(),
            threshold_seconds: default_scrobble_threshold_seconds(),
        }
    }
}

fn default_crossfade_seconds() -> f64 {
    3.0
}

/// Applied to mpv's own `sub-*` properties on startup (see
/// `main.rs::apply_subtitle_style`); color stays a single grayscale
/// level per the monochrome rule, not an arbitrary hue, for both the
/// text and its background box.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubtitleConfig {
    /// Seconds; positive delays subtitles later. Also adjustable live
    /// with `,`/`.` while something's playing.
    #[serde(default)]
    pub delay_secs: f64,
    #[serde(default = "default_sub_font_size")]
    pub font_size: u32,
    /// 0 (bottom) to 150 (top), mpv's own `sub-pos` range.
    #[serde(default = "default_sub_position")]
    pub position: u32,
    #[serde(default = "default_sub_outline_size")]
    pub outline_size: f64,
    /// 0.0 (fully transparent) to 1.0 (opaque).
    #[serde(default)]
    pub background_opacity: f64,
    #[serde(default)]
    pub blur: f64,
    /// 0.0 (black) to 1.0 (white) text color; same scale as
    /// `theme::shade` in the ui, applied here to mpv's own renderer.
    #[serde(default = "default_sub_color_level")]
    pub color_level: f32,
}

fn default_sub_font_size() -> u32 {
    42
}
fn default_sub_position() -> u32 {
    10
}
fn default_sub_outline_size() -> f64 {
    2.0
}
fn default_sub_color_level() -> f32 {
    1.0
}

impl Default for SubtitleConfig {
    fn default() -> Self {
        SubtitleConfig {
            delay_secs: 0.0,
            font_size: default_sub_font_size(),
            position: default_sub_position(),
            outline_size: default_sub_outline_size(),
            background_opacity: 0.0,
            blur: 0.0,
            color_level: default_sub_color_level(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            navidrome_servers: Vec::new(),
            album_art: AlbumArtMode::Auto,
            library_paths: Vec::new(),
            crossfade_enabled: false,
            crossfade_seconds: default_crossfade_seconds(),
            gap_seconds: 0.0,
            subtitles: SubtitleConfig::default(),
            opensubtitles_api_key: None,
            opensubtitles_api_key_file: None,
            subtitle_language: default_subtitle_language(),
            listenbrainz_user_token: None,
            listenbrainz_user_token_file: None,
            scrobble: ScrobbleConfig::default(),
            logo_style: None,
            eq_preset: default_eq_preset(),
            eq_gains: None,
            eq_custom_presets: eq::CustomPresets::default(),
            sidebar_width: default_sidebar_width(),
            bottom_bar_height: default_bottom_bar_height(),
            keybinds: HashMap::new(),
            cache_days_threshold: default_cache_days_threshold(),
            cache_eviction_days: default_cache_eviction_days(),
            mouse: default_mouse(),
            video_output: VideoOutput::default(),
        }
    }
}

pub fn config_path() -> Result<PathBuf> {
    let dir = dirs::config_dir().context("no config dir found for this platform")?;
    Ok(dir.join("wim").join("config.toml"))
}

/// A loaded config plus what is worth saying out loud once: a key
/// nobody reads, a number that had to be corrected, a credential file
/// that would not open. All three used to be silent, and a negative
/// `crossfade_seconds` was worse than silent (it aborted the process
/// on the first sleep timer).
pub struct Loaded {
    pub cfg: Config,
    pub warnings: Vec<String>,
}

/// Loads config.toml, or starts with defaults on first run; never
/// bails out to a cli error. A missing navidrome section just means
/// the in-app login prompt (main.rs) offers to set one up; forcing
/// people to hand-edit a toml file before they can even see the app
/// once was a bad first-run experience, not a reasonable default.
///
/// Corrects rather than refuses, and never writes back: `save`
/// regenerates the file from the struct, which would drop a
/// hand-edited one's comments. A correction holds for the session, and
/// the next settings-menu save persists it through `save_or_warn`.
pub fn load_reporting() -> Result<Loaded> {
    let path = config_path()?;
    if !path.exists() {
        save(&Config::default())?;
        return Ok(Loaded {
            cfg: Config::default(),
            warnings: Vec::new(),
        });
    }
    let raw = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let mut loaded = validate(&raw).with_context(|| format!("parsing {}", path.display()))?;
    loaded.warnings.extend(loaded.cfg.credential_warnings());
    Ok(loaded)
}

/// `load_reporting` minus the disk, so validation itself is testable
/// without a real config.toml.
fn validate(raw: &str) -> Result<Loaded> {
    let mut cfg: Config = toml::from_str(raw)?;
    let mut warnings = unknown_keys(raw);
    warnings.extend(cfg.sanitize());
    Ok(Loaded { cfg, warnings })
}

/// The config, with the warnings printed instead of shown: for a
/// front-end with no notification area of its own (shellswim), which
/// already talks to the user over stderr.
pub fn load_or_init() -> Result<Config> {
    let loaded = load_reporting()?;
    for w in &loaded.warnings {
        eprintln!("config: {w}");
    }
    Ok(loaded.cfg)
}

/// Corrects one number in place, and says nothing when it was already
/// fine. Non-finite is handled apart from out-of-range: `clamp`
/// propagates a nan rather than bounding it, so `nan`/`inf` (which
/// toml parses) take the field's default instead.
fn fix_f64(key: &str, v: &mut f64, lo: f64, hi: f64, fallback: f64, w: &mut Vec<String>) {
    let was = *v;
    let fixed = if was.is_finite() {
        was.clamp(lo, hi)
    } else {
        fallback
    };
    if fixed.to_bits() == was.to_bits() {
        return;
    }
    *v = fixed;
    w.push(format!(
        "{key} = {was} is outside {lo} to {hi}; using {fixed}"
    ));
}

fn fix_int<T: Ord + Copy + std::fmt::Display>(
    key: &str,
    v: &mut T,
    lo: T,
    hi: T,
    w: &mut Vec<String>,
) {
    let was = *v;
    let fixed = was.clamp(lo, hi);
    if fixed == was {
        return;
    }
    *v = fixed;
    w.push(format!(
        "{key} = {was} is outside {lo} to {hi}; using {fixed}"
    ));
}

/// Every top-level key this build reads, aliases included (an older
/// config still says `cache_listens_threshold`). By hand rather than
/// through serde, for the reason in `unknown_keys`; the
/// `every_config_field_is_a_known_key` test pins the list against the
/// struct.
const KNOWN_KEYS: &[&str] = &[
    "navidrome_servers",
    "album_art",
    "library_paths",
    "crossfade_enabled",
    "crossfade_seconds",
    "gap_seconds",
    "subtitles",
    "opensubtitles_api_key",
    "opensubtitles_api_key_file",
    "subtitle_language",
    "listenbrainz_user_token",
    "listenbrainz_user_token_file",
    "scrobble",
    "logo_style",
    "eq_preset",
    "eq_gains",
    "eq_custom_presets",
    "keybinds",
    "cache_days_threshold",
    "cache_listens_threshold",
    "sidebar_width",
    "bottom_bar_height",
    "cache_eviction_days",
    "mouse",
    "video_output",
];

const SUBTITLE_KEYS: &[&str] = &[
    "delay_secs",
    "font_size",
    "position",
    "outline_size",
    "background_opacity",
    "blur",
    "color_level",
];

const SCROBBLE_KEYS: &[&str] = &["threshold_percent", "threshold_seconds"];

const NAVIDROME_KEYS: &[&str] = &[
    "url",
    "username",
    "password",
    "password_file",
    "max_bitrate_kbps",
    "transcode_format",
];

/// Keys nothing reads, each with the nearest real name when one is
/// close (`libary_paths` parsed clean and was never used, so the
/// library just looked empty). A warning and not
/// `deny_unknown_fields`: refusing to start over one stray key in a
/// hand-edited file is the worse failure. Nested tables included,
/// which that attribute would not have covered.
fn unknown_keys(raw: &str) -> Vec<String> {
    let Ok(table) = raw.parse::<toml::Table>() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    collect_unknown(&table, "", KNOWN_KEYS, &mut out);
    if let Some(toml::Value::Table(t)) = table.get("subtitles") {
        collect_unknown(t, "subtitles.", SUBTITLE_KEYS, &mut out);
    }
    if let Some(toml::Value::Table(t)) = table.get("scrobble") {
        collect_unknown(t, "scrobble.", SCROBBLE_KEYS, &mut out);
    }
    if let Some(toml::Value::Array(servers)) = table.get("navidrome_servers") {
        for server in servers {
            if let toml::Value::Table(t) = server {
                collect_unknown(t, "navidrome_servers.", NAVIDROME_KEYS, &mut out);
            }
        }
    }
    out
}

fn collect_unknown(table: &toml::Table, prefix: &str, known: &[&str], out: &mut Vec<String>) {
    for key in table.keys() {
        if known.contains(&key.as_str()) {
            continue;
        }
        out.push(match crate::namehint::closest(key, known.iter().copied()) {
            Some(near) => {
                format!(
                    "unknown config key '{prefix}{key}', ignored; did you mean '{prefix}{near}'?"
                )
            }
            None => format!("unknown config key '{prefix}{key}', ignored"),
        });
    }
}

pub fn save(cfg: &Config) -> Result<()> {
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let toml = toml::to_string_pretty(cfg)?;
    fs::write(&path, toml).with_context(|| format!("writing {}", path.display()))?;
    restrict_to_owner(&path)?;
    Ok(())
}

/// 0600 on the config file: it holds navidrome passwords and a
/// listenbrainz token in cleartext, and the default 0644 left those
/// readable by every account on the machine. Applied on every save,
/// not just creation, so an already-loose file gets tightened too.
#[cfg(unix)]
fn restrict_to_owner(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restricting permissions on {}", path.display()))
}

#[cfg(not(unix))]
fn restrict_to_owner(_path: &Path) -> Result<()> {
    Ok(())
}

/// Persists config, surfacing a failure instead of swallowing it: a
/// read-only config dir or a full disk used to mean every setting
/// silently reverted on the next launch with nothing said about it.
pub fn save_or_warn(cfg: &Config, notifications: &mut Notifications) {
    if let Err(e) = save(cfg) {
        notifications.push(Severity::Error, format!("couldn't save settings: {e}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A number straight out of the file used to reach every reader
    /// unchecked, and this one reached `Duration::from_secs_f64`,
    /// which aborts the process on a negative, nan or infinite value;
    /// `crossfade_seconds = -1` plus a sleep timer was a crash in a
    /// config file.
    #[test]
    fn a_crossfade_no_duration_can_hold_is_corrected_at_load() {
        for raw in [
            "crossfade_seconds = -1\n",
            "crossfade_seconds = nan\n",
            "crossfade_seconds = inf\n",
            "crossfade_seconds = -inf\n",
            "crossfade_seconds = 1e300\n",
            "crossfade_seconds = 900\n",
        ] {
            let loaded = validate(raw).unwrap_or_else(|e| panic!("{raw}: {e}"));
            let secs = loaded.cfg.crossfade_seconds;
            assert!(
                (CROSSFADE_MIN..=CROSSFADE_MAX).contains(&secs),
                "{raw} left {secs}"
            );
            // the use site itself, which is what aborted.
            let _ = std::time::Duration::from_secs_f64(secs);
            assert!(
                loaded
                    .warnings
                    .iter()
                    .any(|w| w.starts_with("crossfade_seconds =")),
                "{raw} was corrected in silence: {:?}",
                loaded.warnings
            );
        }
    }

    /// Out of range is not always a crash; `threshold_percent = 5000`
    /// just meant nothing ever scrobbled, with the settings menu
    /// showing 5000% and no complaint anywhere.
    #[test]
    fn an_impossible_scrobble_threshold_is_corrected_at_load() {
        let loaded = validate("scrobble = { threshold_percent = 5000 }\n").expect("parsing");
        assert_eq!(loaded.cfg.scrobble.threshold_percent, 100.0);
        assert_eq!(loaded.warnings.len(), 1, "{:?}", loaded.warnings);
        // and the whole point of the number: a track played to the end
        // now scrobbles, where before it could not.
        assert!(crate::listenbrainz::should_scrobble(
            &loaded.cfg.scrobble,
            200.0,
            200.0
        ));

        let loaded = validate("scrobble = { threshold_seconds = -3 }\n").expect("parsing");
        assert_eq!(loaded.cfg.scrobble.threshold_seconds, 15.0);
    }

    /// Every other hand-editable number, each held to the range its
    /// reader (mpv, the cache sweep) can act on.
    #[test]
    fn the_rest_of_the_out_of_range_numbers_are_corrected_too() {
        let loaded = validate(
            r#"
gap_seconds = -5
cache_days_threshold = 0
cache_eviction_days = 4000
[subtitles]
position = 900
background_opacity = 4
color_level = -1
font_size = 0
blur = nan
"#,
        )
        .expect("parsing");
        let cfg = &loaded.cfg;
        assert_eq!(cfg.gap_seconds, 0.0);
        assert_eq!(cfg.cache_days_threshold, 1);
        assert_eq!(cfg.cache_eviction_days, 90);
        assert_eq!(cfg.subtitles.position, 150);
        assert_eq!(cfg.subtitles.background_opacity, 1.0);
        assert_eq!(cfg.subtitles.color_level, 0.0);
        assert_eq!(cfg.subtitles.font_size, 8);
        assert_eq!(cfg.subtitles.blur, 0.0);
        assert_eq!(loaded.warnings.len(), 8, "{:?}", loaded.warnings);
    }

    /// `libary_paths` parsed clean and was read by nothing, so the
    /// library came up empty with nothing said about why. Named and
    /// ignored rather than refused: see `unknown_keys`.
    #[test]
    fn a_key_nothing_reads_is_named_with_the_name_it_missed() {
        let loaded = validate("libary_paths = [\"/music\"]\n").expect("parsing");
        assert!(loaded.cfg.library_paths.is_empty());
        assert_eq!(
            loaded.warnings,
            vec![
                "unknown config key 'libary_paths', ignored; did you mean 'library_paths'?"
                    .to_string()
            ]
        );

        // nested tables too, which a `deny_unknown_fields` on `Config`
        // alone would have missed.
        let loaded = validate(
            r#"
[scrobble]
threshhold_percent = 70

[[navidrome_servers]]
url = "https://music.example"
username = "me"
pasword = "x"
"#,
        )
        .expect("parsing");
        assert_eq!(loaded.warnings.len(), 2, "{:?}", loaded.warnings);
        assert!(loaded.warnings[0].contains("'scrobble.threshhold_percent'"));
        assert!(loaded.warnings[0].contains("'scrobble.threshold_percent'"));
        assert!(loaded.warnings[1].contains("'navidrome_servers.pasword'"));
        assert!(loaded.warnings[1].contains("'navidrome_servers.password'"));

        // a key with nothing close by still gets named, just without
        // a guess.
        let loaded = validate("shuffle = true\n").expect("parsing");
        assert_eq!(
            loaded.warnings,
            vec!["unknown config key 'shuffle', ignored".to_string()]
        );
    }

    /// `KNOWN_KEYS` is written by hand, so this is what keeps it
    /// honest: a field added to `Config` and not listed there would
    /// start warning about itself. The literal is exhaustive on
    /// purpose; a new field breaks this test's compile until it is
    /// accounted for.
    #[test]
    fn every_config_field_is_a_known_key() {
        let full = Config {
            navidrome_servers: vec![NavidromeConfig {
                url: "https://music.example".into(),
                username: "me".into(),
                password: Some("x".into()),
                password_file: Some("/run/secret".into()),
                max_bitrate_kbps: Some(320),
                transcode_format: Some("opus".into()),
            }],
            album_art: AlbumArtMode::Auto,
            library_paths: vec!["/music".into()],
            crossfade_enabled: true,
            crossfade_seconds: 3.0,
            gap_seconds: 1.0,
            subtitles: SubtitleConfig::default(),
            opensubtitles_api_key: Some("k".into()),
            opensubtitles_api_key_file: Some("/run/secret".into()),
            subtitle_language: "en".into(),
            listenbrainz_user_token: Some("t".into()),
            listenbrainz_user_token_file: Some("/run/secret".into()),
            scrobble: ScrobbleConfig::default(),
            logo_style: Some("ANSI Shadow".into()),
            eq_preset: Some("flat".into()),
            eq_gains: Some([0.0; 10]),
            eq_custom_presets: eq::CustomPresets::default(),
            keybinds: HashMap::new(),
            cache_days_threshold: 2,
            sidebar_width: 40,
            bottom_bar_height: 8,
            cache_eviction_days: 7,
            mouse: true,
            video_output: VideoOutput::Auto,
        };
        let text = toml::to_string_pretty(&full).expect("serializing");
        let table: toml::Table = text.parse().expect("parsing back");
        for key in table.keys() {
            assert!(
                KNOWN_KEYS.contains(&key.as_str()),
                "'{key}' is a real field but not in KNOWN_KEYS"
            );
        }
        // and nothing written by this build is ever reported as
        // unknown, aliases aside.
        assert!(unknown_keys(&text).is_empty(), "{text}");
        for key in KNOWN_KEYS {
            assert!(
                table.contains_key(*key) || *key == "cache_listens_threshold",
                "'{key}' is in KNOWN_KEYS but is not a field (nor the alias)"
            );
        }
        // the nested lists, checked the same way.
        let nested = |section: &str, known: &[&str]| {
            let table: toml::Table = text.parse().unwrap();
            let sub = match table.get(section) {
                Some(toml::Value::Table(t)) => t.clone(),
                Some(toml::Value::Array(a)) => match &a[0] {
                    toml::Value::Table(t) => t.clone(),
                    other => panic!("{section} is {other:?}"),
                },
                other => panic!("{section} is {other:?}"),
            };
            for key in sub.keys() {
                assert!(known.contains(&key.as_str()), "{section}.{key} unlisted");
            }
        };
        nested("subtitles", SUBTITLE_KEYS);
        nested("scrobble", SCROBBLE_KEYS);
        nested("navidrome_servers", NAVIDROME_KEYS);
    }

    /// A config this build wrote has nothing to say about itself; the
    /// warnings are only ever about what the user typed.
    #[test]
    fn a_config_this_build_wrote_loads_without_a_word() {
        let text = toml::to_string_pretty(&Config::default()).expect("serializing");
        let loaded = validate(&text).expect("parsing");
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        // including the pre-rename names an older config still uses.
        let loaded = validate("cache_listens_threshold = 4\nalbum_art = \"blocks\"\n")
            .expect("an older config still loads");
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        assert_eq!(loaded.cfg.cache_days_threshold, 4);
    }

    /// A token file that will not read used to mean scrobbling was
    /// off forever while onboarding said it was set up and the
    /// settings menu still showed the row. Same treatment as
    /// `resolve_password`: the io error is kept, and load reports it.
    #[test]
    fn an_unreadable_token_file_is_reported_rather_than_silently_ignored() {
        let dir = std::env::temp_dir().join(format!("wim-tokentest-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let good = dir.join("token");
        fs::write(&good, "tok-123\n").unwrap();
        let missing = dir.join("typo").display().to_string();

        let cfg = Config {
            listenbrainz_user_token_file: Some(good.display().to_string()),
            ..Default::default()
        };
        assert_eq!(
            cfg.read_listenbrainz_user_token().unwrap().as_deref(),
            Some("tok-123")
        );
        assert!(cfg.credential_warnings().is_empty());

        let cfg = Config {
            listenbrainz_user_token_file: Some(missing.clone()),
            opensubtitles_api_key_file: Some(missing.clone()),
            ..Default::default()
        };
        assert!(cfg.read_listenbrainz_user_token().is_err());
        assert!(cfg.resolve_listenbrainz_user_token().is_none());
        let warnings = cfg.credential_warnings();
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(
            warnings[0].contains("scrobbling is off") && warnings[0].contains(&missing),
            "{warnings:?}"
        );
        assert!(
            warnings[1].contains("subtitle fetching is off") && warnings[1].contains(&missing),
            "{warnings:?}"
        );

        // nothing configured is not a failure, it is the default.
        assert!(Config::default().credential_warnings().is_empty());
        fs::remove_dir_all(&dir).ok();
    }

    /// The saved eq presets are an array of tables, which toml can
    /// only emit after every plain key; serializing the whole struct
    /// is the only thing that proves the field order still works.
    #[test]
    fn a_config_with_saved_eq_presets_round_trips_through_toml() {
        let mut cfg = Config::default();
        let tuned = [3.0, -2.0, 1.0, 0.5, -4.0, 6.0, -1.5, 2.0, -3.0, 12.0];
        cfg.eq_custom_presets.insert("night", tuned);
        cfg.eq_custom_presets.insert("car", [1.0; 10]);
        cfg.eq_preset = Some("night".to_string());
        cfg.eq_gains = Some(tuned);

        let text = toml::to_string_pretty(&cfg).expect("serializing");
        let back: Config = toml::from_str(&text).expect("parsing back");

        assert_eq!(back.eq_custom_presets, cfg.eq_custom_presets);
        assert_eq!(back.eq_preset.as_deref(), Some("night"));
        assert_eq!(back.eq_gains, Some(tuned));

        // and the first-run shape, with nothing saved and no gains
        // ever applied, is what `save` writes before anything else
        // happens; an unset `eq_gains` has to stay unset rather than
        // being written out as a flat curve (see `eq::restore`).
        let fresh = toml::to_string_pretty(&Config::default()).expect("serializing a fresh config");
        assert!(!fresh.contains("eq_gains"), "unset, so omitted:\n{fresh}");
        assert!(fresh.contains("eq_custom_presets = []"), "{fresh}");
        let fresh: Config = toml::from_str(&fresh).expect("parsing a fresh config back");
        assert_eq!(fresh.eq_gains, None);
    }

    /// Every eq field is optional: a config written before saved
    /// presets existed (and one written by hand with no eq section at
    /// all) has to keep loading.
    #[test]
    fn a_config_with_no_eq_section_loads_with_an_empty_store() {
        let cfg: Config = toml::from_str("album_art = \"auto\"\n").expect("parsing");
        assert!(cfg.eq_custom_presets.is_empty());
        assert_eq!(cfg.eq_gains, None);
    }

    /// A config written before the four art modes existed names a
    /// protocol or `blocks`; every one of those has to keep parsing,
    /// into the mode that means the same thing. `blocks` asked for the
    /// monochrome rasterizer (`ascii-bw`); naming a protocol asked for
    /// real art, which is `full-color`.
    #[test]
    fn the_pre_rename_album_art_values_still_parse() {
        for (raw, want) in [
            ("auto", AlbumArtMode::Auto),
            ("blocks", AlbumArtMode::AsciiBw),
            ("sixel", AlbumArtMode::FullColor),
            ("kitty", AlbumArtMode::FullColor),
            ("iterm2", AlbumArtMode::FullColor),
            ("off", AlbumArtMode::Off),
            // and the names this writes now
            ("ascii-bw", AlbumArtMode::AsciiBw),
            ("ascii-color", AlbumArtMode::AsciiColor),
            ("full-bw", AlbumArtMode::FullBw),
            ("full-color", AlbumArtMode::FullColor),
        ] {
            let cfg: Config = toml::from_str(&format!("album_art = \"{raw}\"\n"))
                .unwrap_or_else(|e| panic!("album_art = {raw:?} stopped parsing: {e}"));
            assert_eq!(cfg.album_art, want, "album_art = {raw:?}");
        }
    }

    /// What `save` writes has to be what `load_or_init` reads, so a
    /// mode set in the app survives a restart.
    #[test]
    fn every_album_art_mode_round_trips_through_toml() {
        for mode in [
            AlbumArtMode::Auto,
            AlbumArtMode::AsciiBw,
            AlbumArtMode::AsciiColor,
            AlbumArtMode::FullBw,
            AlbumArtMode::FullColor,
            AlbumArtMode::Off,
        ] {
            let cfg = Config {
                album_art: mode,
                ..Default::default()
            };
            let text = toml::to_string_pretty(&cfg).expect("serializing");
            let back: Config = toml::from_str(&text).expect("parsing back");
            assert_eq!(back.album_art, mode, "{mode:?} did not survive:\n{text}");
        }
    }

    /// A config written before mouse support existed has to come back
    /// with it on, not off: the field defaulting to `false` through
    /// `#[serde(default)]` is the easy way to ship a feature nobody
    /// gets until they edit a file.
    #[test]
    fn mouse_support_is_on_unless_a_config_turns_it_off() {
        assert!(Config::default().mouse);
        let old: Config = toml::from_str("album_art = \"auto\"\n").expect("parsing");
        assert!(old.mouse);
        let off: Config = toml::from_str("mouse = false\n").expect("parsing");
        assert!(!off.mouse);
    }

    /// `auto` is the default and has to survive a config written before
    /// the option existed, same reasoning as `mouse`: shipping it as
    /// `off` by default is shipping a feature nobody gets.
    #[test]
    fn video_output_defaults_to_auto_and_round_trips() {
        assert_eq!(Config::default().video_output, VideoOutput::Auto);
        let old: Config = toml::from_str("album_art = \"auto\"\n").expect("parsing");
        assert_eq!(old.video_output, VideoOutput::Auto);

        for (raw, want) in [
            ("video_output = \"tct\"\n", VideoOutput::Tct),
            ("video_output = \"kitty\"\n", VideoOutput::Kitty),
            ("video_output = \"off\"\n", VideoOutput::Off),
            ("video_output = \"auto\"\n", VideoOutput::Auto),
        ] {
            let cfg: Config = toml::from_str(raw).unwrap_or_else(|e| panic!("{raw}: {e}"));
            assert_eq!(cfg.video_output, want, "{raw}");
            let back: Config =
                toml::from_str(&toml::to_string_pretty(&cfg).expect("serializing")).expect("back");
            assert_eq!(back.video_output, want, "{raw}");
        }
    }

    /// Verbatim shape `nix/home-manager.nix` generates for a declared
    /// curve: a name and no `eq_gains`, which has to come back as that
    /// curve exactly rather than as "modified to flat".
    #[test]
    fn a_declaratively_set_curve_loads_as_itself() {
        let raw = r#"
eq_preset = "night bus"

[[eq_custom_presets]]
gains = [-4.0, -2.0, 0.0, 1.0, 2.0, 3.0, 2.0, 0.0, -2.0, -4.0]
name = "night bus"
"#;
        let cfg: Config = toml::from_str(raw).expect("parsing");
        let restored = eq::restore(
            cfg.eq_preset.as_deref(),
            cfg.eq_gains,
            &cfg.eq_custom_presets,
        );
        assert_eq!(restored.source, Some(eq::Source::Saved(0)));
        assert!(!restored.modified);
        assert_eq!(
            restored.gains,
            [-4.0, -2.0, 0.0, 1.0, 2.0, 3.0, 2.0, 0.0, -2.0, -4.0]
        );
    }

    #[test]
    #[cfg(unix)]
    fn restrict_to_owner_drops_group_and_other_access() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("wim-cfgtest-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        fs::write(&path, "x = 1\n").unwrap();
        // start deliberately world-readable, the way a plain
        // fs::write leaves it under the usual 022 umask.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        restrict_to_owner(&path).unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "config holds cleartext credentials");
        fs::remove_dir_all(&dir).ok();
    }
}
