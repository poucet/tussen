//! `simply-tussen takes` — the recorded takes, oldest first.

use std::path::Path;

use tussen_hub::take;

pub fn run(dir: &Path) -> anyhow::Result<()> {
    let takes = take::list(dir);
    if takes.is_empty() {
        println!("no takes in {} (record one: simply-tussen run --record)", dir.display());
        return Ok(());
    }
    println!("{:<40} {:>9} {:>9} {:>8}", "take", "length", "size", "events");
    for t in &takes {
        let name = t.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let meta: take::Meta = std::fs::read_to_string(t.join("take.toml"))
            .ok()
            .and_then(|s| toml::from_str(&s).ok())
            .unwrap_or_default();
        let secs = meta.duration_us / 1_000_000;
        let mb = take::size(t) as f64 / 1e6;
        println!("{name:<40} {:>5}:{:02} {:>7.1}MB {:>8}", secs / 60, secs % 60, mb, meta.events);
    }
    println!("in {}", dir.display());
    Ok(())
}
