//! The audio generator: capture the engine's
//! audio and send it as RTP/L16 to every device whose hello asked for it.
//! The hub computes nothing from the audio — analysis is each device's
//! own, from the stream.
//!
//! Threads and hand-offs:
//!
//! * the **capture sink** ([`simply_audio_capture`]'s, on the device's
//!   real-time thread, already resampled to `sample_rate`) copies frames
//!   into an `rtrb` SPSC ring and does nothing else — no allocation, no
//!   locks, no I/O, no logging; a full ring means dropped frames and a
//!   bumped overflow counter;
//! * the **sender thread** pops `packet_frames`-sized blocks, packetizes
//!   them ([`rtp`]) into a [`DelayRing`] and sends to the targets in
//!   [`AudioLink`] (maintained by the hub loop from device hellos and the
//!   console's sync state), each as far behind live as its delay says:
//!   sync point 2 holds a device's audio back by TouchDesigner's round
//!   trip. The ring is preallocated; sending allocates nothing.
//!
//! `[audio] device` names any audio device. One with inputs is read as an
//! input; an output-only one (speakers, headphones, a Multi-Output
//! Device) — and `""`, the default output — is tapped (a CoreAudio process
//! tap, macOS 14.2+). The rule and the capture both live in the shared
//! [`simply_audio_capture`] crate (audiowatch uses the same one); this
//! module only adapts its sink to the ring.

pub mod rtp;

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Context;
use crossbeam_channel::Sender;
use simply_audio_capture as capture;
use simply_osc::Message;

use crate::config::AudioConfig;
use crate::generators::Generator;

/// One device's RTP receiver and how far behind live its stream runs
/// (the round trip for a device that follows TouchDesigner,
/// else 0).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RtpTarget {
    pub addr: SocketAddr,
    pub delay_ms: u32,
}

/// What the hub loop and the audio sender share: where to send RTP
/// (devices that gave a nonzero rtp_port in their hello — the hub loop
/// rewrites this after every hello/bye/prune and sync change) and the
/// counters the stats line prints.
#[derive(Default)]
pub struct AudioLink {
    pub targets: Mutex<Vec<RtpTarget>>,
    /// Static destinations with an `rtp_port` (set once at hub start):
    /// the same audio, undelayed, each on its own RTP stream (SSRC).
    pub static_targets: Mutex<Vec<SocketAddr>>,
    /// Total RTP packets sent (sender thread increments).
    pub packets: AtomicU64,
    /// Frames dropped because the capture ring was full.
    pub overflows: AtomicU64,
    /// Tells the sender thread to exit (tests; the CLI runs until killed).
    pub stop: AtomicBool,
    /// The hub console's timeline tap: min/max/rms per packet,
    /// computed here on the sender thread — never in the capture callback.
    pub tap: OnceLock<Arc<crate::console::Tap>>,
}

pub struct AudioGenerator {
    config: AudioConfig,
    link: Arc<AudioLink>,
}

impl AudioGenerator {
    pub fn new(config: AudioConfig, link: Arc<AudioLink>) -> AudioGenerator {
        AudioGenerator { config, link }
    }
}

impl Generator for AudioGenerator {
    fn name(&self) -> &'static str {
        "audio"
    }

    fn start(&mut self, _out: Sender<Message>) -> anyhow::Result<()> {
        let cfg = self.config.clone();
        let link = Arc::clone(&self.link);
        let (ring_tx, ring) = rtrb::RingBuffer::<f32>::new(cfg.sample_rate as usize * cfg.channels as usize);
        let tap = start_capture(&cfg, ring_tx, &link)?;
        std::thread::Builder::new()
            .name("simply-tussen audio".into())
            .spawn(move || sender_loop(&cfg, &link, Source { ring, tap }))?;
        Ok(())
    }
}

/// Stream whatever is pushed into `ring` (interleaved, at `sample_rate`)
/// as RTP, exactly as a capture does: the sender thread without a
/// device. For tests.
pub fn stream_ring(cfg: AudioConfig, link: Arc<AudioLink>, ring: rtrb::Consumer<f32>) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("simply-tussen audio".into())
        .spawn(move || sender_loop(&cfg, &link, Source { ring, tap: None }))?;
    Ok(())
}

/// One audio device as `simply-tussen list` sees it (merged by name).
pub use capture::Device as AudioDevice;

/// Every audio device, inputs and outputs alike, merged by name.
pub fn list_devices() -> Vec<AudioDevice> {
    capture::list_devices()
}

/// How `[audio] device` is captured: `""` (or blank) taps the default
/// output; a device with inputs is read; an output-only one is tapped.
pub fn resolve(devices: &[AudioDevice], cfg: &AudioConfig) -> anyhow::Result<capture::Source> {
    Ok(capture::resolve(devices, cfg.device().unwrap_or(""))?)
}

/// Start capturing `[audio] device` into `ring` and keep it running until
/// the process exits ([`hold`]). Returns the tapped output's name when the
/// tap permission isn't known to be granted (so silence is worth a hint).
/// Fails — with the available devices in the error — without
/// taking the hub down.
fn start_capture(cfg: &AudioConfig, ring: rtrb::Producer<f32>, link: &Arc<AudioLink>) -> anyhow::Result<Option<String>> {
    let source = resolve(&list_devices(), cfg)?;
    let permission = capture::permission();
    let sink = ring_sink(ring, cfg.channels.max(1) as usize, Arc::clone(link));
    let what = match &source {
        capture::Source::Input(_) => "audio capture unavailable",
        capture::Source::Tap(_) => "audio output tap unavailable",
    };
    let running = capture::start(&source, cfg.sample_rate, cfg.channels, sink).context(what)?;
    let rate = running.device_rate();
    let resampled = if rate != cfg.sample_rate { ", resampled" } else { "" };
    let watch = match running.source() {
        capture::Source::Input(name) => {
            log::info!("audio: capturing input {name:?} at {rate} Hz{resampled}, {} Hz {}ch, {} frames/packet", cfg.sample_rate, cfg.channels, cfg.packet_frames);
            None
        }
        capture::Source::Tap(name) => {
            log::info!(
                "audio: tapping output {name:?} (everything played on it, not muted) at {rate} Hz{resampled}, {} Hz {}ch, {} frames/packet",
                cfg.sample_rate,
                cfg.channels,
                cfg.packet_frames,
            );
            (permission != capture::Permission::Granted).then(|| name.clone())
        }
    };
    hold(running)?;
    Ok(watch)
}

/// The capture sink: runs on the device's real-time thread, so it copies
/// whole frames into the ring and counts what doesn't fit — no allocation,
/// locks, logging or I/O. A partial frame is never written, so a full ring
/// can't knock the channels out of step.
fn ring_sink(mut ring: rtrb::Producer<f32>, channels: usize, link: Arc<AudioLink>) -> impl FnMut(&[f32], u64) + Send + 'static {
    move |frames: &[f32], _capture_ns: u64| {
        let n = frames.len().min(ring.slots() / channels * channels);
        if let Ok(chunk) = ring.write_chunk_uninit(n) {
            chunk.fill_from_iter(frames[..n].iter().copied());
        }
        let dropped = (frames.len() - n) as u64;
        if dropped > 0 {
            link.overflows.fetch_add(dropped, Ordering::Relaxed);
        }
    }
}

/// The one live capture, held here for the life of the process so a
/// Ctrl-C/SIGTERM can drop it — which destroys a tap's aggregate device
/// and tap — before the process exits.
static ACTIVE: Mutex<Option<capture::Capture>> = Mutex::new(None);

/// Keep `running` alive until the process exits; when it is a tap, drop it
/// on SIGINT/SIGTERM, then exit 128+signal.
fn hold(running: capture::Capture) -> anyhow::Result<()> {
    let tap = matches!(running.source(), capture::Source::Tap(_));
    *ACTIVE.lock().unwrap_or_else(|e| e.into_inner()) = Some(running);
    #[cfg(target_os = "macos")]
    if tap {
        use signal_hook::consts::{SIGINT, SIGTERM};
        let mut signals = signal_hook::iterator::Signals::new([SIGINT, SIGTERM])?;
        std::thread::Builder::new().name("simply-tussen tap cleanup".into()).spawn(move || {
            if let Some(sig) = signals.forever().next() {
                if let Some(running) = ACTIVE.lock().unwrap_or_else(|e| e.into_inner()).take() {
                    let name = running.source().device().to_owned();
                    drop(running);
                    log::info!("audio: tap of {name:?} and its aggregate device removed");
                }
                std::process::exit(128 + sig);
            }
        })?;
    }
    #[cfg(not(target_os = "macos"))]
    let _ = tap;
    Ok(())
}

/// What the sender thread reads: the capture ring.
struct Source {
    ring: rtrb::Consumer<f32>,
    /// The tapped output's name, when its permission isn't known to be
    /// granted: watched for the digital silence that produces.
    tap: Option<String>,
}

/// After this long of exact zeros from a tap, say why it might be silent.
const TAP_SILENCE_HINT_AFTER: Duration = Duration::from_secs(10);

/// Pop packet-sized blocks and send them, forever. One
/// block = `packet_frames` frames = one RTP packet to every target.
fn sender_loop(cfg: &AudioConfig, link: &AudioLink, source: Source) {
    let Source { ring: mut capture, tap } = source;
    let channels = cfg.channels.max(1) as usize;
    let block_len = cfg.packet_frames * channels;
    let mut block = vec![0.0f32; block_len];
    let mut filled = 0usize;
    let mut packetizer = rtp::Packetizer::new();
    let mut statics = StaticStreams::default();
    let packet_dur = Duration::from_secs_f64(cfg.packet_frames as f64 / cfg.sample_rate.max(1) as f64);
    let mut ring = DelayRing::new(rtp::HEADER_LEN + block_len * 2, packet_dur);
    let mut readers = Readers::default();
    let socket = match UdpSocket::bind("0.0.0.0:0") {
        Ok(s) => s,
        Err(e) => {
            log::warn!("audio: cannot bind the RTP send socket: {e}");
            return;
        }
    };
    let started = Instant::now();
    let mut level_batch = LevelBatch::default();
    // Output tap only: has it ever delivered a nonzero sample, and was
    // the silence hint logged.
    let (mut heard, mut hinted) = (false, false);

    while !link.stop.load(Ordering::Relaxed) {
        while filled < block_len {
            match capture.pop() {
                Ok(s) => {
                    block[filled] = s;
                    filled += 1;
                }
                Err(_) => break,
            }
        }
        let ready = filled == block_len;
        if ready {
            filled = 0;
        }
        if let (true, false, Some(name)) = (ready, heard, tap.as_deref()) {
            heard = block.iter().any(|&s| s != 0.0);
            if !heard && !hinted && started.elapsed() >= TAP_SILENCE_HINT_AFTER {
                hinted = true;
                log::warn!(
                    "audio: the tap of {name:?} has delivered only digital silence for {} s. \
                     If something is playing there, System Audio Recording permission is missing: {}",
                    TAP_SILENCE_HINT_AFTER.as_secs(),
                    capture::GRANT_HINT,
                );
            }
            if heard && hinted {
                log::info!("audio: the tap of {name:?} is delivering sound");
            }
        }

        if ready {
            packetizer.packet_into(&block, channels, ring.next_slot(Instant::now()));
            statics.send(link, &socket, &block, channels, packetizer.ssrc);
            link.packets.fetch_add(1, Ordering::Relaxed);
            if let Some(tap) = link.tap.get().filter(|t| t.active()) {
                level_batch.push(&block, tap);
            }
        }

        // Every device stream, each at its own delay — including packets
        // that fall due while no new block is ready.
        if let Ok(targets) = link.targets.lock() {
            readers.follow(&targets, ring.head);
        }
        let now = Instant::now();
        for r in &mut readers.0 {
            // Never block, never crash: an unreachable device is its
            // problem; errors would only repeat 187.5 times a second.
            let addr = r.addr;
            r.drain(&ring, now, |packet| {
                let _ = socket.send_to(packet, addr);
            });
        }

        if !ready {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

/// A delayed packet more than this overdue is skipped rather than sent:
/// lowering a delay mid-show jumps the stream forward instead of
/// bursting the backlog at the receiver. Wide enough that a sender
/// thread hiccup never costs packets.
const LATE_SKIP: Duration = Duration::from_millis(50);

/// The last [`crate::sync::MAX_DELAY_MS`] of RTP packets, each stamped
/// with when it was made — the audio half of the sync-point-2 delay. A
/// fixed ring of preallocated buffers: writing a packet reuses a slot,
/// so the sender thread never allocates for it.
pub struct DelayRing {
    slots: Vec<(Instant, Vec<u8>)>,
    /// Packets written so far; packet `i` lives in slot `i % len`.
    pub head: u64,
}

impl DelayRing {
    /// Room for the longest delay plus a margin, each slot preallocated
    /// to `packet_len` bytes.
    pub fn new(packet_len: usize, packet_dur: Duration) -> DelayRing {
        let max = Duration::from_millis(crate::sync::MAX_DELAY_MS as u64) + LATE_SKIP;
        let n = (max.as_secs_f64() / packet_dur.as_secs_f64().max(1e-4)).ceil() as usize + 32;
        let now = Instant::now();
        DelayRing { slots: (0..n).map(|_| (now, Vec::with_capacity(packet_len))).collect(), head: 0 }
    }

    /// The buffer for the next packet, stamped `at`; overwrites the oldest.
    pub fn next_slot(&mut self, at: Instant) -> &mut Vec<u8> {
        let n = self.slots.len() as u64;
        let slot = &mut self.slots[(self.head % n) as usize];
        self.head += 1;
        slot.0 = at;
        &mut slot.1
    }

    /// The oldest packet still held.
    fn oldest(&self) -> u64 {
        self.head.saturating_sub(self.slots.len() as u64)
    }

    fn get(&self, i: u64) -> &(Instant, Vec<u8>) {
        &self.slots[(i % self.slots.len() as u64) as usize]
    }
}

/// One device stream reading the ring `delay` behind live.
pub struct Reader {
    pub addr: SocketAddr,
    delay: Duration,
    delay_ms: u32,
    /// The next packet this receiver gets.
    next: u64,
}

impl Reader {
    /// A new receiver starts at the live edge and hears its first packet
    /// `delay_ms` later.
    pub fn new(target: RtpTarget, head: u64) -> Reader {
        let mut r = Reader { addr: target.addr, delay: Duration::ZERO, delay_ms: 0, next: head };
        r.set_delay(target.delay_ms);
        r
    }

    fn set_delay(&mut self, ms: u32) {
        let ms = ms.min(crate::sync::MAX_DELAY_MS);
        self.delay_ms = ms;
        self.delay = Duration::from_millis(ms as u64);
    }

    /// Send (in order) every packet due by `now`; skip ones overdue by
    /// more than [`LATE_SKIP`].
    pub fn drain(&mut self, ring: &DelayRing, now: Instant, mut send: impl FnMut(&[u8])) {
        self.next = self.next.max(ring.oldest());
        while self.next < ring.head {
            let (made, packet) = ring.get(self.next);
            let due = *made + self.delay;
            if due > now {
                break;
            }
            if now.duration_since(due) <= LATE_SKIP {
                send(packet);
            }
            self.next += 1;
        }
    }
}

/// The device streams, kept in step with [`AudioLink::targets`]; a
/// receiver keeps its place in the ring when only its delay changes.
#[derive(Default)]
pub struct Readers(pub Vec<Reader>);

impl Readers {
    /// Allocates only when the target list actually changed.
    pub fn follow(&mut self, targets: &[RtpTarget], head: u64) {
        let same = targets.len() == self.0.len()
            && targets.iter().zip(&self.0).all(|(t, r)| t.addr == r.addr && t.delay_ms == r.delay_ms);
        if same {
            return;
        }
        let mut old = std::mem::take(&mut self.0);
        self.0 = targets
            .iter()
            .map(|t| match old.iter().position(|r| r.addr == t.addr) {
                Some(i) => {
                    let mut r = old.swap_remove(i);
                    r.set_delay(t.delay_ms);
                    r
                }
                None => Reader::new(*t, head),
            })
            .collect();
    }
}

/// One RTP stream per static destination with an `rtp_port` (e.g. the
/// TouchDesigner laptop): own SSRC, seq and timestamp, so its receiver
/// never depends on the devices' stream. Follows
/// [`AudioLink::static_targets`], which the hub sets after the sender
/// may already be running.
#[derive(Default)]
struct StaticStreams {
    streams: Vec<(SocketAddr, rtp::Packetizer)>,
    /// Reused for every packet.
    buf: Vec<u8>,
}

impl StaticStreams {
    fn send(&mut self, link: &AudioLink, socket: &UdpSocket, block: &[f32], channels: usize, device_ssrc: u32) {
        let Ok(targets) = link.static_targets.lock() else { return };
        if targets.len() != self.streams.len() || targets.iter().zip(&self.streams).any(|(t, (a, _))| t != a) {
            let mut used = vec![device_ssrc];
            self.streams = targets
                .iter()
                .map(|&t| {
                    let mut p = rtp::Packetizer::new();
                    while used.contains(&p.ssrc) {
                        p.ssrc = p.ssrc.wrapping_add(0x9e37_79b9);
                    }
                    used.push(p.ssrc);
                    (t, p)
                })
                .collect();
        }
        drop(targets);
        for (target, p) in &mut self.streams {
            p.packet_into(block, channels, &mut self.buf);
            let _ = socket.send_to(&self.buf, *target);
        }
    }
}

/// Packets' min/max/rms, batched to ~30 Hz for the console timeline.
#[derive(Default)]
struct LevelBatch {
    ts: u64,
    started: Option<Instant>,
    blocks: Vec<[f32; 3]>,
}

impl LevelBatch {
    fn push(&mut self, block: &[f32], tap: &crate::console::Tap) {
        let now = Instant::now();
        if self.started.is_none() {
            self.started = Some(now);
            self.ts = crate::console::us_at(now);
        }
        self.blocks.push(levels(block));
        if self.started.is_some_and(|s| now.duration_since(s) >= crate::console::AUDIO_BATCH) {
            self.started = None;
            let blocks = std::mem::take(&mut self.blocks);
            tap.send(crate::console::Event::Audio { ts: self.ts, blocks });
        }
    }
}

/// `[min, max, rms]` over every sample of a packet (all channels).
pub fn levels(block: &[f32]) -> [f32; 3] {
    if block.is_empty() {
        return [0.0; 3];
    }
    let (mut lo, mut hi, mut sq) = (f32::MAX, f32::MIN, 0.0f64);
    for &s in block {
        lo = lo.min(s);
        hi = hi.max(s);
        sq += (s as f64) * (s as f64);
    }
    [lo, hi, (sq / block.len() as f64).sqrt() as f32]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(name: &str, inputs: bool, outputs: bool) -> AudioDevice {
        AudioDevice { name: name.into(), inputs, outputs, is_default_output: false }
    }

    fn target(port: u16, delay_ms: u32) -> RtpTarget {
        RtpTarget { addr: SocketAddr::from(([127, 0, 0, 1], port)), delay_ms }
    }

    /// Write packets 0..n into the ring, `dt` apart from `t0`; each
    /// packet's one byte is its number.
    fn fill(ring: &mut DelayRing, t0: Instant, dt: Duration, from: u64, to: u64) {
        for i in from..to {
            let slot = ring.next_slot(t0 + dt * i as u32);
            slot.clear();
            slot.push(i as u8);
        }
    }

    fn drained(r: &mut Reader, ring: &DelayRing, now: Instant) -> Vec<u8> {
        let mut got = Vec::new();
        r.drain(ring, now, |p| got.push(p[0]));
        got
    }

    #[test]
    fn the_delay_ring_holds_a_stream_back_by_its_delay() {
        let dt = Duration::from_millis(5);
        let mut ring = DelayRing::new(16, dt);
        let t0 = Instant::now();
        let mut live = Reader::new(target(1, 0), 0);
        let mut late = Reader::new(target(2, 20), 0);
        fill(&mut ring, t0, dt, 0, 10); // packets made at 0..45 ms
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        assert_eq!(drained(&mut live, &ring, at(45)), (0..10).collect::<Vec<u8>>());
        // 20 ms behind: by 45 ms only the packets made by 25 ms are due.
        assert_eq!(drained(&mut late, &ring, at(45)), [0, 1, 2, 3, 4, 5]);
        assert_eq!(drained(&mut late, &ring, at(54)), [6]);
        assert_eq!(drained(&mut late, &ring, at(65)), [7, 8, 9]);
        assert!(drained(&mut late, &ring, at(100)).is_empty());
    }

    #[test]
    fn lowering_a_delay_skips_ahead_instead_of_bursting() {
        let dt = Duration::from_millis(5);
        let mut ring = DelayRing::new(16, dt);
        let t0 = Instant::now();
        let mut readers = Readers::default();
        readers.follow(&[target(1, 200)], ring.head);
        fill(&mut ring, t0, dt, 0, 60); // 0..295 ms
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        assert_eq!(drained(&mut readers.0[0], &ring, at(210)), [0, 1, 2]);
        // 200 → 20 ms: packets due more than LATE_SKIP ago are dropped,
        // the stream resumes 20 ms behind live, in order.
        readers.follow(&[target(1, 20)], ring.head);
        let got = drained(&mut readers.0[0], &ring, at(295));
        assert_eq!(got.last(), Some(&55)); // made at 275 ms, due at 295
        assert_eq!(got.first(), Some(&(55 - 10))); // due 50 ms ago, the oldest kept
        assert!(got.windows(2).all(|w| w[1] == w[0] + 1));
    }

    #[test]
    fn the_ring_covers_the_longest_delay_and_reuses_its_slots() {
        let dt = Duration::from_secs_f64(256.0 / 48000.0);
        let mut ring = DelayRing::new(rtp::HEADER_LEN + 512 * 2, dt);
        let held = ring.slots.len() as f64 * dt.as_secs_f64();
        assert!(held >= crate::sync::MAX_DELAY_MS as f64 / 1000.0, "ring holds only {held} s");
        let caps: Vec<usize> = ring.slots.iter().map(|s| s.1.capacity()).collect();
        let mut p = rtp::Packetizer::new();
        let block = vec![0.25f32; 512];
        let t0 = Instant::now();
        for i in 0..ring.slots.len() as u32 * 2 {
            p.packet_into(&block, 2, ring.next_slot(t0 + dt * i));
        }
        let after: Vec<usize> = ring.slots.iter().map(|s| s.1.capacity()).collect();
        assert_eq!(caps, after, "a slot reallocated");
        // A reader 5 s behind still finds its packet in the ring.
        let mut r = Reader::new(target(1, crate::sync::MAX_DELAY_MS), 0);
        let now = t0 + dt * (ring.head as u32 - 1);
        let mut n = 0;
        r.drain(&ring, now, |_| n += 1);
        assert!(n > 0);
    }

    #[test]
    fn readers_follow_the_target_list_and_keep_their_place() {
        let mut readers = Readers::default();
        readers.follow(&[target(1, 0), target(2, 100)], 7);
        readers.0[1].next = 3;
        readers.follow(&[target(2, 40), target(3, 0)], 9);
        assert_eq!(readers.0.len(), 2);
        assert_eq!((readers.0[0].addr.port(), readers.0[0].delay_ms, readers.0[0].next), (2, 40, 3));
        assert_eq!((readers.0[1].addr.port(), readers.0[1].next), (3, 9));
    }

    #[test]
    fn a_device_with_inputs_is_captured_and_an_output_only_one_is_tapped() {
        let mut devices = [
            dev("BlackHole 2ch", true, true),
            dev("Scarlett 4i4 USB", true, true),
            dev("MacBook Pro Microphone", true, false),
            dev("MacBook Pro Speakers", false, true),
            dev("Multi-Output Device", false, true),
        ];
        devices[3].is_default_output = true;
        let path = |n: &str| resolve(&devices, &AudioConfig { device: n.into(), ..AudioConfig::default() }).unwrap();
        use capture::Source::{Input, Tap};
        assert_eq!(path(""), Tap("MacBook Pro Speakers".into()));
        assert_eq!(path("  "), Tap("MacBook Pro Speakers".into()));
        assert_eq!(path("BlackHole 2ch"), Input("BlackHole 2ch".into()));
        assert_eq!(path("Scarlett 4i4 USB"), Input("Scarlett 4i4 USB".into()));
        assert_eq!(path("MacBook Pro Microphone"), Input("MacBook Pro Microphone".into()));
        assert_eq!(path("MacBook Pro Speakers"), Tap("MacBook Pro Speakers".into()));
        assert_eq!(path("Multi-Output Device"), Tap("Multi-Output Device".into()));
        let nope = AudioConfig { device: "Nope".into(), ..AudioConfig::default() };
        let err = resolve(&devices, &nope).unwrap_err().to_string();
        assert!(err.contains("not found") && err.contains("MacBook Pro Speakers"), "{err}");
    }

    #[test]
    fn the_sink_writes_whole_frames_and_counts_what_does_not_fit() {
        let link = Arc::new(AudioLink::default());
        let (tx, mut rx) = rtrb::RingBuffer::<f32>::new(5);
        let mut sink = ring_sink(tx, 2, Arc::clone(&link));
        sink(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0], 0);
        // Room for 5 samples = 2 whole stereo frames; the third is dropped.
        assert_eq!(rx.slots(), 4);
        assert_eq!(link.overflows.load(Ordering::Relaxed), 2);
        let got: Vec<f32> = std::iter::from_fn(|| rx.pop().ok()).collect();
        assert_eq!(got, [1.0, 2.0, 3.0, 4.0]);
        sink(&[7.0, 8.0], 0);
        assert_eq!((rx.pop(), rx.pop()), (Ok(7.0), Ok(8.0)));
    }
}
