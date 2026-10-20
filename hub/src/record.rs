//! Recording on demand: the console's Record button (and `run --record`)
//! start and stop takes while the hub runs.
//!
//! One [`Slot`] holds the take being written, if any. The router, the
//! audio sender and the link thread each look at it (the router once per
//! loop round, the link thread once per tick, the sender per packet) and
//! hand their records to whatever recorder is there. A take started
//! mid-run is **seeded**: the router writes the last value of every address
//! it has routed at the take's t = 0, and the link thread writes the
//! current timeline at once — so a replay from the start has a consistent
//! world.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime};

use serde::Serialize;

use crate::generators::audio::AudioLink;
use crate::take::{self, Meta, Recorder};

/// The take being written, shared with every tap.
pub type Slot = Arc<Mutex<Option<Arc<Recorder>>>>;

/// The current recorder in `slot`.
pub fn current(slot: &Slot) -> Option<Arc<Recorder>> {
    slot.lock().ok().and_then(|g| g.clone())
}

/// Did the recorder change from `last`? (Pointer identity.)
pub fn changed(last: &Option<Arc<Recorder>>, now: &Option<Arc<Recorder>>) -> bool {
    match (last, now) {
        (None, None) => false,
        (Some(a), Some(b)) => !Arc::ptr_eq(a, b),
        _ => true,
    }
}

/// What the console shows (the `record` field of `hello`).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Status {
    /// Recording right now.
    pub on: bool,
    /// The take's directory name, while on (or the last one, after a stop).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub take: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// How long it has been recording, at the moment this was made.
    pub elapsed_ms: u64,
    /// Why the last start failed, if it did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Starts and stops takes into `dir`.
pub struct Control {
    dir: Option<PathBuf>,
    meta: Meta,
    config_text: Option<String>,
    slot: Slot,
    audio: Option<Arc<AudioLink>>,
    last: Mutex<(Option<PathBuf>, Option<String>)>,
}

impl Control {
    /// `dir`: where takes go (None: recording refuses, saying why).
    /// `meta`: the format of the audio and the quantum. `audio`: the sender
    /// to tap, if there is one.
    pub fn new(dir: Option<PathBuf>, meta: Meta, config_text: Option<String>, audio: Option<Arc<AudioLink>>) -> Arc<Control> {
        Arc::new(Control {
            dir,
            meta,
            config_text,
            slot: Arc::new(Mutex::new(None)),
            audio,
            last: Mutex::new((None, None)),
        })
    }

    pub fn slot(&self) -> Slot {
        Arc::clone(&self.slot)
    }

    /// Start a new take now. Refused while one is being written.
    pub fn start(&self, name: Option<&str>) -> anyhow::Result<Arc<Recorder>> {
        let result = self.try_start(name);
        self.last.lock().unwrap().1 = result.as_ref().err().map(|e| format!("{e:#}"));
        result
    }

    fn try_start(&self, name: Option<&str>) -> anyhow::Result<Arc<Recorder>> {
        let mut slot = self.slot.lock().unwrap();
        anyhow::ensure!(slot.is_none(), "already recording");
        let dir = self.dir.as_ref().ok_or_else(|| anyhow::anyhow!("no takes directory (HOME unset; use --takes-dir)"))?;
        let name = name.map(str::trim).filter(|n| !n.is_empty());
        let base = take::dir_name(SystemTime::now(), name);
        let mut path = dir.join(&base);
        for n in 2.. {
            if !path.exists() {
                break;
            }
            path = dir.join(format!("{base}-{n}"));
        }
        let meta = Meta { name: name.unwrap_or_default().to_owned(), ..self.meta.clone() };
        let rec = Recorder::start(&path, meta, self.config_text.as_deref())?;
        if let Some(a) = &self.audio {
            let samples = self.meta.packet_frames * self.meta.channels.max(1) as usize;
            *a.rec.lock().unwrap() = Some(rec.audio_tap(samples));
        }
        log::info!("record: recording → {}", path.display());
        *slot = Some(Arc::clone(&rec));
        self.last.lock().unwrap().0 = Some(path);
        Ok(rec)
    }

    /// Stop the take being written (it is complete on disk when this
    /// returns). Returns its directory.
    pub fn stop(&self) -> Option<PathBuf> {
        let rec = self.slot.lock().unwrap().take()?;
        if let Some(a) = &self.audio {
            *a.rec.lock().unwrap() = None;
        }
        rec.finish();
        log::info!("record: stopped {} ({} record(s) dropped)", rec.dir().display(), rec.dropped());
        Some(rec.dir().to_owned())
    }

    pub fn status(&self) -> Status {
        let rec = current(&self.slot);
        let last = self.last.lock().unwrap();
        let path = rec.as_ref().map(|r| r.dir().to_owned()).or_else(|| last.0.clone());
        Status {
            on: rec.is_some(),
            take: path.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()),
            path: path.map(|p| p.display().to_string()),
            elapsed_ms: rec.map_or(0, |r| Instant::now().saturating_duration_since(r.start_instant()).as_millis() as u64),
            error: last.1.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_take_starts_and_stops_and_a_second_start_is_refused() {
        let dir = std::env::temp_dir().join(format!("tussen-record-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let audio = Arc::new(AudioLink::default());
        let meta = Meta { sample_rate: 48000, channels: 2, packet_frames: 256, quantum: 4.0, ..Meta::default() };
        let c = Control::new(Some(dir.clone()), meta, None, Some(Arc::clone(&audio)));
        assert!(!c.status().on);
        let rec = c.start(Some("sound check")).unwrap();
        assert!(Arc::ptr_eq(&current(&c.slot()).unwrap(), &rec));
        assert!(audio.rec.lock().unwrap().is_some(), "the audio sender is tapped");
        let s = c.status();
        assert!(s.on && s.take.as_deref().unwrap().ends_with("-sound-check"), "{s:?}");
        assert_eq!(c.start(None).err().unwrap().to_string(), "already recording");
        assert_eq!(c.status().error.as_deref(), Some("already recording"));
        let path = c.stop().unwrap();
        assert!(path.join("take.toml").is_file());
        assert!(audio.rec.lock().unwrap().is_none());
        assert!(!c.status().on && c.status().take.is_some(), "the last take is still named");
        assert_eq!(c.stop(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
