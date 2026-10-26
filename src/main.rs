mod config;
mod mpv;

fn main() -> anyhow::Result<()> {
    let cfg = config::load_or_init()?;
    println!("{cfg:#?}");

    let player = mpv::Mpv::spawn()?;
    let version = player.get_property("mpv-version")?;
    println!("mpv ipc ok: {version}");
    Ok(())
}
