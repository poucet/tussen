//! `audio_out` destinations: the hub's audio played on a local output
//! device — the room's speakers, a pair of headphones — in `run` (monitor
//! the captured mix) and `replay` (hear the take) alike.
//!
//! An audio output is **just another RTP consumer**. It is an in-process
//! receiver on a loopback UDP socket, registered with the audio sender
//! like a device ([`crate::generators::audio::AudioLink::outputs`]), so it
//! gets the same packets through the same delay ring at the delay its
//! destination kind gives it (`engine`, the default: none — the room).
//! Loops and seams in a replay are therefore exactly what every other
//! consumer gets.
//!
//! Inside: [`receive`] parses each datagram ([`rtp::parse`], the one
//! parser `listen --audio` uses too) and puts the packets back in order
//! ([`Reorder`]) into an `rtrb` ring; the device callback ([`Feed::fill`])
//! pops them, maps the channels and resamples linearly to the device rate.
//! No allocation, locks or I/O in the callback.
//!
//! **A reusable unit.** Nothing here knows the hub's sender: [`receive_on`]
//! takes any RTP L16 stream on any address, [`play`] any ring at any rate
//! and channel count, and the clock policy is [`Drift`] alone. The hub
//! wires it to its own stream in `cmd::run`; a mesh stage can point remote
//! streams at it the same way.
//!
//! **Latency and drift.** The feed waits for [`PRIME_MS`] of audio before
//! it plays (and again after running dry) — the jitter buffer — so the
//! output sits that much plus the device's own buffer behind the stream.
//! The stream runs on the hub's clock, the device on its own crystal; when
//! the ring holds more than [`HIGH_MS`] the feed skips back down to
//! [`PRIME_MS`] — a rare, tiny jump rather than an ever-growing delay.

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::Context;
use cpal::traits::{DeviceTrait, StreamTrait};
use simply_audio_device as device;

use crate::generators::audio::rtp;

/// Audio the feed waits for before it plays.
pub const PRIME_MS: u32 = 20;
/// Above this much queued audio the feed skips back to [`PRIME_MS`].
pub const HIGH_MS: u32 = 80;

/// The ring between the receiver and the device callback: room for well
/// over [`HIGH_MS`], so the receiver never has to drop.
pub fn ring(rate: u32, channels: u16) -> (rtrb::Producer<f32>, rtrb::Consumer<f32>) {
    rtrb::RingBuffer::new(rate as usize * channels.max(1) as usize / 4)
}

/// Pick an output device by `wanted`, as `simply-tussen list` prints it:
/// blank = the system default (`None`), else an exact name, else the one
/// name containing it (case-insensitive). Ambiguous or absent errs with
/// the candidates.
pub fn pick(names: &[String], wanted: &str) -> anyhow::Result<Option<String>> {
    let wanted = wanted.trim();
    if wanted.is_empty() {
        return Ok(None);
    }
    if let Some(n) = names.iter().find(|n| *n == wanted) {
        return Ok(Some(n.clone()));
    }
    let low = wanted.to_lowercase();
    let hits: Vec<&String> = names.iter().filter(|n| n.to_lowercase().contains(&low)).collect();
    match hits.as_slice() {
        [one] => Ok(Some((*one).clone())),
        [] => anyhow::bail!("no audio output matches {wanted:?}; outputs: {}", names.join(", ")),
        many => anyhow::bail!(
            "{wanted:?} matches several audio outputs: {}",
            many.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
        ),
    }
}

/// Counters the feed keeps (callback thread writes, anyone reads).
#[derive(Default, Debug)]
pub struct FeedStats {
    /// Device frames played as silence because the ring ran dry.
    pub underruns: AtomicU64,
    /// Stream frames skipped to bring the queue back down (clock drift).
    pub skipped: AtomicU64,
}

/// The clock policy, kept apart from the resampler: when to start
/// playing and how to answer the two clocks (the stream's sender, the
/// device's crystal) drifting apart. Today: start at exactly `prime`
/// samples queued (a fixed latency, whatever the packet phase), and skip
/// back to `prime` when more than `high` piles up. Adaptive resampling
/// (the mesh's D11: a rate `1 + k·e` held to a target fill) replaces
/// [`Drift::decide`] and sets [`Feed::set_ratio`]; nothing else changes.
#[derive(Clone, Copy, Debug)]
pub struct Drift {
    /// Samples (interleaved) to have before playing: the jitter buffer.
    pub prime: usize,
    /// Above this many queued samples, skip back to `prime`.
    pub high: usize,
}

impl Drift {
    /// [`PRIME_MS`] / [`HIGH_MS`] at `rate` × `channels`.
    pub fn default_for(rate: u32, channels: usize) -> Drift {
        let ms = |m: u32| (rate as usize * m as usize / 1000) * channels;
        Drift { prime: ms(PRIME_MS).max(channels), high: ms(HIGH_MS) }
    }

    /// Before one device buffer: whether to play, and how many queued
    /// samples to drop first (a multiple of `channels`).
    pub fn decide(&self, queued: usize, playing: bool, channels: usize) -> (bool, usize) {
        let starting = !playing && queued >= self.prime;
        let play = playing || starting;
        let skip = if (starting && queued > self.prime) || (play && queued > self.high) {
            (queued - self.prime) / channels * channels
        } else {
            0
        };
        (play, skip)
    }
}

/// Adapts a ring of interleaved frames (any stream: its rate, its
/// channels) to the device's buffers (device rate, device channels):
/// channel mapping and linear resampling, with [`Drift`] deciding when.
pub struct Feed {
    ring: rtrb::Consumer<f32>,
    src_ch: usize,
    dev_ch: usize,
    /// Stream frames per device frame.
    step: f64,
    /// Interpolating between frame `a` and `b`, `frac` of the way.
    a: Vec<f32>,
    b: Vec<f32>,
    frac: f64,
    /// Playing (else waiting for `drift.prime` samples).
    primed: bool,
    pub drift: Drift,
    pub stats: Arc<FeedStats>,
}

impl Feed {
    pub fn new(ring: rtrb::Consumer<f32>, src_rate: u32, src_ch: u16, dev_rate: u32, dev_ch: u16) -> Feed {
        let src_ch = src_ch.max(1) as usize;
        Feed {
            ring,
            src_ch,
            dev_ch: dev_ch.max(1) as usize,
            step: src_rate.max(1) as f64 / dev_rate.max(1) as f64,
            a: vec![0.0; src_ch],
            b: vec![0.0; src_ch],
            // ≥ 1: take a frame before the first output one.
            frac: 1.0,
            primed: false,
            drift: Drift::default_for(src_rate, src_ch),
            stats: Arc::default(),
        }
    }

    /// Retune the resampler: `ratio` stream frames per device frame
    /// (nominally stream rate ÷ device rate; an adaptive drift policy
    /// nudges it by ppm).
    pub fn set_ratio(&mut self, ratio: f64) {
        self.step = ratio;
    }

    /// Playing (primed), not waiting for audio.
    pub fn playing(&self) -> bool {
        self.primed
    }

    /// Pop the next stream frame into `b` (after `a` takes the old `b`);
    /// false when the ring has no whole frame.
    fn advance(&mut self) -> bool {
        if self.ring.slots() < self.src_ch {
            return false;
        }
        std::mem::swap(&mut self.a, &mut self.b);
        if let Ok(chunk) = self.ring.read_chunk(self.src_ch) {
            let (x, y) = chunk.as_slices();
            self.b[..x.len()].copy_from_slice(x);
            self.b[x.len()..].copy_from_slice(y);
            chunk.commit_all();
        }
        true
    }

    /// Fill one device buffer (interleaved, `dev_ch` channels).
    pub fn fill(&mut self, out: &mut [f32]) {
        let (play, skip) = self.drift.decide(self.ring.slots(), self.primed, self.src_ch);
        self.primed = play;
        if skip > 0 {
            if let Ok(chunk) = self.ring.read_chunk(skip) {
                chunk.commit_all();
                self.stats.skipped.fetch_add((skip / self.src_ch) as u64, Ordering::Relaxed);
            }
        }
        for frame in out.chunks_mut(self.dev_ch) {
            while self.primed && self.frac >= 1.0 {
                if self.advance() {
                    self.frac -= 1.0;
                } else {
                    // Dry: keep the position, play silence until re-primed.
                    self.primed = false;
                }
            }
            if !self.primed {
                frame.fill(0.0);
                self.stats.underruns.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            let t = self.frac as f32;
            for (c, s) in frame.iter_mut().enumerate() {
                let at = |f: &[f32]| map_channel(f, c, self.dev_ch);
                let (x, y) = (at(&self.a), at(&self.b));
                *s = x + (y - x) * t;
            }
            self.frac += self.step;
        }
    }
}

/// Device channel `c` of `dev_ch` from one take frame: mono feeds every
/// channel, a mono device gets the mean, else channel for channel (extra
/// device channels silent).
fn map_channel(frame: &[f32], c: usize, dev_ch: usize) -> f32 {
    match frame.len() {
        1 => frame[0],
        n if dev_ch == 1 => frame.iter().sum::<f32>() / n as f32,
        n if c < n => frame[c],
        _ => 0.0,
    }
}

/// Puts RTP packets back in sequence order. A packet that arrives early is
/// held until the gap before it fills or [`REORDER_WINDOW`] packets are
/// waiting; then the gap is given up as lost and played as silence (so
/// the timing holds). Late and duplicate packets are dropped. A new SSRC
/// or a jump of more than [`MAX_GAP`] packets starts over.
#[derive(Default)]
pub struct Reorder {
    ssrc: Option<u32>,
    next: u16,
    held: Vec<(u16, Vec<f32>)>,
    silence: Vec<f32>,
    /// Packets given up on (played as silence).
    pub lost: u64,
    /// Packets that came after their place was played, or twice.
    pub late: u64,
}

/// Packets held while waiting for a missing one.
pub const REORDER_WINDOW: usize = 4;
/// A larger jump is a restart, not a loss: no silence for it.
pub const MAX_GAP: u16 = 64;

impl Reorder {
    /// Take one packet; `emit` gets the samples to play, in order.
    pub fn push(&mut self, p: rtp::Packet, mut emit: impl FnMut(&[f32])) {
        let ahead = p.seq.wrapping_sub(self.next);
        if self.ssrc != Some(p.ssrc) || (ahead > MAX_GAP && (ahead as i16) > 0) {
            self.ssrc = Some(p.ssrc);
            self.next = p.seq;
            self.held.clear();
        } else if (p.seq.wrapping_sub(self.next) as i16) < 0 || self.held.iter().any(|h| h.0 == p.seq) {
            self.late += 1;
            return;
        }
        self.held.push((p.seq, p.samples));
        let next = self.next;
        self.held.sort_by_key(|h| h.0.wrapping_sub(next));
        loop {
            match self.held.first() {
                Some((seq, _)) if *seq == self.next => {
                    let (_, samples) = self.held.remove(0);
                    emit(&samples);
                    self.next = self.next.wrapping_add(1);
                }
                Some((seq, samples)) if self.held.len() > REORDER_WINDOW => {
                    let gap = seq.wrapping_sub(self.next);
                    self.silence.resize(samples.len(), 0.0);
                    for _ in 0..gap {
                        emit(&self.silence);
                    }
                    self.lost += gap as u64;
                    self.next = *seq;
                }
                _ => break,
            }
        }
    }
}

/// What a [`Receiver`] counts (its thread writes, anyone reads).
#[derive(Default, Debug)]
pub struct RecvStats {
    pub packets: AtomicU64,
    /// Given up on, played as silence ([`Reorder::lost`]).
    pub lost: AtomicU64,
    /// Arrived after their place was played, or twice ([`Reorder::late`]).
    pub late: AtomicU64,
    /// Samples dropped because the ring was full (the device stalled).
    pub overflow: AtomicU64,
}

/// An RTP receiver thread, feeding a [`Feed`]'s ring. Dropping it stops it.
pub struct Receiver {
    pub addr: SocketAddr,
    pub stats: Arc<RecvStats>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Bind a loopback RTP port (the hub's own stream) and push what arrives,
/// in order, into `ring`.
pub fn receive(ring: rtrb::Producer<f32>) -> std::io::Result<Receiver> {
    receive_on(SocketAddr::from(([127, 0, 0, 1], 0)), ring)
}

/// [`receive`] on any address: any sender's RTP L16 stream. The ring gets
/// its samples as sent (interleaved; the caller knows rate and channels).
pub fn receive_on(bind: SocketAddr, mut ring: rtrb::Producer<f32>) -> std::io::Result<Receiver> {
    let socket = UdpSocket::bind(bind)?;
    socket.set_read_timeout(Some(Duration::from_millis(100)))?;
    let addr = socket.local_addr()?;
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = Arc::clone(&stop);
    let stats = Arc::new(RecvStats::default());
    let counts = Arc::clone(&stats);
    let thread = std::thread::Builder::new().name("simply-tussen audio out rtp".into()).spawn(move || {
        let mut buf = vec![0u8; 65536];
        let mut order = Reorder::default();
        while !stopping.load(Ordering::Relaxed) {
            let Ok(n) = socket.recv(&mut buf) else { continue };
            let Some(p) = rtp::parse(&buf[..n]) else { continue };
            counts.packets.fetch_add(1, Ordering::Relaxed);
            order.push(p, |samples| {
                match ring.write_chunk_uninit(samples.len()) {
                    Ok(chunk) => {
                        chunk.fill_from_iter(samples.iter().copied());
                    }
                    Err(_) => {
                        counts.overflow.fetch_add(samples.len() as u64, Ordering::Relaxed);
                    }
                }
            });
            counts.lost.store(order.lost, Ordering::Relaxed);
            counts.late.store(order.late, Ordering::Relaxed);
        }
    })?;
    Ok(Receiver { addr, stats, stop, thread: Some(thread) })
}

/// The output device `wanted` names (see [`pick`]): its name as listed,
/// the default output's for blank.
pub fn device_name(wanted: &str) -> anyhow::Result<String> {
    let preferred = pick(&device::output_device_names(), wanted)?;
    Ok(device::select_output(preferred.as_deref())?.name)
}

/// A running output device stream. Dropping it stops the stream.
pub struct Device {
    pub name: String,
    pub rate: u32,
    pub channels: u16,
    pub stats: Arc<FeedStats>,
    stop: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Device {
    fn drop(&mut self) {
        self.stop.take();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Open output `wanted` (see [`pick`]) and play `ring` on it — interleaved
/// frames at `src_rate`/`src_ch`. The stream lives on its own thread (a
/// cpal stream is not `Send` on every host).
pub fn play(wanted: &str, ring: rtrb::Consumer<f32>, src_rate: u32, src_ch: u16) -> anyhow::Result<Device> {
    let preferred = pick(&device::output_device_names(), wanted)?;
    let (ready_tx, ready_rx) = mpsc::channel::<anyhow::Result<(String, u32, u16, Arc<FeedStats>)>>();
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let thread = std::thread::Builder::new().name("simply-tussen audio out".into()).spawn(move || {
        let opened = (|| {
            let sel = device::select_output(preferred.as_deref())?;
            let config = device::default_f32_output_config(&sel.device)?.config();
            let mut feed = Feed::new(ring, src_rate, src_ch, config.sample_rate, config.channels);
            let stats = Arc::clone(&feed.stats);
            let name = sel.name.clone();
            let stream = sel.device.build_output_stream::<f32, _, _>(
                config,
                move |data, _| feed.fill(data),
                move |e| log::warn!("audio out {name:?}: {e}"),
                None,
            )?;
            stream.play()?;
            anyhow::Ok((stream, sel.name, config.sample_rate, config.channels, stats))
        })();
        match opened {
            Ok((stream, name, rate, ch, stats)) => {
                let _ = ready_tx.send(Ok((name, rate, ch, stats)));
                let _ = stop_rx.recv(); // until the handle drops
                drop(stream);
            }
            Err(e) => {
                let _ = ready_tx.send(Err(e));
            }
        }
    })?;
    let (name, rate, channels, stats) = ready_rx.recv().context("the audio output thread died")??;
    Ok(Device { name, rate, channels, stats, stop: Some(stop_tx), thread: Some(thread) })
}

/// An `audio_out` destination, running: its RTP receiver and the device.
pub struct AudioOut {
    pub receiver: Receiver,
    pub device: Device,
}

/// Open an `audio_out` destination: the device `wanted`, fed by a fresh
/// loopback RTP receiver for a stream at `rate`/`channels`.
pub fn open(wanted: &str, rate: u32, channels: u16) -> anyhow::Result<AudioOut> {
    open_on(SocketAddr::from(([127, 0, 0, 1], 0)), wanted, rate, channels)
}

/// [`open`] with the receiver bound to `bind`: any RTP L16 stream.
pub fn open_on(bind: SocketAddr, wanted: &str, rate: u32, channels: u16) -> anyhow::Result<AudioOut> {
    let (tx, rx) = ring(rate, channels);
    let device = play(wanted, rx, rate, channels)?;
    let receiver = receive_on(bind, tx).context("binding the audio output's RTP port")?;
    Ok(AudioOut { receiver, device })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Push `frames` (interleaved) into a fresh ring, as the pacer does.
    fn fed(samples: &[f32], src_rate: u32, src_ch: u16, dev_rate: u32, dev_ch: u16) -> Feed {
        let (mut tx, rx) = ring(src_rate, src_ch);
        for &s in samples {
            tx.push(s).unwrap();
        }
        let mut f = Feed::new(rx, src_rate, src_ch, dev_rate, dev_ch);
        f.drift = Drift { prime: samples.len(), high: usize::MAX };
        f
    }

    fn pkt(ssrc: u32, seq: u16, v: f32) -> rtp::Packet {
        rtp::Packet { payload_type: rtp::PAYLOAD_TYPE, seq, timestamp: 0, ssrc, samples: vec![v; 4] }
    }

    fn order(packets: &[(u32, u16, f32)]) -> (Vec<f32>, Reorder) {
        let mut r = Reorder::default();
        let mut got = Vec::new();
        for &(ssrc, seq, v) in packets {
            r.push(pkt(ssrc, seq, v), |s| got.push(s[0]));
        }
        (got, r)
    }

    #[test]
    fn packets_come_out_in_order_across_the_seq_wrap() {
        let (got, r) = order(&[(1, 65534, 1.0), (1, 65535, 2.0), (1, 0, 3.0), (1, 1, 4.0)]);
        assert_eq!(got, [1.0, 2.0, 3.0, 4.0]);
        assert_eq!((r.lost, r.late), (0, 0));
    }

    #[test]
    fn a_reordered_packet_is_put_back_and_late_or_twice_is_dropped() {
        let (got, r) = order(&[(1, 10, 1.0), (1, 12, 3.0), (1, 11, 2.0), (1, 13, 4.0), (1, 11, 9.0), (1, 13, 9.0)]);
        assert_eq!(got, [1.0, 2.0, 3.0, 4.0]);
        assert_eq!((r.lost, r.late), (0, 2));
    }

    #[test]
    fn a_lost_packet_becomes_silence_once_the_window_fills() {
        let (got, r) = order(&[(1, 0, 1.0), (1, 2, 3.0), (1, 3, 4.0), (1, 4, 5.0), (1, 5, 6.0)]);
        assert_eq!(got, [1.0], "held while 1 may still come");
        assert_eq!(r.lost, 0);
        let (got, r) = order(&[(1, 0, 1.0), (1, 2, 3.0), (1, 3, 4.0), (1, 4, 5.0), (1, 5, 6.0), (1, 6, 7.0)]);
        assert_eq!(got, [1.0, 0.0, 3.0, 4.0, 5.0, 6.0, 7.0], "the gap plays as silence, timing kept");
        assert_eq!(r.lost, 1);
    }

    #[test]
    fn a_new_stream_or_a_big_jump_starts_over() {
        let (got, r) = order(&[(1, 5, 1.0), (2, 900, 2.0), (2, 901, 3.0), (2, 20_000, 4.0)]);
        assert_eq!(got, [1.0, 2.0, 3.0, 4.0]);
        assert_eq!((r.lost, r.late), (0, 0));
    }

    #[test]
    fn rtp_over_loopback_reaches_the_feed_in_order() {
        // The real receiver on a real socket, fed by the sender's packetizer.
        let (tx, rx) = ring(48_000, 2);
        let recv = receive(tx).unwrap();
        let mut f = Feed::new(rx, 48_000, 2, 48_000, 2);
        f.drift.prime = 8 * 256 * 2; // start once everything is in: no trim
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut p = rtp::Packetizer::new();
        let mut next = 0i32;
        for _ in 0..8 {
            let block: Vec<f32> = (0..256).flat_map(|_| {
                next += 1;
                let v = (next as f32 + 0.5) / 32767.0; // L16 truncates: lands on `next`
                [v, -v]
            }).collect();
            sock.send_to(&p.packet(&block, 2), recv.addr).unwrap();
        }
        let mut got = Vec::new();
        let until = std::time::Instant::now() + Duration::from_secs(2);
        while f.ring.slots() < f.drift.prime && std::time::Instant::now() < until {
            std::thread::sleep(Duration::from_millis(5));
        }
        while got.len() < 8 * 256 - 64 {
            let mut out = vec![0.0; 2 * 64];
            if f.ring.slots() >= 2 * 65 || f.playing() {
                f.fill(&mut out);
                got.extend(out.chunks(2).map(|c| (c[0] * 32767.0).round() as i32));
            }
        }
        assert!(got.len() >= 8 * 256 - 64, "{} frames", got.len());
        assert_eq!(recv.stats.packets.load(Ordering::Relaxed), 8);
        assert_eq!(recv.stats.lost.load(Ordering::Relaxed) + recv.stats.late.load(Ordering::Relaxed), 0);
        for (i, v) in got.iter().enumerate() {
            assert_eq!(*v, i as i32, "frame {i}"); // one zero lead-in frame, then 1, 2, …
        }
    }

    #[test]
    fn names_match_exactly_then_by_unique_substring() {
        let names: Vec<String> = ["MacBook Pro Speakers", "External Headphones", "BlackHole 2ch"].map(String::from).into();
        assert_eq!(pick(&names, "").unwrap(), None);
        assert_eq!(pick(&names, "  ").unwrap(), None);
        assert_eq!(pick(&names, "BlackHole 2ch").unwrap().as_deref(), Some("BlackHole 2ch"));
        assert_eq!(pick(&names, "speakers").unwrap().as_deref(), Some("MacBook Pro Speakers"));
        let e = pick(&names, "nope").unwrap_err().to_string();
        assert!(e.contains("outputs: MacBook Pro Speakers, External Headphones, BlackHole 2ch"), "{e}");
        assert!(pick(&names, "e").unwrap_err().to_string().contains("several"));
    }

    #[test]
    fn same_rate_stereo_passes_through_one_frame_late() {
        // Linear interpolation starts between a zero frame and the first
        // real one: the output is the input delayed by exactly one frame.
        let src: Vec<f32> = (1..=8).flat_map(|i| [i as f32, -(i as f32)]).collect();
        let mut f = fed(&src, 48_000, 2, 48_000, 2);
        let mut out = vec![9.0; 8];
        f.fill(&mut out);
        assert_eq!(out, [0.0, 0.0, 1.0, -1.0, 2.0, -2.0, 3.0, -3.0]);
        f.fill(&mut out);
        assert_eq!(out, [4.0, -4.0, 5.0, -5.0, 6.0, -6.0, 7.0, -7.0]);
    }

    #[test]
    fn channels_map_to_the_device() {
        // (Linear interpolation needs the frame after the one it plays.)
        let mut mono_dev = fed(&[1.0, 3.0, 7.0, 7.0], 48_000, 2, 48_000, 1);
        let mut out = [0.0; 2];
        mono_dev.fill(&mut out);
        assert_eq!(out, [0.0, 2.0]);
        let mut quad = fed(&[1.0, 3.0, 7.0, 7.0], 48_000, 2, 48_000, 4);
        let mut out = [9.0; 8];
        quad.fill(&mut out);
        assert_eq!(out, [0.0, 0.0, 0.0, 0.0, 1.0, 3.0, 0.0, 0.0]);
        let mut from_mono = fed(&[5.0, 7.0], 48_000, 1, 48_000, 2);
        let mut out = [9.0; 4];
        from_mono.fill(&mut out);
        assert_eq!(out, [0.0, 0.0, 5.0, 5.0]);
    }

    #[test]
    fn a_ramp_resamples_linearly_to_44k1_and_96k() {
        // A ramp stays a ramp under linear interpolation: output frame k
        // lies at take position k·step (minus the one-frame lead-in).
        let src: Vec<f32> = (0..4800).map(|i| i as f32).collect();
        for dev_rate in [44_100u32, 96_000] {
            let mut f = fed(&src, 48_000, 1, dev_rate, 1);
            let mut out = vec![0.0; 2000];
            f.fill(&mut out);
            let step = 48_000.0 / dev_rate as f64;
            for (k, s) in out.iter().enumerate().skip(1) {
                let want = k as f64 * step - 1.0;
                assert!((*s as f64 - want.max(0.0)).abs() < 1e-3 || want < 0.0, "{dev_rate}: frame {k} = {s}, want {want}");
            }
        }
    }

    #[test]
    fn it_waits_to_prime_and_re_primes_after_running_dry() {
        let (mut tx, rx) = ring(48_000, 1);
        let mut f = Feed::new(rx, 48_000, 1, 48_000, 1);
        let prime = f.drift.prime; // 20 ms = 960 frames
        assert_eq!(prime, 960);
        for i in 0..prime - 1 {
            tx.push(i as f32 + 1.0).unwrap();
        }
        let mut out = vec![9.0; 64];
        f.fill(&mut out);
        assert!(out.iter().all(|&s| s == 0.0), "not primed yet: silence");
        tx.push(prime as f32).unwrap();
        let mut out = vec![0.0; prime + 10];
        f.fill(&mut out);
        assert_eq!(&out[..3], [0.0, 1.0, 2.0]);
        assert_eq!(out[prime - 1], (prime - 1) as f32, "played in order");
        assert!(out[prime..].iter().all(|&s| s == 0.0), "dry: silence");
        let before = f.stats.underruns.load(Ordering::Relaxed);
        let mut out = vec![9.0; 16];
        f.fill(&mut out);
        assert!(out.iter().all(|&s| s == 0.0));
        assert_eq!(f.stats.underruns.load(Ordering::Relaxed), before + 16);
        // Re-primed, it resumes exactly where it ran dry.
        for i in 0..prime {
            tx.push((prime + 1 + i) as f32).unwrap();
        }
        let mut out = vec![9.0; 4];
        f.fill(&mut out);
        assert_eq!(out, [prime as f32, prime as f32 + 1.0, prime as f32 + 2.0, prime as f32 + 3.0]);
    }

    #[test]
    fn a_queue_grown_by_drift_is_skipped_back_down() {
        let (mut tx, rx) = ring(48_000, 2);
        let mut f = Feed::new(rx, 48_000, 2, 48_000, 2);
        for _ in 0..48_000 / 10 * 2 {
            tx.push(0.5).unwrap(); // 100 ms queued > HIGH_MS
        }
        let mut out = vec![0.0; 2];
        f.fill(&mut out);
        assert_eq!(f.stats.skipped.load(Ordering::Relaxed), 4800 - 960, "skipped down to 20 ms");
        assert_eq!(f.ring.slots(), 960 * 2 - 2, "then played one frame");
    }

    #[test]
    fn a_seam_in_the_ring_plays_through_without_a_gap() {
        // The pacer writes pass n's last frames then pass n+1's first ones
        // back to back; the feed, fed in pacer-sized blocks between device
        // buffers of another size, emits every sample once, in order.
        let (mut tx, rx) = ring(48_000, 2);
        let mut f = Feed::new(rx, 48_000, 2, 48_000, 2);
        f.drift.prime = 512; // the first block, exactly
        let mut next = 1.0f32;
        let mut got = Vec::new();
        for _ in 0..40 {
            for _ in 0..256 {
                tx.push(next).unwrap();
                tx.push(-next).unwrap();
                next += 1.0;
            }
            let mut out = vec![0.0; 2 * 200];
            f.fill(&mut out);
            got.extend(out.chunks(2).map(|c| c[0]));
        }
        let played: Vec<f32> = got.into_iter().skip(1).collect();
        for (i, s) in played.iter().enumerate() {
            assert_eq!(*s, i as f32 + 1.0, "sample {i}");
        }
    }
}
