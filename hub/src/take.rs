//! A take on disk: one directory per recording (design: `docs/design/replay.md` §3).
//!
//! ```text
//! <takes>/2026-10-20T21-14-03-rehearsal/
//!   take.toml    meta, rewritten at every flush (duration, audio lead)
//!   audio.wav    L16 interleaved, exactly what RTP carried; header patched at every flush
//!   events.bin   append-only log: OSC at ingress, sync, Link timeline, audio anchors
//!   hub.toml     the config in use when recording started
//! ```
//!
//! **Crash-safe by construction**: append-only, no footer, no index. The
//! [`Recorder`] flushes every [`FLUSH`] and syncs to disk once a second;
//! [`Take::open`] stops at the last whole record and sizes the WAV from
//! the file, so a take whose hub was killed opens up to its last flush.
//!
//! Every stamp is µs since the take's start on the host clock (`Instant`):
//! the router's `now` for OSC, the RTP packet's stamp for audio, the link
//! thread's host time for the Link timeline.
//!
//! The event log:
//!
//! ```text
//! "TUSSTAKE" u16 version
//! record := u32 len · u8 kind · u64 t_us · payload      (len counts kind + t + payload)
//!   1 osc     u16 origin · raw OSC message bytes
//!   2 origin  u16 id · utf-8 ("192.0.2.21:50123", "hub")
//!   3 sync    u32 signal_delay_ms · u32 td_round_trip_ms
//!   4 link    f64 bpm · f64 beat at t · u8 playing · u8 mode · f64 quantum
//!   5 anchor  u64 frame (the audio frame whose packet was stamped t) · u8 flags
//! ```

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use anyhow::Context;
use crossbeam_channel::{Receiver, Sender};
use serde::{Deserialize, Serialize};
use simply_osc::{Message, Packet};

const MAGIC: &[u8; 8] = b"TUSSTAKE";
const VERSION: u16 = 1;
/// How often the writers flush: a hub crash loses at most this much.
pub const FLUSH: Duration = Duration::from_millis(250);
/// How often a take is synced to disk (power loss).
const FSYNC: Duration = Duration::from_secs(1);
/// Records the router may queue before the recorder drops (and counts).
const QUEUE: usize = 65_536;
/// An audio anchor at least this often, and whenever the packet stamps
/// leave the sample clock by more than [`ANCHOR_SLIP_US`].
const ANCHOR_EVERY_US: u64 = 1_000_000;
const ANCHOR_SLIP_US: i64 = 20_000;

const K_OSC: u8 = 1;
const K_ORIGIN: u8 = 2;
const K_SYNC: u8 = 3;
const K_LINK: u8 = 4;
const K_ANCHOR: u8 = 5;

/// The origin id of hub-made signals (generators: MIDI, pad, clock).
pub const HUB: &str = "hub";

/// `take.toml`.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Meta {
    pub version: u16,
    pub name: String,
    /// UTC wall time at t = 0, ms since the Unix epoch.
    pub started_unix_ms: u64,
    /// The hub clock (the console's `ts`) at t = 0.
    pub hub_us_at_start: u64,
    pub sample_rate: u32,
    pub channels: u16,
    pub packet_frames: usize,
    /// Beats per bar of the recorded timeline.
    pub quantum: f64,
    /// The signal delay in force at the end of the take (design D6).
    pub signal_delay_ms: u32,
    /// Last flush: take length and what it holds.
    pub duration_us: u64,
    pub frames: u64,
    pub events: u64,
}

/// One point of the recorded Link timeline: from `t` on, `bpm`, the beat
/// at `t` being `beat`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LinkPoint {
    pub t: u64,
    pub bpm: f64,
    pub beat: f64,
    pub playing: bool,
    /// 0 waiting, 1 joined, 2 started (`link::Mode`).
    pub mode: u8,
    pub quantum: f64,
}

/// `(frame, t)`: the audio frame whose RTP packet was stamped `t`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Anchor {
    pub frame: u64,
    pub t: u64,
}

/// What the recorder thread is handed.
enum Rec {
    Osc { at: Instant, from: Option<SocketAddr>, bytes: Vec<u8> },
    Sync { at: Instant, signal_delay_ms: u32, td_round_trip_ms: u32 },
    Link { at: Instant, bpm: f64, beat: f64, playing: bool, mode: u8, quantum: f64 },
}

/// The audio sender's side of a recording: whole packets of samples and
/// their stamps, over wait-free rings (the sender never writes a file).
pub struct AudioRecTx {
    samples: rtrb::Producer<f32>,
    stamps: rtrb::Producer<Instant>,
    frames: usize,
    pub dropped: u64,
}

impl AudioRecTx {
    /// One packetized block, stamped `at`. A full ring drops the packet
    /// whole (the next anchor keeps the timing right).
    pub fn push(&mut self, block: &[f32], at: Instant) {
        if self.samples.slots() < block.len() || self.stamps.slots() < 1 || block.len() != self.frames {
            self.dropped += 1;
            return;
        }
        if let Ok(chunk) = self.samples.write_chunk_uninit(block.len()) {
            chunk.fill_from_iter(block.iter().copied());
        }
        let _ = self.stamps.push(at);
    }
}

struct AudioRecRx {
    samples: rtrb::Consumer<f32>,
    stamps: rtrb::Consumer<Instant>,
    block: usize,
}

/// A recording in progress. Hand it to the router ([`crate::hub::Extras`]),
/// the audio sender and the link thread; each `try_send`s, never blocks.
pub struct Recorder {
    tx: Sender<Rec>,
    start: Instant,
    dir: PathBuf,
    dropped: AtomicU64,
    stop: Arc<AtomicBool>,
    audio: Mutex<Option<AudioRecRx>>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Recorder {
    /// Create `dir` (must not exist) and start writing into it.
    pub fn start(dir: &Path, mut meta: Meta, config_text: Option<&str>) -> anyhow::Result<Arc<Recorder>> {
        fs::create_dir_all(dir.parent().unwrap_or(Path::new("."))).with_context(|| format!("creating {}", dir.display()))?;
        fs::create_dir(dir).with_context(|| format!("creating take {}", dir.display()))?;
        if let Some(t) = config_text {
            fs::write(dir.join("hub.toml"), t)?;
        }
        let start = Instant::now();
        meta.version = VERSION;
        meta.started_unix_ms =
            SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
        meta.hub_us_at_start = crate::console::us_at(start);
        let mut events = BufWriter::new(File::create(dir.join("events.bin"))?);
        events.write_all(MAGIC)?;
        events.write_all(&VERSION.to_le_bytes())?;
        events.flush()?;
        let wav = Wav::create(&dir.join("audio.wav"), meta.sample_rate, meta.channels)?;
        let (tx, rx) = crossbeam_channel::bounded(QUEUE);
        let stop = Arc::new(AtomicBool::new(false));
        let rec = Arc::new(Recorder {
            tx,
            start,
            dir: dir.to_owned(),
            dropped: AtomicU64::new(0),
            stop: Arc::clone(&stop),
            audio: Mutex::new(None),
            thread: Mutex::new(None),
        });
        let writer = Writer {
            dir: dir.to_owned(),
            meta,
            start,
            events,
            wav,
            origins: HashMap::new(),
            audio_frames: 0,
            last_anchor: None,
            n_events: 0,
            last_t: 0,
        };
        let weak = Arc::downgrade(&rec);
        let t = std::thread::Builder::new()
            .name("simply-tussen record".into())
            .spawn(move || writer.run(rx, &stop, weak))?;
        *rec.thread.lock().unwrap() = Some(t);
        Ok(rec)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn start_instant(&self) -> Instant {
        self.start
    }

    /// Records dropped because the recorder fell behind.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    fn send(&self, r: Rec) {
        if self.tx.try_send(r).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// A message the router routed, at its `now`. `from` None = the hub.
    pub fn osc(&self, at: Instant, from: Option<SocketAddr>, m: &Message) {
        if let Ok(bytes) = m.encode() {
            self.send(Rec::Osc { at, from, bytes });
        }
    }

    pub fn sync(&self, at: Instant, signal_delay_ms: u32, td_round_trip_ms: u32) {
        self.send(Rec::Sync { at, signal_delay_ms, td_round_trip_ms });
    }

    pub fn link(&self, at: Instant, bpm: f64, beat: f64, playing: bool, mode: u8, quantum: f64) {
        self.send(Rec::Link { at, bpm, beat, playing, mode, quantum });
    }

    /// The audio sender's tap: `frames` samples (interleaved) per packet.
    /// One per recording.
    pub fn audio_tap(&self, packet_samples: usize) -> AudioRecTx {
        let cap = packet_samples * 400; // ~2 s at 187.5 packets/s
        let (samples, srx) = rtrb::RingBuffer::new(cap);
        let (stamps, trx) = rtrb::RingBuffer::new(400);
        *self.audio.lock().unwrap() = Some(AudioRecRx { samples: srx, stamps: trx, block: packet_samples });
        AudioRecTx { samples, stamps, frames: packet_samples, dropped: 0 }
    }

    /// Stop and write everything out (tests, a clean exit). A take is
    /// valid without this.
    pub fn finish(&self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.lock().unwrap().take() {
            let _ = t.join();
        }
    }
}

struct Writer {
    dir: PathBuf,
    meta: Meta,
    start: Instant,
    events: BufWriter<File>,
    wav: Wav,
    origins: HashMap<String, u16>,
    audio_frames: u64,
    last_anchor: Option<Anchor>,
    n_events: u64,
    last_t: u64,
}

impl Writer {
    fn t(&self, at: Instant) -> u64 {
        at.saturating_duration_since(self.start).as_micros() as u64
    }

    fn record(&mut self, kind: u8, t: u64, payload: &[u8]) -> std::io::Result<()> {
        let len = (1 + 8 + payload.len()) as u32;
        self.events.write_all(&len.to_le_bytes())?;
        self.events.write_all(&[kind])?;
        self.events.write_all(&t.to_le_bytes())?;
        self.events.write_all(payload)?;
        self.last_t = self.last_t.max(t);
        Ok(())
    }

    fn origin(&mut self, t: u64, from: Option<SocketAddr>) -> std::io::Result<u16> {
        let name = from.map_or_else(|| HUB.to_owned(), |a| a.to_string());
        if let Some(&id) = self.origins.get(&name) {
            return Ok(id);
        }
        let id = self.origins.len() as u16;
        let mut p = id.to_le_bytes().to_vec();
        p.extend_from_slice(name.as_bytes());
        self.record(K_ORIGIN, t, &p)?;
        self.origins.insert(name, id);
        Ok(id)
    }

    fn write(&mut self, r: Rec) -> std::io::Result<()> {
        match r {
            Rec::Osc { at, from, bytes } => {
                let t = self.t(at);
                let id = self.origin(t, from)?;
                let mut p = id.to_le_bytes().to_vec();
                p.extend_from_slice(&bytes);
                self.n_events += 1;
                self.record(K_OSC, t, &p)
            }
            Rec::Sync { at, signal_delay_ms, td_round_trip_ms } => {
                self.meta.signal_delay_ms = signal_delay_ms;
                let mut p = signal_delay_ms.to_le_bytes().to_vec();
                p.extend_from_slice(&td_round_trip_ms.to_le_bytes());
                let t = self.t(at);
                self.record(K_SYNC, t, &p)
            }
            Rec::Link { at, bpm, beat, playing, mode, quantum } => {
                self.meta.quantum = quantum;
                let mut p = Vec::with_capacity(26);
                p.extend_from_slice(&bpm.to_le_bytes());
                p.extend_from_slice(&beat.to_le_bytes());
                p.push(playing as u8);
                p.push(mode);
                p.extend_from_slice(&quantum.to_le_bytes());
                let t = self.t(at);
                self.record(K_LINK, t, &p)
            }
        }
    }

    /// Move what the audio sender handed over into the WAV, anchoring.
    fn drain_audio(&mut self, audio: &mut AudioRecRx) -> std::io::Result<()> {
        let rate = self.meta.sample_rate.max(1) as u64;
        let channels = self.meta.channels.max(1) as usize;
        while let Ok(at) = audio.stamps.pop() {
            let Ok(chunk) = audio.samples.read_chunk(audio.block) else { break };
            let (a, b) = chunk.as_slices();
            self.wav.write(a)?;
            self.wav.write(b)?;
            chunk.commit_all();
            let t = self.t(at);
            let frame = self.audio_frames;
            let anchor = match self.last_anchor {
                None => true,
                Some(last) => {
                    let expected = last.t + (frame - last.frame) * 1_000_000 / rate;
                    t >= last.t + ANCHOR_EVERY_US || (t as i64 - expected as i64).abs() > ANCHOR_SLIP_US
                }
            };
            if anchor {
                let mut p = frame.to_le_bytes().to_vec();
                p.push(0);
                self.record(K_ANCHOR, t, &p)?;
                self.last_anchor = Some(Anchor { frame, t });
            }
            self.audio_frames += (audio.block / channels) as u64;
        }
        Ok(())
    }

    fn flush(&mut self, sync: bool) -> std::io::Result<()> {
        self.events.flush()?;
        self.wav.flush()?;
        self.meta.duration_us = self.last_t.max(self.start.elapsed().as_micros() as u64);
        self.meta.frames = self.audio_frames;
        self.meta.events = self.n_events;
        if let Ok(text) = toml::to_string(&self.meta) {
            // Beside, then rename over: take.toml is never half-written.
            let tmp = self.dir.join(".take.toml.new");
            if fs::write(&tmp, text).is_ok() {
                let _ = fs::rename(&tmp, self.dir.join("take.toml"));
            }
        }
        if sync {
            self.events.get_ref().sync_data()?;
            self.wav.file.get_ref().sync_data()?;
        }
        Ok(())
    }

    fn run(mut self, rx: Receiver<Rec>, stop: &AtomicBool, rec: std::sync::Weak<Recorder>) {
        let mut last_flush = Instant::now();
        let mut last_sync = Instant::now();
        let mut audio: Option<AudioRecRx> = None;
        let _ = self.flush(false);
        loop {
            let done = stop.load(Ordering::Relaxed);
            let r = rx.recv_timeout(Duration::from_millis(20));
            let mut result = Ok(());
            if let Ok(r) = r {
                result = self.write(r);
                while let (Ok(()), Ok(r)) = (&result, rx.try_recv()) {
                    result = self.write(r);
                }
            }
            if audio.is_none() {
                match rec.upgrade() {
                    Some(rec) => audio = rec.audio.lock().unwrap().take(),
                    None => {
                        let _ = self.flush(true);
                        return;
                    }
                }
            }
            if let (Ok(()), Some(a)) = (&result, audio.as_mut()) {
                result = self.drain_audio(a);
            }
            if result.is_ok() && (done || last_flush.elapsed() >= FLUSH) {
                last_flush = Instant::now();
                let sync = done || last_sync.elapsed() >= FSYNC;
                if sync {
                    last_sync = Instant::now();
                }
                result = self.flush(sync);
            }
            if let Err(e) = result {
                // Disk full or a write error stops the recording, never the hub.
                log::warn!("record: {e}; recording stopped, the hub goes on ({})", self.dir.display());
                return;
            }
            if done {
                return;
            }
        }
    }
}

/// A WAV whose header is patched to the data written at every flush.
struct Wav {
    file: BufWriter<File>,
    data: u64,
}

const WAV_HEADER: u64 = 44;

impl Wav {
    fn create(path: &Path, rate: u32, channels: u16) -> std::io::Result<Wav> {
        let mut file = BufWriter::new(File::create(path)?);
        file.write_all(&wav_header(rate, channels, 0))?;
        Ok(Wav { file, data: 0 })
    }

    fn write(&mut self, samples: &[f32]) -> std::io::Result<()> {
        for &s in samples {
            let v = (s.clamp(-1.0, 1.0) * 32767.0).round() as i16;
            self.file.write_all(&v.to_le_bytes())?;
        }
        self.data += samples.len() as u64 * 2;
        Ok(())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()?;
        let f = self.file.get_mut();
        let data = self.data.min(u32::MAX as u64 - WAV_HEADER) as u32;
        f.seek(SeekFrom::Start(4))?;
        f.write_all(&(data + 36).to_le_bytes())?;
        f.seek(SeekFrom::Start(40))?;
        f.write_all(&data.to_le_bytes())?;
        f.seek(SeekFrom::End(0))?;
        Ok(())
    }
}

fn wav_header(rate: u32, channels: u16, data: u32) -> [u8; 44] {
    let mut h = [0u8; 44];
    let block = channels * 2;
    h[0..4].copy_from_slice(b"RIFF");
    h[4..8].copy_from_slice(&(data + 36).to_le_bytes());
    h[8..12].copy_from_slice(b"WAVE");
    h[12..16].copy_from_slice(b"fmt ");
    h[16..20].copy_from_slice(&16u32.to_le_bytes());
    h[20..22].copy_from_slice(&1u16.to_le_bytes());
    h[22..24].copy_from_slice(&channels.to_le_bytes());
    h[24..28].copy_from_slice(&rate.to_le_bytes());
    h[28..32].copy_from_slice(&(rate * block as u32).to_le_bytes());
    h[32..34].copy_from_slice(&block.to_le_bytes());
    h[34..36].copy_from_slice(&16u16.to_le_bytes());
    h[36..40].copy_from_slice(b"data");
    h[40..44].copy_from_slice(&data.to_le_bytes());
    h
}

/// One replayable OSC record.
#[derive(Clone, Debug)]
pub struct Event {
    pub t: u64,
    pub origin: u16,
    pub msg: Message,
}

/// A take, opened for replay: the whole log in memory, the WAV read on
/// demand.
pub struct Take {
    pub dir: PathBuf,
    pub meta: Meta,
    pub events: Vec<Event>,
    pub origins: HashMap<u16, String>,
    pub links: Vec<LinkPoint>,
    /// `(t, signal_delay_ms)`.
    pub syncs: Vec<(u64, u32)>,
    pub anchors: Vec<Anchor>,
    /// Whole frames in the WAV (from the file length, not the header).
    pub frames: u64,
    wav: Option<File>,
}

impl Take {
    pub fn open(dir: &Path) -> anyhow::Result<Take> {
        let meta: Meta = toml::from_str(
            &fs::read_to_string(dir.join("take.toml")).with_context(|| format!("{} is not a take", dir.display()))?,
        )
        .context("reading take.toml")?;
        let mut log = Vec::new();
        File::open(dir.join("events.bin"))?.read_to_end(&mut log)?;
        anyhow::ensure!(log.len() >= 10 && &log[..8] == MAGIC, "events.bin: not a take log");
        let mut take = Take {
            dir: dir.to_owned(),
            meta,
            events: Vec::new(),
            origins: HashMap::new(),
            links: Vec::new(),
            syncs: Vec::new(),
            anchors: Vec::new(),
            frames: 0,
            wav: None,
        };
        let mut i = 10;
        // A torn last record (a crash mid-write) is shorter than its length: stop there.
        while i + 4 <= log.len() {
            let len = u32::from_le_bytes(log[i..i + 4].try_into().unwrap()) as usize;
            if len < 9 || i + 4 + len > log.len() {
                break;
            }
            let rec = &log[i + 4..i + 4 + len];
            i += 4 + len;
            let (kind, t, p) = (rec[0], u64::from_le_bytes(rec[1..9].try_into().unwrap()), &rec[9..]);
            take.decode(kind, t, p);
        }
        // Arrival order is time order; a stable sort makes sure of it.
        take.events.sort_by_key(|e| e.t);
        take.links.sort_by_key(|l| l.t);
        let channels = take.meta.channels.max(1) as u64;
        if let Ok(f) = File::open(dir.join("audio.wav")) {
            let len = f.metadata()?.len();
            take.frames = len.saturating_sub(WAV_HEADER) / (2 * channels);
            take.wav = Some(f);
        }
        // An anchor past the frames that made it to disk is dropped with them.
        let frames = take.frames;
        take.anchors.retain(|a| a.frame < frames.max(1));
        take.meta.duration_us = take.meta.duration_us.max(take.end_us());
        Ok(take)
    }

    fn decode(&mut self, kind: u8, t: u64, p: &[u8]) {
        let f64_at = |o: usize| p.get(o..o + 8).map(|b| f64::from_le_bytes(b.try_into().unwrap()));
        match kind {
            K_OSC if p.len() > 2 => {
                let origin = u16::from_le_bytes([p[0], p[1]]);
                if let Ok(Packet::Message(msg)) = simply_osc::wire::parse(&p[2..]) {
                    self.events.push(Event { t, origin, msg });
                }
            }
            K_ORIGIN if p.len() >= 2 => {
                self.origins.insert(u16::from_le_bytes([p[0], p[1]]), String::from_utf8_lossy(&p[2..]).into_owned());
            }
            K_SYNC if p.len() >= 8 => self.syncs.push((t, u32::from_le_bytes(p[0..4].try_into().unwrap()))),
            K_LINK if p.len() >= 26 => {
                if let (Some(bpm), Some(beat), Some(quantum)) = (f64_at(0), f64_at(8), f64_at(18)) {
                    self.links.push(LinkPoint { t, bpm, beat, playing: p[16] != 0, mode: p[17], quantum });
                }
            }
            K_ANCHOR if p.len() >= 8 => {
                self.anchors.push(Anchor { frame: u64::from_le_bytes(p[0..8].try_into().unwrap()), t });
            }
            _ => {}
        }
    }

    /// The last stamp in the log or the end of the audio.
    fn end_us(&self) -> u64 {
        let ev = self.events.last().map_or(0, |e| e.t);
        let link = self.links.last().map_or(0, |l| l.t);
        let audio = self.anchors.last().map_or(0, |a| {
            a.t + (self.frames.saturating_sub(a.frame)) * 1_000_000 / self.meta.sample_rate.max(1) as u64
        });
        ev.max(link).max(audio)
    }

    /// The signal delay at the end of the take (design D6), else meta's.
    pub fn signal_delay_ms(&self) -> u32 {
        self.syncs.last().map_or(self.meta.signal_delay_ms, |s| s.1)
    }

    /// The audio frame stamped `t` µs: between anchors the sample clock
    /// rules; before the first and after the last, extrapolated.
    pub fn frame_at(&self, t: u64) -> Option<u64> {
        let rate = self.meta.sample_rate.max(1) as f64;
        if self.frames == 0 || self.anchors.is_empty() {
            return None;
        }
        let i = self.anchors.partition_point(|a| a.t <= t);
        let a = self.anchors[i.saturating_sub(1)];
        let f = a.frame as f64 + (t as f64 - a.t as f64) * rate / 1e6;
        // Never past the next anchor's frame (a gap is skipped, not replayed early).
        let f = match self.anchors.get(i) {
            Some(next) if i > 0 => f.min(next.frame as f64),
            _ => f,
        };
        Some(f.max(0.0).round() as u64)
    }

    /// Read interleaved samples from `frame` into `out` (zeros past the end).
    pub fn read_audio(&self, frame: u64, out: &mut [f32]) {
        out.fill(0.0);
        let Some(f) = &self.wav else { return };
        let channels = self.meta.channels.max(1) as u64;
        if frame >= self.frames {
            return;
        }
        let n = out.len().min(((self.frames - frame) * channels) as usize);
        let mut buf = vec![0u8; n * 2];
        #[cfg(unix)]
        let ok = {
            use std::os::unix::fs::FileExt;
            f.read_exact_at(&mut buf, WAV_HEADER + frame * channels * 2).is_ok()
        };
        #[cfg(not(unix))]
        let ok = {
            let mut f = f;
            f.seek(SeekFrom::Start(WAV_HEADER + frame * channels * 2)).is_ok() && f.read_exact(&mut buf).is_ok()
        };
        if ok {
            for (o, b) in out.iter_mut().zip(buf.chunks_exact(2)) {
                *o = i16::from_le_bytes([b[0], b[1]]) as f32 / 32767.0;
            }
        }
    }
}

/// The takes directory: `$XDG_DATA_HOME/tussen/takes`, else
/// `~/.local/share/tussen/takes` (macOS too, as for the config).
pub fn default_dir(xdg_data_home: Option<&Path>, home: Option<&Path>) -> Option<PathBuf> {
    let base = match xdg_data_home {
        Some(x) if x.is_absolute() => x.to_owned(),
        _ => home.filter(|h| !h.as_os_str().is_empty())?.join(".local").join("share"),
    };
    Some(base.join("tussen").join("takes"))
}

/// A new take's directory name: UTC `2026-10-20T21-14-03`, plus `-name`.
pub fn dir_name(now: SystemTime, name: Option<&str>) -> String {
    let secs = now.duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Civil from days (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    let stamp = format!("{y:04}-{m:02}-{d:02}T{:02}-{:02}-{:02}", rem / 3600, rem / 60 % 60, rem % 60);
    match name.map(sanitize).filter(|n| !n.is_empty()) {
        Some(n) => format!("{stamp}-{n}"),
        None => stamp,
    }
}

fn sanitize(name: &str) -> String {
    name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' }).collect()
}

/// Every take in `dir`, oldest first (names sort by time).
pub fn list(dir: &Path) -> Vec<PathBuf> {
    let mut takes: Vec<PathBuf> = fs::read_dir(dir)
        .map(|r| r.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.join("take.toml").is_file()).collect())
        .unwrap_or_default();
    takes.sort();
    takes
}

/// `latest`, a name in `dir`, or a path.
pub fn resolve(dir: &Path, which: &str) -> anyhow::Result<PathBuf> {
    if which == "latest" {
        return list(dir).pop().ok_or_else(|| anyhow::anyhow!("no takes in {}", dir.display()));
    }
    let p = Path::new(which);
    if p.join("take.toml").is_file() {
        return Ok(p.to_owned());
    }
    let named = dir.join(which);
    if named.join("take.toml").is_file() {
        return Ok(named);
    }
    // A unique suffix (`rehearsal` for `2026-…-rehearsal`).
    let hits: Vec<PathBuf> = list(dir)
        .into_iter()
        .filter(|t| t.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.ends_with(which)))
        .collect();
    match hits.as_slice() {
        [one] => Ok(one.clone()),
        [] => anyhow::bail!("no take {which:?} in {}", dir.display()),
        _ => Ok(hits.last().unwrap().clone()),
    }
}

/// The size of everything in a take directory, in bytes.
pub fn size(dir: &Path) -> u64 {
    fs::read_dir(dir).map_or(0, |r| r.filter_map(|e| e.ok()?.metadata().ok()).map(|m| m.len()).sum())
}

/// Truncate a file (tests: a crash mid-write).
pub fn truncate(path: &Path, len: u64) -> std::io::Result<()> {
    OpenOptions::new().write(true).open(path)?.set_len(len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use simply_osc::Value;

    #[test]
    fn take_names_are_utc_stamps() {
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_792_530_843); // 2026-10-20T21:14:03Z
        assert_eq!(dir_name(t, Some("re hearsal")), "2026-10-20T21-14-03-re-hearsal");
        assert_eq!(dir_name(SystemTime::UNIX_EPOCH, None), "1970-01-01T00-00-00");
    }

    #[test]
    fn the_takes_dir_follows_xdg_data_home() {
        assert_eq!(default_dir(Some(Path::new("/x")), Some(Path::new("/h"))), Some("/x/tussen/takes".into()));
        assert_eq!(default_dir(Some(Path::new("rel")), Some(Path::new("/h"))), Some("/h/.local/share/tussen/takes".into()));
        assert_eq!(default_dir(None, None), None);
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tussen-take-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    fn meta() -> Meta {
        Meta { sample_rate: 48000, channels: 2, packet_frames: 256, quantum: 4.0, ..Meta::default() }
    }

    #[test]
    fn a_take_round_trips_and_a_torn_tail_is_ignored() {
        let dir = tmp("roundtrip");
        let rec = Recorder::start(&dir, meta(), Some("[osc]\n")).unwrap();
        let mut audio = rec.audio_tap(512);
        let t0 = rec.start_instant();
        rec.link(t0, 120.0, 0.0, true, 0, 4.0);
        for i in 0..10u32 {
            let at = t0 + Duration::from_millis(10 * i as u64);
            rec.osc(at, None, &Message::new("/tussen/midi/cc/1").arg(Value::Float32(i as f32)));
            audio.push(&vec![0.5; 512], at);
        }
        rec.sync(t0, 40, 0);
        rec.finish();
        let take = Take::open(&dir).unwrap();
        assert_eq!(take.events.len(), 10);
        assert_eq!(take.events[3].t, 30_000);
        assert_eq!(take.origins[&take.events[0].origin], "hub");
        assert_eq!(take.frames, 2560);
        assert_eq!(take.signal_delay_ms(), 40);
        assert_eq!(take.links[0].bpm, 120.0);
        let mut s = [0f32; 4];
        take.read_audio(100, &mut s);
        assert!((s[0] - 0.5).abs() < 1e-3);
        assert_eq!(fs::read_to_string(dir.join("hub.toml")).unwrap(), "[osc]\n");
        // A crash mid-write, at every byte: the take opens, keeping every
        // whole record before the cut.
        let log = dir.join("events.bin");
        let full = fs::read(&log).unwrap();
        let mut last = 0;
        for len in 10..full.len() {
            fs::write(&log, &full[..len]).unwrap();
            let n = Take::open(&dir).unwrap().events.len();
            assert!(n >= last && n <= 10, "cut at {len}: {n} events");
            last = n;
        }
        assert!(last >= 9);
        truncate(&dir.join("audio.wav"), WAV_HEADER + 1001).unwrap();
        let take = Take::open(&dir).unwrap();
        assert_eq!(take.frames, 250, "the WAV is sized from the file");
        let _ = fs::remove_dir_all(&dir);
    }
}
