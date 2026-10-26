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
    pub navidrome: Option<NavidromeConfig>,
    #[serde(default)]
    pub album_art: AlbumArtMode,
    /// Directories to scan for local music and video.
    #[serde(default)]
    pub library_paths: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            navidrome: None,
            album_art: AlbumArtMode::Auto,
            library_paths: Vec::new(),
        }
    }
}

pub fn config_path() -> Result<PathBuf> {
    let dir = dirs::config_dir().context("no config dir found for this platform")?;
    Ok(dir.join("wim").join("config.toml"))
}

/// Loads config.toml, writing a commented default template on first run.
pub fn load_or_init() -> Result<Config> {
    let path = config_path()?;
    if !path.exists() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, DEFAULT_TEMPLATE)?;
        anyhow::bail!(
            "no config found; wrote a template to {} — fill it in and re-run",
            path.display()
        );
    }
    let raw = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let cfg: Config = toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    Ok(cfg)
}

const DEFAULT_TEMPLATE: &str = r#"# wim config
# this file can also be generated declaratively by the wim home-manager
# module; hand-editing and Nix-managed config are equivalent to the app.

[navidrome]
url = "https://navidrome.example.com"
username = "me"
# either set a literal password, or point at a file (e.g. an agenix secret)
# password = "..."
password_file = "/run/secrets/navidrome-password"

# auto | blocks | sixel | kitty | iterm2 | off
album_art = "auto"

library_paths = []
"#;
