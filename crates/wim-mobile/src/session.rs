//! The object the native side holds.

use std::sync::{Arc, Mutex};

use wim::config::{Config, NavidromeConfig};
use wim::player::Player;
use wim::swimsession::SwimSession;

use crate::driver::Driver;
use crate::{Bubble, PlatformPlayer, PlaylistEmulator, WimConfig, WimError};

#[derive(uniffi::Object)]
pub struct WimSession {
    driver: Mutex<Driver>,
    /// Present when the engine is native; tests drive a `FakePlayer`.
    emulator: Option<Arc<PlaylistEmulator>>,
}

/// Points everything `wim` finds through `dirs` at the app's own
/// directory: on iOS and Android there is no usual home to write to.
fn confine_to(data_dir: &str) -> Result<(), WimError> {
    let root = std::path::Path::new(data_dir);
    for (var, sub) in [
        ("HOME", ""),
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_CACHE_HOME", "cache"),
    ] {
        let dir = root.join(sub);
        std::fs::create_dir_all(&dir).map_err(|e| WimError::Setup {
            message: format!("cannot create {}: {e}", dir.display()),
        })?;
        std::env::set_var(var, &dir);
    }
    Ok(())
}

#[uniffi::export]
impl WimSession {
    /// The password stays in this process's memory; no config file is
    /// written, so nothing of it reaches `data_dir`.
    #[uniffi::constructor]
    pub fn new(
        config: WimConfig,
        player: Arc<dyn PlatformPlayer>,
    ) -> Result<Arc<WimSession>, WimError> {
        confine_to(&config.data_dir)?;
        let cfg = Config {
            navidrome_servers: vec![NavidromeConfig {
                url: config.server_url,
                username: config.username,
                password: Some(config.password),
                password_file: None,
                max_bitrate_kbps: None,
                transcode_format: None,
            }],
            ..Config::default()
        };
        let emulator = Arc::new(PlaylistEmulator::new(player));
        Ok(Self::from_parts(cfg, emulator.clone(), Some(emulator)))
    }

    /// Queues one command (one line) and returns its id.
    pub fn submit(&self, text: String) -> u64 {
        self.driver.lock().unwrap().submit(&text)
    }

    /// Call every ~100ms from the UI; returns what arrived since.
    pub fn tick(&self) -> Vec<Bubble> {
        self.driver.lock().unwrap().tick()
    }

    /// The engine's position, in seconds; `duration` 0 if unknown.
    pub fn on_time(&self, secs: f64, duration: f64) {
        if let Some(e) = &self.emulator {
            e.on_time(secs, duration);
        }
    }

    /// The engine finished the url it was given.
    pub fn on_ended(&self) {
        if let Some(e) = &self.emulator {
            e.on_ended();
        }
    }

    /// The engine could not play the url; reported, then skipped.
    pub fn on_error(&self, message: String) {
        self.driver
            .lock()
            .unwrap()
            .session
            .console
            .fail(format!("playback failed: {message}"));
        self.on_ended();
    }
}

impl WimSession {
    /// The seam tests and the example use: any `Player`, any config.
    pub fn from_parts(
        cfg: Config,
        player: Arc<dyn Player + Send + Sync>,
        emulator: Option<Arc<PlaylistEmulator>>,
    ) -> Arc<WimSession> {
        Arc::new(WimSession {
            driver: Mutex::new(Driver::new(SwimSession::new(cfg), player)),
            emulator,
        })
    }
}
