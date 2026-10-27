use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::eq;

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

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AlbumArtMode {
    #[default]
    Auto,
    Blocks,
    Sixel,
    Kitty,
    Iterm2,
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
    /// Name of the active `eq::PRESETS` entry (restored by name, same
    /// reasoning as `logo_style`), or `None` while a hand-tuned
    /// `eq_gains` is active instead (see `app::App::eq_custom`).
    #[serde(default = "default_eq_preset")]
    pub eq_preset: Option<String>,
    /// Always kept in sync with whatever's actually applied (the
    /// active preset's gains, or a hand-tuned set); read back on
    /// startup regardless of `eq_preset`, so a custom eq survives a
    /// restart even though it has no name of its own.
    #[serde(default = "default_eq_gains")]
    pub eq_gains: [f64; 10],
    /// Action name (`keymap::KeyAction::name`) -> key name
    /// (`keymap::key_label`'s inverse); only non-default bindings
    /// need to appear here (see `Keymap::overrides`), so most users
    /// remapping one or two keys never see a giant table. Unknown
    /// action/key names are ignored rather than rejected, same
    /// leniency as every other best-effort field.
    #[serde(default)]
    pub keybinds: HashMap<String, String>,
    /// How many times a remote song must start playing before it's
    /// automatically downloaded to the local cache (see `cache.rs`).
    /// Tweakable live in the settings menu ('u').
    #[serde(default = "default_cache_listens_threshold")]
    pub cache_listens_threshold: u32,
    /// How many days a cached track can go unplayed before it's
    /// evicted (the file deleted, not its play history) to free disk
    /// space. Also tweakable in the settings menu.
    #[serde(default = "default_cache_eviction_days")]
    pub cache_eviction_days: u64,
}

fn default_subtitle_language() -> String {
    "en".to_string()
}

fn default_eq_preset() -> Option<String> {
    Some(eq::PRESETS[0].name.to_string())
}

fn default_eq_gains() -> [f64; 10] {
    eq::PRESETS[0].gains
}

fn default_cache_listens_threshold() -> u32 {
    2
}

fn default_cache_eviction_days() -> u64 {
    7
}

impl Config {
    pub fn resolve_opensubtitles_api_key(&self) -> Option<String> {
        if let Some(k) = &self.opensubtitles_api_key {
            return Some(k.clone());
        }
        let path = self.opensubtitles_api_key_file.as_ref()?;
        fs::read_to_string(path)
            .ok()
            .map(|s| s.trim_end_matches('\n').to_string())
    }

    pub fn resolve_listenbrainz_user_token(&self) -> Option<String> {
        if let Some(t) = &self.listenbrainz_user_token {
            return Some(t.clone());
        }
        let path = self.listenbrainz_user_token_file.as_ref()?;
        fs::read_to_string(path)
            .ok()
            .map(|s| s.trim_end_matches('\n').to_string())
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
            subtitles: SubtitleConfig::default(),
            opensubtitles_api_key: None,
            opensubtitles_api_key_file: None,
            subtitle_language: default_subtitle_language(),
            listenbrainz_user_token: None,
            listenbrainz_user_token_file: None,
            scrobble: ScrobbleConfig::default(),
            logo_style: None,
            eq_preset: default_eq_preset(),
            eq_gains: default_eq_gains(),
            keybinds: HashMap::new(),
            cache_listens_threshold: default_cache_listens_threshold(),
            cache_eviction_days: default_cache_eviction_days(),
        }
    }
}

pub fn config_path() -> Result<PathBuf> {
    let dir = dirs::config_dir().context("no config dir found for this platform")?;
    Ok(dir.join("wim").join("config.toml"))
}

/// Loads config.toml, or starts with defaults on first run; never
/// bails out to a cli error. A missing navidrome section just means
/// the in-app login prompt (main.rs) offers to set one up; forcing
/// people to hand-edit a toml file before they can even see the app
/// once was a bad first-run experience, not a reasonable default.
pub fn load_or_init() -> Result<Config> {
    let path = config_path()?;
    if !path.exists() {
        save(&Config::default())?;
        return Ok(Config::default());
    }
    let raw = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let cfg: Config =
        toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    Ok(cfg)
}

pub fn save(cfg: &Config) -> Result<()> {
    let path = config_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let toml = toml::to_string_pretty(cfg)?;
    fs::write(&path, toml).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}
