//! The emulator behind a recording native engine.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{answer, library, over_emulator, serial};
use wim_mobile::{PlatformPlayer, PlaylistEmulator, WimSession};

#[derive(Default)]
struct Native(Mutex<Vec<String>>);

impl Native {
    fn log(&self, s: String) {
        self.0.lock().unwrap().push(s);
    }
    fn calls(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
    fn loads(&self) -> Vec<String> {
        let calls = self.calls();
        calls
            .into_iter()
            .filter(|c| c.starts_with("load "))
            .collect()
    }
}

impl PlatformPlayer for Native {
    fn load(&self, url: String) {
        self.log(format!("load {url}"));
    }
    fn play(&self) {
        self.log("play".into());
    }
    fn pause(&self) {
        self.log("pause".into());
    }
    fn seek(&self, secs: f64) {
        self.log(format!("seek {secs}"));
    }
    fn set_volume(&self, level: f64) {
        self.log(format!("volume {level}"));
    }
    fn stop(&self) {
        self.log("stop".into());
    }
}

fn rig(name: &str) -> (Arc<WimSession>, Arc<PlaylistEmulator>, Arc<Native>) {
    let native = Arc::new(Native::default());
    let emu = Arc::new(PlaylistEmulator::new(native.clone()));
    let s = over_emulator(library(name, &["crest", "dawn"]), &emu);
    (s, emu, native)
}

fn run(s: &WimSession, line: &str) -> Vec<wim_mobile::Bubble> {
    let id = s.submit(line.into());
    answer(s, id)
}

#[test]
fn ended_advances_the_queue_then_finishes_it() {
    let _g = serial();
    let (s, _emu, native) = rig("advance");
    run(&s, "play(crest)");
    run(&s, "queue(dawn)");
    assert_eq!(native.loads().len(), 1, "{:?}", native.calls());
    s.on_ended();
    let said = s.tick();
    let loads = native.loads();
    assert!(loads.len() == 2 && loads[1].contains("dawn"), "{loads:?}");
    assert!(
        said.iter().any(|b| b.text.contains("now playing")),
        "{said:?}"
    );
    s.on_ended();
    let said = s.tick();
    assert!(
        said.iter().any(|b| b.text.contains("queue finished")),
        "{said:?}"
    );
}

#[test]
fn loop_one_reloads_the_same_track_when_it_ends() {
    let _g = serial();
    let (s, _emu, native) = rig("loop");
    run(&s, "play(crest)");
    run(&s, "loop(one)");
    s.on_ended();
    s.tick();
    let loads = native.loads();
    assert!(loads.len() == 2 && loads[0] == loads[1], "{loads:?}");
}

#[test]
fn a_silence_is_a_timer_not_a_load() {
    let _g = serial();
    let (s, emu, native) = rig("silence");
    run(&s, "play(crest)");
    run(&s, "silence(2s)");
    s.on_ended();
    s.tick();
    assert_eq!(native.loads().len(), 1, "{:?}", native.calls());
    assert!(native.calls().contains(&"stop".to_string()));
    emu.run_clock(Duration::from_secs(3));
    let said = s.tick();
    assert!(
        said.iter().any(|b| b.text.contains("queue finished")),
        "{said:?}"
    );
}

#[test]
fn new_local_plays_a_folder_through_the_native_engine() {
    let _g = serial();
    let dir = std::env::temp_dir().join(format!("wim-mobile-local-{}", std::process::id()));
    let music = dir.join("music");
    std::fs::create_dir_all(&music).unwrap();
    std::fs::write(music.join("crest.mp3"), b"").unwrap();
    let native = Arc::new(Native::default());
    let s = WimSession::new_local(
        dir.join("home").to_string_lossy().into_owned(),
        music.to_string_lossy().into_owned(),
        native.clone(),
    )
    .unwrap();
    let id = s.submit("play(crest)".into());
    answer(&s, id);
    assert!(
        native.loads().iter().any(|l| l.contains("crest")),
        "{:?}",
        native.calls()
    );
}
