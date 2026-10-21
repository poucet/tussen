//! A take as one `.zip`, to share (`export`) and to bring back (`import`,
//! or `replay <file.zip>` directly).
//!
//! ```text
//! <take>.zip
//!   tussen-take.toml   the manifest: format, the take's directory name
//!   take.toml          byte for byte (deflated, as are the next two)
//!   events.bin         up to its last whole record
//!   hub.toml           when the take has one
//!   audio.wav          its whole frames, stored (deflate barely shrinks PCM); left out by --no-audio
//! ```
//!
//! An import writes an ordinary take directory, the same files, so
//! recording and replay know nothing of zips. It
//! refuses a zip without the manifest, of another format, or holding
//! anything outside that list (which also rules out zip-slip paths).
//!
//! **Identity**: a zip whose `take.toml` is byte-identical to a take's
//! already in the takes directory, with audio the same way (both or
//! neither), *is* that take — `take.toml` carries the name, the start
//! wall time to the ms, the length and the event count. Importing it again
//! reuses that directory. A different take under a taken name gets a
//! `-2`, `-3`, … suffix; nothing is ever overwritten.

use std::fs::{self, File};
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::take::{self, Meta, Records, Take};

/// The format this build writes and the only one it reads.
pub const FORMAT: u32 = 1;

const MANIFEST: &str = "tussen-take.toml";
const TAKE_TOML: &str = "take.toml";
const EVENTS: &str = "events.bin";
const HUB_TOML: &str = "hub.toml";
const AUDIO_WAV: &str = "audio.wav";
/// Everything a take zip may hold.
const ENTRIES: [&str; 5] = [MANIFEST, TAKE_TOML, EVENTS, HUB_TOML, AUDIO_WAV];

/// `tussen-take.toml`.
#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    format: u32,
    /// The take's directory name where it was recorded.
    take: String,
}

/// What [`export`] wrote.
#[derive(Debug)]
pub struct Exported {
    pub path: PathBuf,
    /// The zip's size.
    pub bytes: u64,
    /// Audio frames in the zip.
    pub frames: u64,
}

/// Write the take at `dir` to the zip `out` (replaced if it exists),
/// with its audio or without.
pub fn export(dir: &Path, out: &Path, audio: bool) -> anyhow::Result<Exported> {
    let t = Take::open(dir)?;
    let name = dir_name(dir);
    let take_toml = fs::read(dir.join(TAKE_TOML))?;
    let log = fs::read(dir.join(EVENTS))?;
    let mut records = Records::new(&log)?;
    records.by_ref().for_each(drop);
    let log = &log[..records.end()];
    let hub_toml = fs::read(dir.join(HUB_TOML)).ok();
    let pcm = if audio { t.audio_bytes()? } else { None };
    let frames = if pcm.is_some() { t.frames } else { 0 };
    let manifest = Manifest { format: FORMAT, take: name };

    let file = File::create(out).with_context(|| format!("creating {}", out.display()))?;
    let mut zip = ZipWriter::new(BufWriter::new(file));
    let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    let put = |zip: &mut ZipWriter<_>, name: &str, bytes: &[u8]| -> anyhow::Result<()> {
        zip.start_file(name, deflated)?;
        zip.write_all(bytes)?;
        Ok(())
    };
    put(&mut zip, MANIFEST, toml::to_string(&manifest)?.as_bytes())?;
    put(&mut zip, TAKE_TOML, &take_toml)?;
    put(&mut zip, EVENTS, log)?;
    if let Some(h) = &hub_toml {
        put(&mut zip, HUB_TOML, h)?;
    }
    if let Some(mut pcm) = pcm {
        // A fresh header: a take cut off mid-recording may carry a stale one.
        let data = frames * t.meta.channels.max(1) as u64 * 2;
        let stored = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Stored)
            .large_file(data + take::WAV_HEADER >= u32::MAX as u64);
        zip.start_file(AUDIO_WAV, stored)?;
        let header_data = data.min(u32::MAX as u64 - take::WAV_HEADER) as u32;
        zip.write_all(&take::wav_header(t.meta.sample_rate, t.meta.channels, header_data))?;
        std::io::copy(&mut pcm, &mut zip)?;
    }
    zip.finish()?.flush()?;
    Ok(Exported { path: out.to_owned(), bytes: fs::metadata(out)?.len(), frames })
}

/// What [`import`] did.
#[derive(Debug)]
pub struct Imported {
    pub dir: PathBuf,
    /// The takes directory already held this take; nothing was written.
    pub reused: bool,
}

/// Unpack the take zip `zip` into `takes_dir` (see the module doc for
/// identity and naming).
pub fn import(zip: &Path, takes_dir: &Path) -> anyhow::Result<Imported> {
    let what = || format!("{} is not a tussen take zip", zip.display());
    let mut ar = ZipArchive::new(File::open(zip).with_context(|| format!("opening {}", zip.display()))?)
        .with_context(what)?;
    for i in 0..ar.len() {
        let name = ar.by_index(i)?.name().to_owned();
        anyhow::ensure!(ENTRIES.contains(&name.as_str()), "{}: unexpected entry {name:?}", what());
    }
    let manifest: Manifest = toml::from_str(&read_entry(&mut ar, MANIFEST).with_context(what)?.map(String::from_utf8).transpose()?.unwrap_or_default())
        .with_context(|| format!("{}: reading {MANIFEST}", what()))?;
    anyhow::ensure!(
        manifest.format == FORMAT,
        "{}: take zip format {}, this simply-tussen reads format {FORMAT} (a newer simply-tussen made it?)",
        zip.display(),
        manifest.format
    );
    let need = |ar: &mut ZipArchive<File>, name: &str| {
        read_entry(ar, name)?.ok_or_else(|| anyhow::anyhow!("{}: no {name}", zip.display()))
    };
    let take_toml = need(&mut ar, TAKE_TOML)?;
    toml::from_str::<Meta>(std::str::from_utf8(&take_toml)?).context("reading take.toml")?;
    let events = need(&mut ar, EVENTS)?;
    Records::new(&events)?;
    let hub_toml = read_entry(&mut ar, HUB_TOML)?;
    let has_audio = ar.index_for_name(AUDIO_WAV).is_some();

    if let Some(dir) = take::list(takes_dir).into_iter().find(|d| same_take(d, &take_toml, has_audio)) {
        return Ok(Imported { dir, reused: true });
    }
    fs::create_dir_all(takes_dir).with_context(|| format!("creating {}", takes_dir.display()))?;
    let base = Some(take::sanitize(&manifest.take)).filter(|n| !n.is_empty()).unwrap_or_else(|| "imported".into());
    let dir = (1..)
        .map(|n| takes_dir.join(if n == 1 { base.clone() } else { format!("{base}-{n}") }))
        .find(|d| !d.exists())
        .unwrap();
    // Written beside under a hidden name, then renamed: a take directory
    // is never half there. take.toml last — it is what makes a take.
    let tmp = takes_dir.join(format!(".import-{}", dir_name(&dir)));
    let _ = fs::remove_dir_all(&tmp);
    let written = (|| -> anyhow::Result<()> {
        fs::create_dir(&tmp)?;
        fs::write(tmp.join(EVENTS), &events)?;
        if let Some(h) = &hub_toml {
            fs::write(tmp.join(HUB_TOML), h)?;
        }
        if has_audio {
            let mut wav = ar.by_name(AUDIO_WAV)?;
            let mut out = BufWriter::new(File::create(tmp.join(AUDIO_WAV))?);
            std::io::copy(&mut wav, &mut out)?;
            out.flush()?;
        }
        fs::write(tmp.join(TAKE_TOML), &take_toml)?;
        fs::rename(&tmp, &dir)?;
        Ok(())
    })();
    if let Err(e) = written {
        let _ = fs::remove_dir_all(&tmp);
        return Err(e.context(format!("importing {} into {}", zip.display(), takes_dir.display())));
    }
    Ok(Imported { dir, reused: false })
}

/// What `replay` takes: a take zip from any path (imported into `takes_dir`
/// first, or found there already), else as [`take::resolve`].
pub fn resolve(takes_dir: &Path, which: &str) -> anyhow::Result<PathBuf> {
    let p = Path::new(which);
    if p.is_file() && p.extension().is_some_and(|e| e.eq_ignore_ascii_case("zip")) {
        let imported = import(p, takes_dir)?;
        let verb = if imported.reused { "already imported as" } else { "imported to" };
        log::info!("{} {verb} {}", p.display(), imported.dir.display());
        return Ok(imported.dir);
    }
    take::resolve(takes_dir, which)
}

fn dir_name(dir: &Path) -> String {
    dir.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned())
}

/// An entry's bytes, None when absent.
fn read_entry(ar: &mut ZipArchive<File>, name: &str) -> anyhow::Result<Option<Vec<u8>>> {
    let mut f = match ar.by_name(name) {
        Ok(f) => f,
        Err(zip::result::ZipError::FileNotFound) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let mut v = Vec::with_capacity(f.size().min(1 << 26) as usize);
    f.read_to_end(&mut v)?;
    Ok(Some(v))
}

fn same_take(dir: &Path, take_toml: &[u8], has_audio: bool) -> bool {
    let wav = fs::metadata(dir.join(AUDIO_WAV)).map_or(0, |m| m.len());
    fs::read(dir.join(TAKE_TOML)).is_ok_and(|t| t == take_toml) && (wav > take::WAV_HEADER) == has_audio
}
