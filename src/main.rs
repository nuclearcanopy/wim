mod app;
mod config;
mod library;
mod mpv;
mod search;
mod subsonic;
mod term;
mod theme;
mod ui;

use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode};

use ui::banner::Banner;

fn main() -> anyhow::Result<()> {
    let cfg = config::load_or_init()?;
    println!("{cfg:#?}");

    let player = mpv::Mpv::spawn()?;
    let version = player.get_property("mpv-version")?;
    println!("mpv ipc ok: {version}");
    println!("continuing to the banner smoke test...");
    std::thread::sleep(Duration::from_millis(1200));

    let mut guard = term::TerminalGuard::new()?;
    let start = Instant::now();
    loop {
        let phase = start.elapsed().as_secs_f32() * 0.25;
        guard.terminal.draw(|f| {
            f.render_widget(Banner { phase }, f.area());
        })?;

        if event::poll(Duration::from_millis(16))? {
            if let Event::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    _ => {}
                }
            }
        }
    }
    drop(guard);

    let _ = player;
    Ok(())
}
