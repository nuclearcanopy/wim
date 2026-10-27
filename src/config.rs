use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

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
}

fn default_crossfade_seconds() -> f64 {
    3.0
}

impl Default for Config {
    fn default() -> Self {
        Config {
            navidrome_servers: Vec::new(),
            album_art: AlbumArtMode::Auto,
            library_paths: Vec::new(),
            crossfade_enabled: false,
            crossfade_seconds: default_crossfade_seconds(),
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
