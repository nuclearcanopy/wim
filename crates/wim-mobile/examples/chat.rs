//! A stdin chat loop over `WimSession` with a `FakePlayer`: no network,
//! no native engine. Try `dry(play(crest))` and `view(queue)`.

use std::io::BufRead;
use std::sync::Arc;
use std::time::{Duration, Instant};

use wim::config::Config;
use wim::player::FakePlayer;
use wim_mobile::{confine_to, BubbleKind, WimSession};

fn main() {
    let dir = std::env::temp_dir().join(format!("wim-mobile-chat-{}", std::process::id()));
    let music = dir.join("music");
    std::fs::create_dir_all(&music).expect("create library");
    for t in ["crest", "tide", "ember"] {
        std::fs::write(music.join(format!("{t}.mp3")), b"").expect("write track");
    }
    confine_to(dir.join("home").to_str().expect("utf-8 path")).expect("confine");
    let cfg = Config {
        library_paths: vec![music.to_string_lossy().into_owned()],
        ..Config::default()
    };
    let player = Arc::new(FakePlayer::new());
    let session = WimSession::from_parts(cfg, player.clone(), None);
    session.wait_ready();

    for line in std::io::stdin().lock().lines().map_while(Result::ok) {
        let line = line.trim();
        if line == "quit" {
            break;
        }
        if line.is_empty() {
            continue;
        }
        let id = session.submit(line.to_string());
        let end = Instant::now() + Duration::from_secs(5);
        while Instant::now() < end {
            for b in session.tick() {
                let who = match b.kind {
                    BubbleKind::Reply => "wim",
                    BubbleKind::System => "sys",
                    BubbleKind::Error => "err",
                };
                println!("[{id}->{}] {who}: {}", b.reply_to, b.text);
            }
            if !session.busy() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    for call in player.calls() {
        println!("player: {call}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
