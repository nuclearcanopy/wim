mod config;

fn main() -> anyhow::Result<()> {
    let cfg = config::load_or_init()?;
    println!("{cfg:#?}");
    Ok(())
}
