#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use wim::config::Config;
use wim::player::Player;
use wim_mobile::{confine_to, Bubble, PlaylistEmulator, WimSession};

static ENV: Mutex<()> = Mutex::new(());

/// `confine_to` sets process-wide environment, so sessions are built
/// and used one test at a time.
pub fn serial() -> MutexGuard<'static, ()> {
    ENV.lock().unwrap_or_else(|e| e.into_inner())
}

fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("wim-mobile-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A config over a library of empty `.mp3` files named as given.
pub fn library(name: &str, tracks: &[&str]) -> Config {
    let dir = temp(name);
    let music = dir.join("music");
    std::fs::create_dir_all(&music).unwrap();
    for t in tracks {
        std::fs::write(music.join(format!("{t}.mp3")), b"").unwrap();
    }
    confine_to(dir.join("home").to_str().unwrap()).unwrap();
    Config {
        library_paths: vec![music.to_string_lossy().into_owned()],
        ..Config::default()
    }
}

pub fn over(cfg: Config, player: Arc<dyn Player + Send + Sync>) -> Arc<WimSession> {
    let s = WimSession::from_parts(cfg, player, None);
    s.wait_ready();
    s
}

pub fn over_emulator(cfg: Config, emu: &Arc<PlaylistEmulator>) -> Arc<WimSession> {
    let s = WimSession::from_parts(cfg, emu.clone(), Some(emu.clone()));
    s.wait_ready();
    s
}

/// Ticks until nothing is running or waiting (or five seconds pass)
/// and returns every bubble; `_id` documents which message it is for.
pub fn answer(s: &WimSession, _id: u64) -> Vec<Bubble> {
    let end = Instant::now() + Duration::from_secs(5);
    let mut all = Vec::new();
    while Instant::now() < end {
        all.extend(s.tick());
        if !s.busy() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    all
}
