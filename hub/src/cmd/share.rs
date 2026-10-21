//! `simply-tussen export` / `import` — a take as one zip, to share.

use std::path::{Path, PathBuf};

use tussen_hub::share;
use tussen_hub::take;

pub fn export(takes_dir: &Path, which: &str, out: Option<PathBuf>, audio: bool) -> anyhow::Result<()> {
    let dir = take::resolve(takes_dir, which)?;
    let name = dir.file_name().map_or_else(|| "take".into(), |n| n.to_string_lossy().into_owned());
    let out = out.unwrap_or_else(|| PathBuf::from(format!("{name}.zip")));
    let e = share::export(&dir, &out, audio)?;
    let audio = if e.frames > 0 { "with audio" } else { "no audio" };
    println!("{} — {:.2} MB ({audio})", e.path.display(), e.bytes as f64 / 1e6);
    Ok(())
}

pub fn import(zip: &Path, takes_dir: &Path) -> anyhow::Result<()> {
    let i = share::import(zip, takes_dir)?;
    let name = i.dir.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    if i.reused {
        println!("{name} (already imported, in {})", takes_dir.display());
    } else {
        println!("{name} (imported into {})", takes_dir.display());
    }
    Ok(())
}
