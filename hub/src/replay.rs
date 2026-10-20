//! `simply-tussen replay <take>`: a take played back through the router as if
//! live (design: `docs/design/replay.md`).
//!
//! A replay is a generator. Its **event thread** sleeps to each recorded
//! signal's time and hands it to the router, which routes it like any
//! other (remaps, sync points, subscriptions; the console marks it
//! `src: "replay"`). Its **pacer** pushes the recorded audio into the
//! existing RTP sender ([`stream_ring`]): same packetizer, one SSRC,
//! continuous sequence across loop seams.
//!
//! **One timeline.** The take's Link log gives the recorded beat at every
//! take time τ. A [`Plan`] lays passes over host time: a pass plays τ from
//! `t0` to `t1`; the next pass starts at the next host time whose beat is
//! in the same bar phase as `t0`'s recorded beat — at once for a loop of
//! whole bars, after less than a bar of silence for the whole take. The
//! published beat (`/tussen/clock/…`, the Link session) is the recorded
//! beat plus a whole number of bars per pass, so it keeps counting up
//! across seams and `beat mod quantum` is always the recorded one.
//!
//! **Link** (unless `--replay-link off`): the replay leads the local Link
//! session. It listens briefly; with nobody there it founds a session at
//! the take's tempo and beat, else it places the take on that session's
//! grid (bar phase) and proposes the take's tempo. Every recorded tempo
//! change and start/stop is proposed at its own time; at every pass start
//! the next pass is placed on the session's grid, so a session someone
//! else moved is caught up with at the next seam.
//!
//! **State at a seam or a start offset**: the last value of every address
//! before the pass start is sent where it differs from what consumers last
//! got; MIDI notes and pad buttons not held at that point are sent 0;
//! events (`…/press`, `onbeat`, …) are never re-fired.
//!
//! **Audio timing (D6)**: ingress-faithful. Audio plays at its recorded
//! packet stamp relative to the signals' recorded arrival — exactly as it
//! entered the hub live — and the rig's sync points then align it as they
//! did live. `--offset-ms` is a manual nudge (default 0).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use simply_link::{HostClock, Link, Micros, Network, SessionState};
use simply_osc::{Message, Value};

use crate::config::AudioConfig;
use crate::console::{Event, Tap};
use crate::generators::audio::{stream_ring, AudioLink};
use crate::link::{self, Beats, LinkConfig, LinkStatus, Mode, Sample};
use crate::take::{LinkPoint, Take};

/// How a replay treats Ableton Link.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum LinkMode {
    /// Lead the local session with the take's timeline (found one if none).
    Lead,
    /// Leave Link alone: the hub is no Link peer while replaying.
    Off,
}

/// What `simply-tussen replay` was asked for.
#[derive(Clone, Debug)]
pub struct Options {
    /// Loop bars `a..b`: from bar a's first beat to bar b's (1-based).
    pub loop_bars: Option<(u32, u32)>,
    /// Start at this bar (default: the loop's start, else the take's).
    pub from_bar: Option<u32>,
    /// A manual nudge: play the audio this many ms ahead of the signals
    /// (negative: behind). Default 0, the recorded timing.
    pub audio_lead_ms: Option<i32>,
    pub link: LinkMode,
    /// Where Link runs (tests: a private network).
    pub network: Network,
    /// How long to listen for a Link session before founding one.
    pub link_wait: Duration,
    /// Beats per bar when the take has no Link record.
    pub quantum: f64,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            loop_bars: None,
            from_bar: None,
            audio_lead_ms: None,
            link: LinkMode::Lead,
            network: Network::default(),
            link_wait: Duration::from_millis(1200),
            quantum: LinkConfig::default().quantum,
        }
    }
}

/// The recorded Link timeline: the beat, tempo and transport at every
/// take time, and where the bars are.
#[derive(Clone, Debug)]
pub struct Grid {
    points: Vec<LinkPoint>,
    pub quantum: f64,
    /// `(τ, beat)` of every bar line in the take, in order.
    pub bars: Vec<(u64, f64)>,
}

impl Grid {
    /// From a take's Link records; a take without any gets 120 BPM from beat 0.
    pub fn new(points: &[LinkPoint], quantum: f64, end: u64) -> Grid {
        let mut points: Vec<LinkPoint> = points.iter().filter(|p| p.bpm > 0.0).copied().collect();
        if points.is_empty() {
            points.push(LinkPoint { t: 0, bpm: 120.0, beat: 0.0, playing: true, mode: 0, quantum });
        }
        let quantum = points.last().map_or(quantum, |p| p.quantum).max(1.0);
        let mut g = Grid { points, quantum, bars: Vec::new() };
        for i in 0..g.points.len() {
            let p = g.points[i];
            let from = if i == 0 { 0 } else { p.t };
            let to = g.points.get(i + 1).map_or(end, |n| n.t);
            if to <= from {
                continue;
            }
            let (b0, b1) = (g.beat_in(&p, from), g.beat_in(&p, to));
            let mut k = (b0 / quantum - 1e-9).ceil();
            while k * quantum < b1 - 1e-9 {
                let beat = k * quantum;
                let t = (p.t as f64 + (beat - p.beat) * 60e6 / p.bpm).round().max(0.0) as u64;
                g.bars.push((t.clamp(from, to), beat));
                k += 1.0;
            }
        }
        g
    }

    fn point(&self, t: u64) -> &LinkPoint {
        let i = self.points.partition_point(|p| p.t <= t);
        &self.points[i.saturating_sub(1)]
    }

    fn beat_in(&self, p: &LinkPoint, t: u64) -> f64 {
        p.beat + (t as f64 - p.t as f64) * p.bpm / 60e6
    }

    /// The recorded beat at take time `t`.
    pub fn beat_at(&self, t: u64) -> f64 {
        self.beat_in(self.point(t), t)
    }

    pub fn bpm_at(&self, t: u64) -> f64 {
        self.point(t).bpm
    }

    pub fn playing_at(&self, t: u64) -> bool {
        self.point(t).playing
    }

    /// When the tempo in force at `t` took over (0 for the first).
    pub fn segment_start(&self, t: u64) -> u64 {
        let p = self.point(t);
        if std::ptr::eq(p, &self.points[0]) {
            0
        } else {
            p.t
        }
    }

    /// Take time of bar `n` (1-based).
    pub fn bar(&self, n: u32) -> Option<u64> {
        self.bars.get((n as usize).checked_sub(1)?).map(|b| b.0)
    }

    /// The bar `t` is in, with the fraction (1.0 = bar 1's first beat;
    /// before bar 1: below 1).
    pub fn bar_at(&self, t: u64) -> f64 {
        let i = self.bars.partition_point(|b| b.0 <= t);
        if i == 0 {
            return 1.0 - (self.bars.first().map_or(0.0, |b| b.1) - self.beat_at(t)) / self.quantum;
        }
        let (_, beat) = self.bars[i - 1];
        i as f64 + ((self.beat_at(t) - beat) / self.quantum).clamp(0.0, 0.999)
    }
}

/// One pass over take time `t0..t1`, starting at host time `h`, the
/// published beat being the recorded one plus `k`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pass {
    pub h: Micros,
    pub t0: u64,
    pub t1: u64,
    pub k: f64,
}

impl Pass {
    pub fn end(&self) -> Micros {
        self.h + (self.t1 - self.t0) as Micros
    }
}

/// Where host time falls in the plan.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Pos {
    /// Pass number `n` at take time `t`.
    In { n: u64, pass: Pass, t: u64 },
    /// Between pass `n` and the next, which starts at `next`.
    Gap { n: u64, pass: Pass, next: Micros },
}

/// The passes over host time (module doc). Shared by the event thread and
/// the pacer; the next pass is placed when the current one ends.
pub struct Plan {
    grid: Arc<Grid>,
    /// The loop: every pass after the first plays `a..b`.
    a: u64,
    b: u64,
    /// The current pass, numbered, and the one before it (the pacer may
    /// still be finishing it).
    cur: (u64, Pass),
    prev: Option<(u64, Pass)>,
    next: Option<Pass>,
    link: Option<Arc<Link>>,
}

impl Plan {
    pub fn new(grid: Arc<Grid>, first: Pass, a: u64, b: u64, link: Option<Arc<Link>>) -> Plan {
        Plan { grid, a, b, cur: (0, first), prev: None, next: None, link }
    }

    /// The published beat at the end of pass `p`, and the tempo there.
    fn end_beat(&self, p: &Pass) -> (f64, f64) {
        let bpm = self.grid.bpm_at(p.t1.saturating_sub(1));
        let ours = self.grid.beat_in(self.grid.point(p.t1.saturating_sub(1)), p.t1) + p.k;
        (ours, bpm)
    }

    /// Place the pass after `p` on the grid: the session's if the replay
    /// is in one, else its own.
    fn place_after(&self, p: &Pass) -> Pass {
        let q = self.grid.quantum;
        let h_end = p.end();
        let (ours, bpm) = self.end_beat(p);
        let session = self.link.as_ref().map(|l| l.state()).filter(SessionState::joined);
        let beat_end = session.as_ref().map_or(ours, |s| s.beat_at(h_end));
        let beat_a = self.grid.beat_at(self.a);
        let mut r = (beat_a - beat_end).rem_euclid(q);
        if r > q - 1e-6 || r < 1e-6 {
            r = 0.0;
        }
        let target = beat_end + r;
        let h = match &session {
            Some(s) if r > 0.0 => s.host_at_beat(target),
            _ => h_end + (r * 60e6 / bpm).round() as Micros,
        };
        Pass { h: h.max(h_end), t0: self.a, t1: self.b, k: target - beat_a }
    }

    pub fn locate(&mut self, h: Micros) -> Pos {
        loop {
            let (n, p) = self.cur;
            if h < p.h {
                return match self.prev {
                    Some((pn, pp)) if h >= pp.h && h < pp.end() => Pos::In { n: pn, pass: pp, t: pp.t0 + (h - pp.h) as u64 },
                    Some((pn, pp)) => Pos::Gap { n: pn, pass: pp, next: p.h },
                    // Before the first pass starts: a gap before it.
                    None => Pos::Gap { n, pass: p, next: p.h },
                };
            }
            if h < p.end() {
                return Pos::In { n, pass: p, t: p.t0 + (h - p.h) as u64 };
            }
            let next = match self.next {
                Some(x) => x,
                None => {
                    let x = self.place_after(&p);
                    self.next = Some(x);
                    x
                }
            };
            if h < next.h {
                return Pos::Gap { n, pass: p, next: next.h };
            }
            self.prev = Some(self.cur);
            self.cur = (n + 1, next);
            self.next = None;
        }
    }

    /// The published clock at host time `h`.
    pub fn clock(&mut self, h: Micros) -> Sample {
        let grid = Arc::clone(&self.grid);
        let (bpm, beat, running) = match self.locate(h) {
            Pos::In { pass, t, .. } => (grid.bpm_at(t), grid.beat_at(t) + pass.k, grid.playing_at(t)),
            Pos::Gap { pass, next, .. } if h < pass.h => {
                // Before the first pass: count in to it.
                let bpm = grid.bpm_at(pass.t0);
                (bpm, grid.beat_at(pass.t0) + pass.k - (next - h) as f64 * bpm / 60e6, grid.playing_at(pass.t0))
            }
            Pos::Gap { pass, .. } => {
                let (beat, bpm) = self.end_beat(&pass);
                (bpm, beat + (h - pass.end()) as f64 * bpm / 60e6, grid.playing_at(pass.t1.saturating_sub(1)))
            }
        };
        Sample { bpm, beat, phase: beat.rem_euclid(1.0), running }
    }
}

/// Never re-fired on a seek or a seam (osc-signals.md's "event" entries).
pub fn is_event(addr: &str) -> bool {
    addr == "/tussen/clock/onbeat" || ["/press", "/tap", "/recenter", "/onset"].iter().any(|e| addr.ends_with(e))
}

/// Never replayed: hellos and byes (no phantom devices) and the recorded
/// clock (the replay publishes its own from the timeline).
pub fn is_skipped(addr: &str) -> bool {
    addr.starts_with("/tussen/hub/") || addr.starts_with(link::PREFIX)
}

/// Held state that must be let go when a jump lands where it was not held.
fn is_held(addr: &str) -> bool {
    addr.starts_with("/tussen/midi/note/") || addr.starts_with("/tussen/pad/")
}

/// The value of every non-event address as of take time `t` (its last
/// record before `t`), in first-seen order.
pub fn snapshot(take: &Take, t: u64) -> Vec<Message> {
    let mut at: HashMap<&str, usize> = HashMap::new();
    let mut order: Vec<&str> = Vec::new();
    for (i, e) in take.events.iter().enumerate() {
        if e.t >= t {
            break;
        }
        let a = e.msg.address.as_str();
        if is_skipped(a) || is_event(a) {
            continue;
        }
        if at.insert(a, i).is_none() {
            order.push(a);
        }
    }
    order.iter().map(|a| take.events[at[a]].msg.clone()).collect()
}

fn zero_like(m: &Message) -> Message {
    let v = match m.args.first() {
        Some(Value::Int32(_)) => Value::Int32(0),
        _ => Value::Float32(0.0),
    };
    Message::new(m.address.clone()).arg(v)
}

fn nonzero(m: &Message) -> bool {
    m.args.first().and_then(Value::as_float).is_some_and(|v| v != 0.0)
}

/// What the console's top bar shows while replaying.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Status {
    pub take: String,
    pub bar: f64,
    pub bars: usize,
    #[serde(rename = "loop", skip_serializing_if = "Option::is_none")]
    pub loop_bars: Option<(u32, u32)>,
}

/// A running replay. Dropping it stops it.
pub struct Replay {
    stop: Arc<AtomicBool>,
    pub plan: Arc<Mutex<Plan>>,
    pub grid: Arc<Grid>,
    link: Option<Arc<Link>>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Replay {
    /// The Link peer the replay leads with, if any.
    pub fn link(&self) -> Option<&Link> {
        self.link.as_deref()
    }
}

impl Drop for Replay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

/// Lead time between placing the first pass and playing it.
const START_MARGIN_US: Micros = 100_000;

/// Resolve the loop and start bar to take times `(start, a, b)`.
pub fn bounds(grid: &Grid, opts: &Options, end: u64) -> anyhow::Result<(u64, u64, u64)> {
    let bar = |n: u32| {
        grid.bar(n).ok_or_else(|| anyhow::anyhow!("the take has {} bar line(s); there is no bar {n}", grid.bars.len()))
    };
    let (a, b) = match opts.loop_bars {
        Some((a, b)) => {
            anyhow::ensure!(a >= 1 && b > a, "--loop wants <bar>..<bar>, first < second, from 1");
            (bar(a)?, bar(b)?)
        }
        None => (0, end),
    };
    let start = match opts.from_bar {
        Some(n) => bar(n)?,
        None => a,
    };
    anyhow::ensure!(start < b, "--from must lie before the loop's end");
    Ok((start, a, b))
}

/// Start replaying `take`: signals into `out` (route them as
/// [`crate::hub::Src::Replay`]), audio through `audio`'s RTP sender.
pub fn start(
    take: Arc<Take>,
    opts: &Options,
    out: Sender<Message>,
    audio: Option<(AudioConfig, Arc<AudioLink>)>,
    tap: Option<Arc<Tap>>,
) -> anyhow::Result<Replay> {
    let end = take.meta.duration_us.max(1);
    let grid = Arc::new(Grid::new(&take.links, opts.quantum, end));
    let (start, a, b) = bounds(&grid, opts, end)?;
    let lead_ms = opts.audio_lead_ms.unwrap_or(0);
    if lead_ms != 0 {
        log::info!("replay: audio nudged {} ms {} the signals (--offset-ms)", lead_ms.abs(), if lead_ms > 0 { "ahead of" } else { "behind" });
    }
    let lead_us = lead_ms as i64 * 1000;

    let link = match opts.link {
        LinkMode::Off => None,
        LinkMode::Lead => Some(Arc::new(
            Link::join_on(link::join_config(&LinkConfig::default()), opts.network)
                .map_err(|e| anyhow::anyhow!("joining Ableton Link: {e}"))?,
        )),
    };
    let clock = link.as_ref().map_or_else(HostClock::new, |l| l.clock());
    let (beat_s, bpm_s) = (grid.beat_at(start), grid.bpm_at(start));
    let first = match &link {
        None => Pass { h: clock.now() + START_MARGIN_US, t0: start, t1: b, k: 0.0 },
        Some(l) => {
            let until = Instant::now() + opts.link_wait;
            while !l.state().joined() && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(20));
            }
            let now = clock.now();
            let h = now + START_MARGIN_US;
            if l.state().joined() {
                l.propose_tempo(bpm_s, now);
                let s = l.state();
                let at = s.beat_at(h);
                let target = at + (beat_s - at).rem_euclid(grid.quantum);
                log::info!(
                    "replay: leading the Link session ({} other peer(s)) at {bpm_s:.2} BPM, on its bar grid",
                    s.peers
                );
                Pass { h: s.host_at_beat(target), t0: start, t1: b, k: target - beat_s }
            } else {
                log::info!("replay: no Link session — founding one at {bpm_s:.2} BPM");
                l.found(bpm_s, beat_s - (h - clock.now()) as f64 * bpm_s / 60e6);
                Pass { h, t0: start, t1: b, k: 0.0 }
            }
        }
    };
    let plan = Arc::new(Mutex::new(Plan::new(Arc::clone(&grid), first, a, b, link.clone())));
    let stop = Arc::new(AtomicBool::new(false));
    let mut threads = Vec::new();

    if let Some((cfg, audio_link)) = audio {
        if take.frames > 0 {
            let (ring_tx, ring_rx) = rtrb::RingBuffer::<f32>::new(cfg.packet_frames * cfg.channels.max(1) as usize * 64);
            stream_ring(cfg.clone(), Arc::clone(&audio_link), ring_rx)?;
            let pacer = Pacer {
                take: Arc::clone(&take),
                plan: Arc::clone(&plan),
                clock,
                lead_us,
                channels: take.meta.channels.max(1) as usize,
                rate: take.meta.sample_rate.max(1) as f64,
                block: cfg.packet_frames,
            };
            let stop = Arc::clone(&stop);
            threads.push(
                std::thread::Builder::new()
                    .name("simply-tussen replay audio".into())
                    .spawn(move || pacer.run(ring_tx, &stop, &audio_link))?,
            );
        } else {
            log::info!("replay: the take holds no audio");
        }
    }

    let status = Status {
        take: take.dir.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
        bar: 0.0,
        bars: grid.bars.len(),
        loop_bars: opts.loop_bars,
    };
    let events = Events {
        take: Arc::clone(&take),
        grid: Arc::clone(&grid),
        plan: Arc::clone(&plan),
        clock,
        link: link.clone(),
        out,
        tap,
        status,
    };
    let stop2 = Arc::clone(&stop);
    threads.push(std::thread::Builder::new().name("simply-tussen replay".into()).spawn(move || events.run(&stop2))?);
    Ok(Replay { stop, plan, grid, link, threads })
}

/// The event thread's state.
struct Events {
    take: Arc<Take>,
    grid: Arc<Grid>,
    plan: Arc<Mutex<Plan>>,
    clock: HostClock,
    link: Option<Arc<Link>>,
    out: Sender<Message>,
    tap: Option<Arc<Tap>>,
    status: Status,
}

impl Events {
    fn run(mut self, stop: &AtomicBool) {
        if !crate::timing::precise_wakeups() {
            log::warn!("replay: precise timer wakeups refused; signals may land a few ms late");
        }
        let mut pass_n: Option<u64> = None;
        // The next event of the current pass.
        let mut next = 0usize;
        // What consumers last got, per address (for state on a jump).
        let mut sent: HashMap<String, Message> = HashMap::new();
        let mut snapshots: HashMap<u64, Vec<Message>> = HashMap::new();
        let mut beats = Beats::default();
        let mut next_tick: Micros = 0;
        let mut proposed: Option<(f64, bool)> = None;
        let mut last_console = Instant::now() - Duration::from_secs(1);
        let mut last_drift = Instant::now();
        let send = |m: Message| self.out.send(m).is_ok();
        while !stop.load(Ordering::Relaxed) {
            let now = self.clock.now();
            let pos = self.plan.lock().unwrap().locate(now);
            let mut wake = now + link::PERIOD_US;
            if let Pos::In { n, pass, t } = pos {
                if pass_n != Some(n) {
                    pass_n = Some(n);
                    next = self.take.events.partition_point(|e| e.t < pass.t0);
                    // The world as it was at the pass start.
                    let snap = snapshots.entry(pass.t0).or_insert_with(|| snapshot(&self.take, pass.t0));
                    let have: std::collections::HashSet<&str> = snap.iter().map(|m| m.address.as_str()).collect();
                    let mut restore: Vec<Message> =
                        snap.iter().filter(|m| sent.get(&m.address) != Some(*m)).cloned().collect();
                    restore.extend(
                        sent.values().filter(|m| is_held(&m.address) && !have.contains(m.address.as_str()) && nonzero(m)).map(zero_like),
                    );
                    for m in restore {
                        sent.insert(m.address.clone(), m.clone());
                        if !send(m) {
                            return;
                        }
                    }
                    log::debug!("replay: pass {n} from {} µs", pass.t0);
                }
                // Every event due by now, in order.
                while let Some(e) = self.take.events.get(next) {
                    if e.t >= pass.t1 {
                        break;
                    }
                    let due = pass.h + (e.t - pass.t0) as Micros;
                    if due > now {
                        wake = wake.min(due);
                        break;
                    }
                    next += 1;
                    let a = &e.msg.address;
                    if is_skipped(a) {
                        continue;
                    }
                    if !is_event(a) {
                        sent.insert(a.clone(), e.msg.clone());
                    }
                    if !send(e.msg.clone()) {
                        return;
                    }
                }
                wake = wake.min(pass.end());
                // Link: every recorded tempo change and start/stop, at its time.
                if let Some(l) = &self.link {
                    let (bpm, playing) = (self.grid.bpm_at(t), self.grid.playing_at(t));
                    let at = pass.h + self.grid.segment_start(t).saturating_sub(pass.t0) as Micros;
                    if proposed.is_none_or(|(b, _)| (b - bpm).abs() > 1e-4) {
                        l.propose_tempo(bpm, at.min(now));
                    }
                    if proposed.is_none_or(|(_, p)| p != playing) && (proposed.is_some() || !playing) {
                        l.propose_start_stop(playing, at.min(now));
                    }
                    proposed = Some((bpm, playing));
                }
                self.status.bar = self.grid.bar_at(t);
            } else if let Pos::Gap { next: h, .. } = pos {
                wake = wake.min(h);
            }

            // The clock: 60 Hz, and onbeat at the beat's own time.
            let s = self.plan.lock().unwrap().clock(now);
            if let Some(b) = beats.update(s.beat, s.running) {
                if !send(link::onbeat(b)) {
                    return;
                }
            }
            if now >= next_tick {
                for m in s.messages() {
                    if !send(m) {
                        return;
                    }
                }
                next_tick = now + link::PERIOD_US;
            }
            wake = wake.min(next_tick);
            if s.running && s.bpm > 0.0 {
                let to_beat = (s.beat.floor() + 1.0 - s.beat) * 60e6 / s.bpm;
                wake = wake.min(now + to_beat as Micros + 1);
            }

            if let Some(l) = &self.link {
                let st = l.state();
                if st.joined() && last_drift.elapsed() >= Duration::from_secs(10) {
                    let off = st.beat_at(now) - s.beat;
                    if off.abs() > 0.05 {
                        last_drift = Instant::now();
                        log::warn!("replay: the Link session is {off:+.2} beats off the take; re-placing at the next seam");
                    }
                }
            }
            if let Some(tap) = self.tap.as_ref().filter(|t| t.active()) {
                if last_console.elapsed() >= Duration::from_millis(250) {
                    last_console = Instant::now();
                    let (mode, peers, peer_list) = match &self.link {
                        Some(l) => {
                            let st = l.state();
                            let mode = if st.founder { Mode::Started } else { Mode::of(&st, false) };
                            (mode, st.peers, link::peer_rows(&l.peers(), now, &[], |_| None))
                        }
                        None => (Mode::Waiting, 0, Vec::new()),
                    };
                    tap.send(Event::Link(LinkStatus {
                        ts: crate::console::us_at(self.clock.instant(now)),
                        beat: s.beat,
                        bpm: s.bpm,
                        quantum: self.grid.quantum,
                        playing: s.running,
                        mode,
                        peers,
                        peer_list,
                        replay: Some(self.status.clone()),
                    }));
                }
            }
            let wake = wake.max(now + 200).min(now + link::PERIOD_US);
            std::thread::sleep(self.clock.instant(wake).saturating_duration_since(Instant::now()));
        }
    }
}

/// Pushes the take's audio into the RTP sender's ring in real time.
struct Pacer {
    take: Arc<Take>,
    plan: Arc<Mutex<Plan>>,
    clock: HostClock,
    lead_us: i64,
    channels: usize,
    rate: f64,
    block: usize,
}

/// A seam's fade, in frames (5 ms at 48 kHz).
const FADE: u64 = 240;
/// The pacer re-syncs to the anchors when the sample clock is this far off.
const RESYNC_FRAMES: i64 = 960;

impl Pacer {
    /// The WAV frame for take time `t`, any `--offset-ms` nudge applied.
    fn frame(&self, t: u64) -> u64 {
        self.take.frame_at((t as i64 + self.lead_us).max(0) as u64).unwrap_or(0)
    }

    fn run(self, mut ring: rtrb::Producer<f32>, stop: &AtomicBool, link: &AudioLink) {
        let _ = crate::timing::precise_wakeups();
        let ch = self.channels;
        let mut block = vec![0f32; self.block * ch];
        let h0 = self.plan.lock().unwrap().cur.1.h.min(self.clock.now());
        let mut n: u64 = 0; // frames emitted
        // (pass number, next frame, pass's first and end frame)
        let mut cursor: Option<(u64, u64, u64, u64)> = None;
        while !stop.load(Ordering::Relaxed) && !link.stop.load(Ordering::Relaxed) {
            let h_block = h0 + (n as f64 * 1e6 / self.rate) as Micros;
            let now = self.clock.now();
            if h_block > now {
                std::thread::sleep(Duration::from_micros((h_block - now) as u64));
                continue;
            }
            let mut i = 0;
            while i < self.block {
                let h = h0 + ((n + i as u64) as f64 * 1e6 / self.rate) as Micros;
                let pos = self.plan.lock().unwrap().locate(h);
                let Pos::In { n: pn, pass, t } = pos else {
                    block[i * ch..(i + 1) * ch].fill(0.0);
                    i += 1;
                    continue;
                };
                let expected = self.frame(t);
                let (first, end) = (self.frame(pass.t0), self.frame(pass.t1));
                let mut at = match cursor {
                    Some((cn, f, ..)) if cn == pn && (f as i64 - expected as i64).abs() <= RESYNC_FRAMES => f,
                    _ => expected,
                };
                if at >= end {
                    block[i * ch..(i + 1) * ch].fill(0.0);
                    i += 1;
                    cursor = Some((pn, at, first, end));
                    continue;
                }
                let take = (self.block - i).min((end - at) as usize);
                self.take.read_audio(at, &mut block[i * ch..(i + take) * ch]);
                for j in 0..take {
                    let f = at + j as u64;
                    let gain = ((f - first + 1).min(end - f) as f32 / FADE as f32).min(1.0);
                    if gain < 1.0 {
                        for s in &mut block[(i + j) * ch..(i + j + 1) * ch] {
                            *s *= gain;
                        }
                    }
                }
                at += take as u64;
                i += take;
                cursor = Some((pn, at, first, end));
            }
            if ring.slots() >= block.len() {
                if let Ok(chunk) = ring.write_chunk_uninit(block.len()) {
                    chunk.fill_from_iter(block.iter().copied());
                }
            }
            n += self.block as u64;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pt(t: u64, bpm: f64, beat: f64) -> LinkPoint {
        LinkPoint { t, bpm, beat, playing: true, mode: 0, quantum: 4.0 }
    }

    #[test]
    fn bars_follow_the_recorded_beat_through_a_tempo_change() {
        // 120 BPM from beat 1 (bar line at beat 4 = 1.5 s), 60 BPM from 5 s (beat 9).
        let g = Grid::new(&[pt(0, 120.0, 1.0), pt(5_000_000, 60.0, 11.0)], 4.0, 20_000_000);
        let bars: Vec<(u64, f64)> = g.bars.clone();
        assert_eq!(bars[0], (1_500_000, 4.0));
        assert_eq!(bars[1], (3_500_000, 8.0));
        assert_eq!(bars[2], (6_000_000, 12.0)); // 1 beat at 60 BPM after 5 s
        assert_eq!(bars[3], (10_000_000, 16.0));
        assert_eq!(g.bar(1), Some(1_500_000));
        assert!((g.bar_at(4_500_000) - 2.5).abs() < 1e-9);
        assert_eq!(g.bpm_at(4_999_999), 120.0);
        assert_eq!(g.segment_start(7_000_000), 5_000_000);
    }

    #[test]
    fn a_whole_bar_loop_keeps_the_beat_counting_with_no_gap() {
        let g = Arc::new(Grid::new(&[pt(0, 120.0, 0.0)], 4.0, 20_000_000));
        let (a, b) = (g.bar(2).unwrap(), g.bar(4).unwrap()); // beats 4..12, 2 s..6 s
        let first = Pass { h: 1_000_000, t0: a, t1: b, k: 0.0 };
        let mut plan = Plan::new(Arc::clone(&g), first, a, b, None);
        let mut last = f64::MIN;
        for h in (1_000_000..30_000_000).step_by(10_000) {
            let s = plan.clock(h);
            assert!(s.beat > last, "beat goes up at {h}");
            last = s.beat;
        }
        // Pass 3 starts exactly 3 × 4 s after pass 0, at beat 4 + 3·8.
        let mut plan = Plan::new(Arc::clone(&g), first, a, b, None);
        match plan.locate(13_000_000) {
            Pos::In { n, pass, t } => {
                assert_eq!((n, pass.h, t, pass.k), (3, 13_000_000, a, 24.0));
            }
            p => panic!("{p:?}"),
        }
    }

    #[test]
    fn the_whole_take_loops_onto_the_next_bar_in_phase() {
        // A 5 s take at 120 BPM from beat 0: ends at beat 10, mid-bar.
        let g = Arc::new(Grid::new(&[pt(0, 120.0, 0.0)], 4.0, 5_000_000));
        let mut plan = Plan::new(Arc::clone(&g), Pass { h: 0, t0: 0, t1: 5_000_000, k: 0.0 }, 0, 5_000_000, None);
        // Beat 10 → the next beat ≡ 0 mod 4 is 12: 1 s of gap.
        assert!(matches!(plan.locate(5_500_000), Pos::Gap { next: 6_000_000, .. }));
        match plan.locate(6_000_000) {
            Pos::In { n: 1, pass, t: 0 } => assert_eq!(pass.k, 12.0),
            p => panic!("{p:?}"),
        }
        assert!((plan.clock(5_500_000).beat - 11.0).abs() < 1e-9);
    }

    #[test]
    fn events_and_hellos_are_told_apart() {
        assert!(is_event("/tussen/quest/controller/a/press"));
        assert!(is_event("/tussen/clock/onbeat"));
        assert!(!is_event("/tussen/midi/note/60"));
        assert!(is_skipped("/tussen/hub/hello"));
        assert!(is_skipped("/tussen/clock/beat"));
        assert!(!is_skipped("/djay/deck/1/bpm"));
    }
}
