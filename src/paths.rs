//! Where wim keeps its files. Linux and Windows defer to the `dirs`
//! crate unchanged. macOS would otherwise get `~/Library/...`; wim
//! uses the XDG layout there instead (honouring the XDG_* variables
//! when set), so a dotfile setup behaves the same as on Linux.

use std::path::PathBuf;

/// The environment inputs, so the choice is testable without touching
/// the process environment.
#[derive(Debug, Clone, Default)]
pub struct Env {
    pub home: Option<PathBuf>,
    pub xdg_config_home: Option<PathBuf>,
    pub xdg_cache_home: Option<PathBuf>,
    pub xdg_state_home: Option<PathBuf>,
    pub xdg_data_home: Option<PathBuf>,
}

impl Env {
    pub fn from_process() -> Self {
        let get = |k: &str| std::env::var_os(k).map(PathBuf::from);
        Env {
            home: get("HOME").or_else(dirs::home_dir),
            xdg_config_home: get("XDG_CONFIG_HOME"),
            xdg_cache_home: get("XDG_CACHE_HOME"),
            xdg_state_home: get("XDG_STATE_HOME"),
            xdg_data_home: get("XDG_DATA_HOME"),
        }
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Config,
    Cache,
    State,
    Data,
}

/// XDG rule: a variable counts only when set to an absolute path.
fn xdg_pick(var: &Option<PathBuf>, home: &Option<PathBuf>, default: &str) -> Option<PathBuf> {
    match var {
        Some(p) if p.is_absolute() => Some(p.clone()),
        _ => home.as_ref().map(|h| h.join(default)),
    }
}

fn xdg(env: &Env, kind: Kind) -> Option<PathBuf> {
    match kind {
        Kind::Config => xdg_pick(&env.xdg_config_home, &env.home, ".config"),
        Kind::Cache => xdg_pick(&env.xdg_cache_home, &env.home, ".cache"),
        Kind::State => xdg_pick(&env.xdg_state_home, &env.home, ".local/state"),
        Kind::Data => xdg_pick(&env.xdg_data_home, &env.home, ".local/share"),
    }
}

fn pick(macos: bool, env: &Env, kind: Kind, native: fn() -> Option<PathBuf>) -> Option<PathBuf> {
    if macos { xdg(env, kind) } else { native() }
}

const MACOS: bool = cfg!(target_os = "macos");

pub fn config_dir_for(macos: bool, env: &Env) -> Option<PathBuf> {
    pick(macos, env, Kind::Config, dirs::config_dir)
}
pub fn cache_dir_for(macos: bool, env: &Env) -> Option<PathBuf> {
    pick(macos, env, Kind::Cache, dirs::cache_dir)
}
/// State dir, falling back to the data dir, as every caller did.
pub fn state_dir_for(macos: bool, env: &Env) -> Option<PathBuf> {
    pick(macos, env, Kind::State, dirs::state_dir)
        .or_else(|| pick(macos, env, Kind::Data, dirs::data_dir))
}

pub fn config_dir() -> Option<PathBuf> {
    if MACOS {
        config_dir_for(true, &Env::from_process())
    } else {
        dirs::config_dir()
    }
}
pub fn cache_dir() -> Option<PathBuf> {
    if MACOS {
        cache_dir_for(true, &Env::from_process())
    } else {
        dirs::cache_dir()
    }
}
pub fn state_dir() -> Option<PathBuf> {
    if MACOS {
        state_dir_for(true, &Env::from_process())
    } else {
        dirs::state_dir().or_else(dirs::data_dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(home: &str) -> Env {
        Env {
            home: Some(home.into()),
            ..Env::default()
        }
    }

    #[test]
    fn macos_defaults_match_linux_layout() {
        let e = env("/Users/a");
        assert_eq!(config_dir_for(true, &e), Some("/Users/a/.config".into()));
        assert_eq!(cache_dir_for(true, &e), Some("/Users/a/.cache".into()));
        assert_eq!(
            state_dir_for(true, &e),
            Some("/Users/a/.local/state".into())
        );
    }

    #[test]
    fn macos_honours_xdg() {
        let e = Env {
            home: Some("/Users/a".into()),
            xdg_config_home: Some("/x/c".into()),
            xdg_cache_home: Some("/x/k".into()),
            xdg_state_home: Some("/x/s".into()),
            xdg_data_home: Some("/x/d".into()),
        };
        assert_eq!(config_dir_for(true, &e), Some("/x/c".into()));
        assert_eq!(cache_dir_for(true, &e), Some("/x/k".into()));
        assert_eq!(state_dir_for(true, &e), Some("/x/s".into()));
    }

    #[test]
    fn macos_ignores_relative_xdg() {
        let mut e = env("/Users/a");
        e.xdg_config_home = Some("rel".into());
        assert_eq!(config_dir_for(true, &e), Some("/Users/a/.config".into()));
    }

    #[test]
    fn macos_state_falls_back_to_data() {
        let e = Env {
            xdg_data_home: Some("/x/d".into()),
            ..Env::default()
        };
        assert_eq!(state_dir_for(true, &e), Some("/x/d".into()));
        assert_eq!(config_dir_for(true, &Env::default()), None);
    }

    #[test]
    fn non_macos_delegates_to_dirs() {
        let e = env("/ignored");
        assert_eq!(config_dir_for(false, &e), dirs::config_dir());
        assert_eq!(cache_dir_for(false, &e), dirs::cache_dir());
        assert_eq!(
            state_dir_for(false, &e),
            dirs::state_dir().or_else(dirs::data_dir)
        );
    }
}
