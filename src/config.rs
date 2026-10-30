use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

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

#[cfg(test)]
mod tests {
    use super::*;

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
