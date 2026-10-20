//! The clock generator: simply-tussen joins Ableton Link and
//! publishes the session's beat under `/tussen/clock/…`.
//!
//! **Waiting, not founding.** Link has no master, and between two
//! sessions the older one wins — so a hub that founded a session at its
//! own tempo would impose it on every DJ who switches Link on later (djay
//! Pro adopting the hub's 120 slowed the playing track). So the hub never
//! founds by itself. It starts **waiting** ([`Mode::Waiting`]): it listens
//! for Link peers but announces nothing, and publishes the clock from a
//! private timeline at `[link] tempo` (console tempo and start/stop change
//! it), so the rig has a beat. The first session it hears it **joins**
//! ([`Mode::Joined`]) as the newcomer, at that session's tempo and beat;
//! nobody else changes anything.
//!
//! (A replay — `simply-tussen replay`, [`crate::replay`] — is the exception: this
//! generator does not run, and the replay leads the session with the
//! take's timeline.)
//!
//! **Starting a session** is the human's call: the console's Start
//! session founds one now, at the private timeline's tempo and beat
//! ([`Mode::Started`]) — for when nobody else will join.
//!
//! **The rule for going back** ([`should_leave`]): a session the hub
//! *joined* it leaves when every other peer in it has gone (said goodbye
//! or gone quiet past Link's ttl), and it waits again — the private
//! timeline carrying on from the session's beat. A session started from
//! the console it stays in until the hub stops.
//!
//! **Following.** In a session, a console command ([`LinkCmd`]) proposes
//! tempo or start/stop once, at the moment the human acts; the hub keeps
//! no stance of its own to re-assert, so a merge or a joining peer never
//! hears the hub's old tempo. [`propose`]'s one call site is the console
//! channel: session changes structurally cannot produce proposals. A
//! session the hub started meets later ones by Link's own rule (the older
//! wins), as any running session does. The log says which session the hub
//! is in and every tempo it follows.
//!
//! **Signals** (MIDI class, like `/tussen/midi/…`): `bpm`, `beat` (the
//! session's beat), `phase` (`frac(beat)`) and `running` (0/1), all `f`,
//! at 60 Hz; `onbeat` (`i`, the beat index) when a whole beat passes,
//! sent at the beat's own time rather than on the next 60 Hz tick.
//! `running` is the session's start/stop state; a session nobody starts
//! or stops runs. While waiting they come from the private timeline. On
//! joining, `beat` jumps to the session's (whose beat it is, lined up with
//! the music, is the point); starting a session and going back to waiting
//! carry the beat on without a jump.
//!
//! `beat` goes out as a 32-bit float: the private timeline starts at beat
//! 0, so it holds a millisecond's precision for over 9 hours at 120 BPM.
//!
//! **Peers.** The console's `link` frame also lists every Link peer the hub
//! hears ([`peer_rows`]): where it is (the address it is heard from, a
//! name for it by reverse DNS, or "this machine"), whether it is in the
//! hub's session, its tempo and transport, and how long ago it was last
//! heard. Link's messages carry no application name, so which app a peer
//! is cannot be shown — only where it runs.
//!
//! The pure parts — [`sample`], [`Beats`], [`next_wake`], [`peer_rows`],
//! [`should_leave`], [`private`] — take plain data and a host time, so the tests drive them from a fake
//! Link timeline with no sockets.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use serde::{Deserialize, Serialize};
use simply_link::timeline::{GhostXForm, MICROBEATS};
use simply_link::{Link, Micros, Network, PeerInfo, SessionState, StartStop, Status, Timeline};
use simply_osc::{Message, Value};

use crate::console::{self, Event, Tap};
use crate::generators::Generator;
use crate::record::{self, Slot};
use crate::take::Recorder;

/// Everything published lives under this prefix.
pub const PREFIX: &str = "/tussen/clock/";
/// µs between two publishes of the continuous signals (60 Hz).
pub const PERIOD_US: Micros = 16_667;
/// How often the console page gets the session state (~4 Hz).
const CONSOLE_PERIOD: Duration = Duration::from_millis(250);

/// `[link]`.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default)]
pub struct LinkConfig {
    /// The tempo of the hub's own beat while it waits for a Link session,
    /// and of a session started from the console. Never imposed on a
    /// session the hub joins: that session's tempo wins.
    pub tempo: f64,
    /// Beats per bar: the console draws a bar line every `quantum` beats.
    pub quantum: f64,
}

impl Default for LinkConfig {
    fn default() -> Self {
        LinkConfig { tempo: 120.0, quantum: 4.0 }
    }
}

/// A console command for the session.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum LinkCmd {
    Tempo(f64),
    Playing(bool),
    /// Start a session now (if waiting).
    Start,
}

/// Where the hub is with Link (module doc).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// In no session: listening, silent, beat from the private timeline.
    Waiting,
    /// In a session it joined; leaves when the others have gone.
    Joined,
    /// Started a session from the console; stays until the hub stops.
    Started,
}

impl Mode {
    pub fn of(state: &SessionState, started: bool) -> Mode {
        match (state.joined(), started) {
            (false, _) => Mode::Waiting,
            (true, true) => Mode::Started,
            (true, false) => Mode::Joined,
        }
    }
}

/// The rule for going back to waiting: in a session the hub joined, with
/// nobody else left in it. Never for a session started from the console.
pub fn should_leave(state: &SessionState, started: bool) -> bool {
    Mode::of(state, started) == Mode::Joined && state.peers == 0
}

/// The hub's own beat while it waits: `bpm`, the beat at host time `host`
/// being `beat`, on the host clock — a session of one, never announced, in
/// the shape of a [`SessionState`] so [`sample`] and [`next_wake`] read it
/// like any session.
pub fn private(bpm: f64, beat: f64, host: Micros) -> SessionState {
    SessionState {
        status: Status::Joined,
        session: None,
        peers: 0,
        founder: false,
        timeline: Timeline::at(bpm, beat, host),
        xform: GhostXForm { intercept: 0 },
        start_stop: None,
    }
}

/// The private timeline that carries on from `session`'s beat, tempo and
/// transport — what the hub falls back to when it leaves.
pub fn carry_on(session: &SessionState) -> SessionState {
    SessionState { session: None, peers: 0, founder: false, status: Status::Joined, ..*session }
}

/// A console command applied to the private timeline at host time `at`.
pub fn apply_private(state: &mut SessionState, cmd: LinkCmd, at: Micros) {
    let ghost = state.xform.host_to_ghost(at);
    match cmd {
        LinkCmd::Tempo(bpm) => state.timeline = state.timeline.retempo(bpm, ghost),
        LinkCmd::Playing(playing) => {
            let beats = (state.timeline.beat_at(ghost) * MICROBEATS).round() as i64;
            state.start_stop = Some(StartStop { playing, beats, timestamp: ghost });
        }
        LinkCmd::Start => {}
    }
}

/// What the console page draws the beat grid from (the `link` frame).
#[derive(Clone, Debug, PartialEq)]
pub struct LinkStatus {
    /// Hub clock µs ([`console::now_us`]).
    pub ts: u64,
    /// The session's beat at `ts`.
    pub beat: f64,
    pub bpm: f64,
    pub quantum: f64,
    pub playing: bool,
    pub mode: Mode,
    /// Other peers in the hub's session.
    pub peers: usize,
    /// Every peer heard, in or out of the session ([`peer_rows`]).
    pub peer_list: Vec<PeerRow>,
    /// Set while replaying a take: what the top bar shows.
    pub replay: Option<crate::replay::Status>,
}

/// One Link peer as the console lists it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PeerRow {
    /// Link's node id — what tells two peers on one host apart.
    pub node: String,
    /// The session it announces.
    pub session: String,
    /// Is it in the hub's session?
    pub in_session: bool,
    /// The address it is heard from: its LAN address if it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// The host's name by reverse DNS, once known (never for `local`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    /// Running on the hub's own machine.
    pub local: bool,
    pub bpm: f64,
    /// Its start/stop state; absent if it never set one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub playing: Option<bool>,
    /// How long ago it was last heard.
    pub age_ms: u64,
}

/// The console's rows for `peers` at host time `now`: in-session peers
/// first, then by host. A peer is `local` if it is heard over loopback or
/// from one of `own` (this machine's addresses); its `host` is the first
/// non-loopback address it is heard from, and `name` is asked for that
/// address's hostname (only for peers on other machines).
pub fn peer_rows(
    peers: &[PeerInfo],
    now: Micros,
    own: &[IpAddr],
    name: impl Fn(IpAddr) -> Option<String>,
) -> Vec<PeerRow> {
    let mut rows: Vec<PeerRow> = peers
        .iter()
        .map(|p| {
            let from = || p.paths.iter().map(|h| h.from);
            let local = from().any(|ip| ip.is_loopback() || own.contains(&ip));
            let host = from().find(|ip| !ip.is_loopback()).or_else(|| from().next());
            PeerRow {
                node: p.node.to_string(),
                session: p.session.to_string(),
                in_session: p.in_session,
                host: host.map(|ip| ip.to_string()),
                hostname: host.filter(|_| !local).and_then(&name),
                local,
                bpm: p.timeline.bpm(),
                playing: p.start_stop.map(|s| s.playing),
                age_ms: (now - p.heard).max(0) as u64 / 1000,
            }
        })
        .collect();
    rows.sort_by(|a, b| (!a.in_session, &a.host, &a.node).cmp(&(!b.in_session, &b.host, &b.node)));
    rows
}

/// This machine's addresses, re-read every [`simply_link::RESCAN`] — what
/// tells a peer on the hub's own machine from one across the room.
struct OwnAddrs {
    ips: Vec<IpAddr>,
    read: Option<Instant>,
}

impl OwnAddrs {
    fn get(&mut self) -> &[IpAddr] {
        if self.read.is_none_or(|t| t.elapsed() >= simply_link::RESCAN) {
            self.ips =
                simply_link::interfaces(&Network::default()).iter().map(|i| IpAddr::V4(i.addr)).collect();
            self.read = Some(Instant::now());
        }
        &self.ips
    }
}

/// The continuous clock signals at one instant.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Sample {
    pub bpm: f64,
    pub beat: f64,
    pub phase: f64,
    pub running: bool,
}

/// Is the session's beat running? Its start/stop state; a session nobody
/// starts or stops runs.
pub fn running(state: &SessionState) -> bool {
    state.joined() && state.is_playing().unwrap_or(true)
}

/// The clock signals at host time `host`; `None` until joined.
pub fn sample(state: &SessionState, host: Micros) -> Option<Sample> {
    if !state.joined() {
        return None;
    }
    let beat = state.beat_at(host);
    Some(Sample { bpm: state.tempo(), beat, phase: beat.rem_euclid(1.0), running: running(state) })
}

impl Sample {
    /// `bpm`, `beat`, `phase`, `running`.
    pub fn messages(&self) -> [Message; 4] {
        let f = |name: &str, v: f64| Message::new(format!("{PREFIX}{name}")).arg(Value::Float32(v as f32));
        [
            f("bpm", self.bpm),
            f("beat", self.beat),
            f("phase", self.phase),
            f("running", if self.running { 1.0 } else { 0.0 }),
        ]
    }
}

/// The `onbeat` event for beat index `beat`.
pub fn onbeat(beat: i64) -> Message {
    Message::new(format!("{PREFIX}onbeat")).arg(Value::Int32(beat as i32))
}

/// Fires `onbeat` once per whole beat while running. Stopping, or the
/// beat going backwards (a timeline jump), re-arms without firing; a jump
/// forward fires once, for the beat it lands in.
#[derive(Debug, Default)]
pub struct Beats {
    last: Option<i64>,
}

impl Beats {
    pub fn update(&mut self, beat: f64, running: bool) -> Option<i64> {
        if !running {
            self.last = None;
            return None;
        }
        let now = beat.floor() as i64;
        let fired = match self.last {
            Some(last) if now > last => Some(now),
            _ => None,
        };
        self.last = Some(now);
        fired
    }
}

/// A console command, clamped to what the session can hold. Proposed
/// once, when the human acts; nothing is remembered to re-assert later
/// (module doc — the hub follows between console actions).
pub fn clamp(cmd: LinkCmd) -> LinkCmd {
    match cmd {
        LinkCmd::Tempo(bpm) => {
            LinkCmd::Tempo(bpm.clamp(simply_link::timeline::MIN_BPM, simply_link::timeline::MAX_BPM))
        }
        LinkCmd::Playing(_) | LinkCmd::Start => cmd,
    }
}

/// The `simply_link` join config: never found by itself (module doc) —
/// a session is only ever joined, or started from the console.
pub fn join_config(config: &LinkConfig) -> simply_link::Config {
    simply_link::Config { bpm: config.tempo, found_after: None, ..simply_link::Config::default() }
}

/// The host time to wake at: the next 60 Hz tick, or the next whole beat
/// if that comes first while running (so `onbeat` is on time).
pub fn next_wake(state: &SessionState, now: Micros, next_tick: Micros) -> Micros {
    if !running(state) {
        return next_tick;
    }
    let next_beat = state.host_at_beat(state.beat_at(now).floor() + 1.0) + 1;
    next_tick.min(next_beat.max(now))
}

/// The published timeline into a take: on every change (tempo,
/// transport, mode, a jump in the beat) and once a second.
#[derive(Default)]
pub struct TimelineTake {
    last: Option<(SessionState, Mode, Micros)>,
}

impl TimelineTake {
    pub fn record(&mut self, rec: &Recorder, s: &SessionState, mode: Mode, quantum: f64, at: Instant, now: Micros) {
        let changed = match &self.last {
            None => true,
            Some((last, last_mode, when)) => {
                *last_mode != mode
                    || (last.tempo() - s.tempo()).abs() > 1e-6
                    || running(last) != running(s)
                    || (last.beat_at(now) - s.beat_at(now)).abs() > 1e-3
                    || now - when >= 1_000_000
            }
        };
        if changed {
            let mode_id = match mode {
                Mode::Waiting => 0,
                Mode::Joined => 1,
                Mode::Started => 2,
            };
            rec.link(at, s.tempo(), s.beat_at(now), running(s), mode_id, quantum);
            self.last = Some((*s, mode, now));
        }
    }
}

/// The generator: joins Link in `start`, then runs its own thread.
pub struct LinkGenerator {
    config: LinkConfig,
    cmds: Receiver<LinkCmd>,
    tap: Option<Arc<Tap>>,
    recorder: Option<Slot>,
}

impl LinkGenerator {
    /// `cmds` and `tap` come from the console, if there is one.
    pub fn new(config: LinkConfig, cmds: Receiver<LinkCmd>, tap: Option<Arc<Tap>>) -> LinkGenerator {
        LinkGenerator { config, cmds, tap, recorder: None }
    }

    /// Recording on demand: the published timeline goes into whatever
    /// take the slot holds, starting with the timeline as it is.
    pub fn recording(mut self, recorder: Option<Slot>) -> LinkGenerator {
        self.recorder = recorder;
        self
    }
}

impl Generator for LinkGenerator {
    fn name(&self) -> &'static str {
        "link"
    }

    fn start(&mut self, out: Sender<Message>) -> anyhow::Result<()> {
        let config = self.config.clone();
        let link = Link::join(join_config(&config))
            .map_err(|e| anyhow::anyhow!("joining Ableton Link: {e}"))?;
        log::info!(
            "link: waiting for a Link session to join (own beat at {} BPM until then), quantum {}",
            config.tempo,
            config.quantum
        );
        let cmds = self.cmds.clone();
        let tap = self.tap.clone();
        let rec = self.recorder.clone();
        std::thread::Builder::new()
            .name("simply-tussen link".into())
            .spawn(move || run_link(&link, &config, &cmds, tap.as_deref(), rec.as_ref(), &out))?;
        Ok(())
    }
}

fn propose(link: &Link, cmd: LinkCmd) {
    let now = link.clock().now();
    match cmd {
        LinkCmd::Tempo(bpm) => {
            log::debug!("link: tempo → {bpm:.2} BPM");
            link.propose_tempo(bpm, now);
        }
        LinkCmd::Playing(p) => {
            log::info!("link: {}", if p { "start" } else { "stop" });
            link.propose_start_stop(p, now);
        }
        LinkCmd::Start => {} // not a proposal: see `command`
    }
}

/// A console command: proposed to the session if the hub is in one, else
/// applied to the private timeline. Start founds a session from the
/// private timeline — its tempo, beat and transport — if the hub is
/// waiting.
fn command(link: &Link, private: &mut SessionState, started: &mut bool, cmd: LinkCmd) {
    let now = link.clock().now();
    let joined = link.state().joined();
    match cmd {
        LinkCmd::Start if joined => log::info!("link: already in a session; nothing to start"),
        LinkCmd::Start => {
            log::info!("link: starting a session at {:.2} BPM (console)", private.tempo());
            link.found(private.tempo(), private.beat_at(now));
            if let Some(playing) = private.is_playing() {
                link.propose_start_stop(playing, now);
            }
            *started = link.state().founder;
        }
        _ if joined => propose(link, cmd),
        _ => {
            log::debug!("link: own beat (waiting) — {cmd:?}");
            apply_private(private, cmd, now);
        }
    }
}

fn run_link(
    link: &Link,
    config: &LinkConfig,
    cmds: &Receiver<LinkCmd>,
    tap: Option<&Tap>,
    slot: Option<&Slot>,
    out: &Sender<Message>,
) {
    let mut taken = TimelineTake::default();
    let mut rec: Option<Arc<Recorder>> = None;
    let clock = link.clock();
    let mut beats = Beats::default();
    let mut next_tick = clock.now();
    let mut last_console = Instant::now() - CONSOLE_PERIOD;
    let mut last_status: Option<(String, usize)> = None;
    let mut logged_bpm = (0.0, Instant::now());
    let names = crate::rdns::Names::system();
    let mut own = OwnAddrs { ips: Vec::new(), read: None };
    let mut private = private(config.tempo, 0.0, clock.now());
    let mut started = false;
    loop {
        let mut changed = false;
        // The only place the hub proposes anything: a human's console
        // command. Session changes never do (module doc).
        while let Ok(cmd) = cmds.try_recv() {
            command(link, &mut private, &mut started, clamp(cmd));
            changed = true;
        }
        let mut state = link.state();
        if should_leave(&state, started) {
            log::info!("link: everyone else has left the session — waiting for the next one");
            link.leave();
            private = carry_on(&state);
            state = link.state();
            changed = true;
        }
        if state.joined() {
            private = carry_on(&state);
        }
        let mode = Mode::of(&state, started);
        // The beat published: the session's, or the private one while waiting.
        let beat = if state.joined() { state } else { private };
        let session = state.session.map_or_else(|| "-".into(), |s| s.to_string());
        let status = (format!("{mode:?} ({:?}) in session {session}", state.status), state.peers);
        if last_status.as_ref() != Some(&status) {
            log::info!("link: {} at {:.2} BPM, {} peer(s)", status.0, beat.tempo(), status.1);
            last_status = Some(status);
            logged_bpm = (beat.tempo(), Instant::now());
        }
        // A tempo the hub follows, at most twice a second while a DJ rides
        // the fader — debug only: an hour of riding is not news.
        if state.joined()
            && (state.tempo() - logged_bpm.0).abs() >= 0.01
            && logged_bpm.1.elapsed() >= Duration::from_millis(500)
        {
            log::debug!("link: following {:.2} BPM", state.tempo());
            logged_bpm = (state.tempo(), Instant::now());
        }

        let now = clock.now();
        if let Some(slot) = slot {
            let cur = record::current(slot);
            if record::changed(&rec, &cur) {
                // A new take: its timeline starts with the current one.
                taken = TimelineTake::default();
                rec = cur;
            }
        }
        if let Some(rec) = &rec {
            taken.record(rec, &beat, mode, config.quantum, clock.instant(now), now);
        }
        if let Some(s) = sample(&beat, now) {
            if let Some(b) = beats.update(s.beat, s.running) {
                if out.send(onbeat(b)).is_err() {
                    return;
                }
            }
            if now >= next_tick {
                for m in s.messages() {
                    if out.send(m).is_err() {
                        return;
                    }
                }
            }
            if let Some(tap) = tap.filter(|t| t.active()) {
                if changed || last_console.elapsed() >= CONSOLE_PERIOD {
                    last_console = Instant::now();
                    tap.send(Event::Link(LinkStatus {
                        ts: console::us_at(clock.instant(now)),
                        beat: s.beat,
                        bpm: s.bpm,
                        quantum: config.quantum,
                        playing: s.running,
                        mode,
                        peers: state.peers,
                        peer_list: peer_rows(&link.peers(), now, own.get(), |ip| names.get(ip)),
                        replay: None,
                    }));
                }
            }
        }
        while next_tick <= now {
            next_tick += PERIOD_US;
        }
        let wake = next_wake(&beat, now, next_tick);
        std::thread::sleep(clock.instant(wake).saturating_duration_since(Instant::now()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use simply_link::NodeId;

    /// A joined session at `bpm` whose beat 0 is at host time `origin`.
    fn session(bpm: f64, origin: Micros, playing: Option<bool>) -> SessionState {
        SessionState {
            status: Status::Joined,
            session: Some(NodeId(*b"session1")),
            peers: 1,
            founder: false,
            // Ghost time 1 s ahead of host time: the xform must be applied.
            timeline: Timeline::at(bpm, 0.0, origin + 1_000_000),
            xform: GhostXForm { intercept: 1_000_000 },
            start_stop: playing.map(|playing| StartStop { playing, beats: 0, timestamp: 0 }),
        }
    }

    fn f(m: &Message) -> f32 {
        match m.args[..] {
            [Value::Float32(v)] => v,
            _ => panic!("not one float: {m:?}"),
        }
    }

    #[test]
    fn the_clock_signals_follow_the_session_timeline() {
        let s = session(120.0, 10_000_000, None);
        // 1.25 s after beat 0 at 120 BPM = beat 2.5.
        let smp = sample(&s, 11_250_000).unwrap();
        assert_eq!(smp, Sample { bpm: 120.0, beat: 2.5, phase: 0.5, running: true });
        let msgs = smp.messages();
        let addrs: Vec<&str> = msgs.iter().map(|m| m.address.as_str()).collect();
        assert_eq!(addrs, ["/tussen/clock/bpm", "/tussen/clock/beat", "/tussen/clock/phase", "/tussen/clock/running"]);
        assert_eq!(msgs.iter().map(f).collect::<Vec<_>>(), [120.0, 2.5, 0.5, 1.0]);
        // 90 BPM, 3 s in: beat 4.5 (to the wire's µs-per-beat rounding).
        let smp = sample(&session(90.0, 0, None), 3_000_000).unwrap();
        assert!((smp.beat - 4.5).abs() < 1e-5 && (smp.phase - 0.5).abs() < 1e-5);
        // Before beat 0 the phase stays in [0, 1).
        let smp = sample(&s, 9_750_000).unwrap();
        assert_eq!((smp.beat, smp.phase), (-0.5, 0.5));
    }

    #[test]
    fn running_is_the_transport_and_a_session_nobody_stops_runs() {
        assert!(sample(&session(120.0, 0, None), 0).unwrap().running);
        assert!(sample(&session(120.0, 0, Some(true)), 0).unwrap().running);
        let stopped = sample(&session(120.0, 0, Some(false)), 0).unwrap();
        assert!(!stopped.running);
        assert_eq!(f(&stopped.messages()[3]), 0.0);
        let mut searching = session(120.0, 0, None);
        searching.status = Status::Searching;
        assert_eq!(sample(&searching, 0), None);
    }

    #[test]
    fn onbeat_fires_once_per_whole_beat_while_running() {
        let mut b = Beats::default();
        assert_eq!(b.update(2.9, true), None); // arming, mid-beat
        assert_eq!(b.update(2.99, true), None);
        assert_eq!(b.update(3.0, true), Some(3));
        assert_eq!(b.update(3.5, true), None);
        assert_eq!(b.update(6.2, true), Some(6)); // a jump forward fires once
        assert_eq!(b.update(5.0, true), None); // backwards re-arms
        assert_eq!(b.update(6.0, true), Some(6));
        assert_eq!(b.update(7.0, false), None); // stopped
        assert_eq!(b.update(8.1, true), None); // restart arms, doesn't fire stale
        assert_eq!(b.update(9.0, true), Some(9));
        assert_eq!(onbeat(9).args, [Value::Int32(9)]);
        assert_eq!(onbeat(9).address, "/tussen/clock/onbeat");
    }

    #[test]
    fn the_hub_wakes_for_the_beat_when_it_comes_before_the_tick() {
        let s = session(120.0, 0, None);
        // Beat 1 is at 500 ms. At 490 ms with a tick due at 506.7 ms, wake for the beat.
        assert_eq!(next_wake(&s, 490_000, 506_667), 500_001);
        assert_eq!(next_wake(&s, 400_000, 416_667), 416_667);
        // Stopped: only ticks.
        assert_eq!(next_wake(&session(120.0, 0, Some(false)), 490_000, 506_667), 506_667);
        // And at the wake the beat has passed: onbeat fires on time.
        assert!(s.beat_at(500_001) >= 1.0);
    }

    #[test]
    fn the_hub_never_founds_a_session_by_itself() {
        let c = join_config(&LinkConfig { tempo: 97.5, quantum: 3.0 });
        assert_eq!(c.found_after, None, "it waits, and joins whatever session it hears");
    }

    /// Waiting: the private timeline is a beat like any session's, at
    /// `[link] tempo` from beat 0, and console commands move it — tempo
    /// without a jump in the beat, start/stop as the transport.
    #[test]
    fn while_waiting_the_hub_keeps_its_own_beat_and_the_console_drives_it() {
        let mut p = private(120.0, 0.0, 5_000_000);
        assert_eq!(sample(&p, 6_000_000).unwrap(), Sample { bpm: 120.0, beat: 2.0, phase: 0.0, running: true });
        apply_private(&mut p, LinkCmd::Tempo(60.0), 6_000_000);
        let s = sample(&p, 7_000_000).unwrap();
        assert!((s.bpm - 60.0).abs() < 1e-3 && (s.beat - 3.0).abs() < 1e-3, "{s:?}");
        apply_private(&mut p, LinkCmd::Playing(false), 7_000_000);
        assert!(!sample(&p, 7_500_000).unwrap().running);
        assert_eq!(next_wake(&p, 7_500_000, 7_516_667), 7_516_667, "stopped: only ticks");
        apply_private(&mut p, LinkCmd::Start, 7_500_000);
        assert!(!sample(&p, 7_500_000).unwrap().running, "Start is not a timeline change");
    }

    /// The rule for going back: a joined session with nobody else in it
    /// is left; a session started from the console never is; and the
    /// beat carries on from the session's when the hub leaves.
    #[test]
    fn the_hub_waits_again_only_when_a_joined_session_empties() {
        let mut s = session(126.0, 0, Some(true));
        assert_eq!(Mode::of(&s, false), Mode::Joined);
        assert!(!should_leave(&s, false), "a peer is still there");
        s.peers = 0;
        assert!(should_leave(&s, false), "everyone else has gone");
        assert!(!should_leave(&s, true), "a started session stays");
        assert_eq!(Mode::of(&s, true), Mode::Started);
        let mut waiting = s;
        waiting.status = Status::Searching;
        assert_eq!(Mode::of(&waiting, true), Mode::Waiting);
        assert!(!should_leave(&waiting, false), "nothing to leave");
        let carried = carry_on(&s);
        for t in [0, 1_000_000, 9_000_000] {
            assert_eq!(sample(&carried, t), sample(&s, t), "no jump on leaving");
        }
        assert_eq!((carried.session, carried.peers), (None, 0));
    }

    #[test]
    fn a_dj_joining_link_keeps_their_tempo() {
        // The djay scenario: a peer
        // arrives playing at 98 BPM — a session merge, from the hub's
        // side a new SessionId at a tempo that isn't `[link] tempo`. The
        // hub must not emit LinkCmd::Tempo. Structurally it cannot: the
        // policy surface is `clamp` (console commands) and `join_config`
        // (founding), and neither takes a SessionState — the state-driven
        // proposal path (the old Leader::on_state) is deleted, and
        // `propose` has exactly one call site, the console channel. What
        // a session change CAN drive is the published signals, which
        // follow the session's own tempo:
        let merged = session(98.0, 0, Some(true));
        // (to the wire's µs-per-beat rounding)
        assert!((sample(&merged, 0).unwrap().bpm - 98.0).abs() < 1e-3);
        // …and nothing else: there is no API through which this state
        // could become a proposal.
    }

    #[test]
    fn a_console_command_asserts_once_and_is_clamped() {
        // Console-set still proposes (passed on, clamped)…
        assert_eq!(clamp(LinkCmd::Tempo(5000.0)), LinkCmd::Tempo(999.0));
        assert_eq!(clamp(LinkCmd::Tempo(128.0)), LinkCmd::Tempo(128.0));
        assert_eq!(clamp(LinkCmd::Playing(false)), LinkCmd::Playing(false));
        // …once: nothing holds the value to re-assert at a later merge.
        // Set-then-second-merge semantics, pinned: after the console sets
        // 128, a merge into a 110 session leaves 110 standing — the
        // proposal fired when the human acted, and the hub has gone back
        // to following (no Leader state exists to disagree with the
        // session; `clamp` is a pure function).
        let after_merge = session(110.0, 0, Some(true));
        assert!((sample(&after_merge, 0).unwrap().bpm - 110.0).abs() < 1e-3);
    }

    fn peer(node: &[u8; 8], session: &[u8; 8], in_session: bool, from: &[&str], heard: Micros) -> PeerInfo {
        PeerInfo {
            node: NodeId(*node),
            session: NodeId(*session),
            in_session,
            timeline: Timeline::at(98.0, 0.0, 0),
            start_stop: None,
            paths: from
                .iter()
                .map(|ip| simply_link::PeerPath {
                    from: ip.parse().unwrap(),
                    endpoint: format!("{ip}:20808").parse().unwrap(),
                })
                .collect(),
            heard,
        }
    }

    /// The peer list: in-session first; a peer on this machine (over
    /// loopback, or from one of our own addresses) is `local` and never
    /// looked up; one across the room shows its LAN address and its name;
    /// the age is from the last time it was heard.
    #[test]
    fn the_console_lists_every_peer_and_where_it_is() {
        let own: Vec<IpAddr> = vec!["127.0.0.1".parse().unwrap(), "192.0.2.10".parse().unwrap()];
        let asked = std::cell::RefCell::new(Vec::new());
        let name = |ip: IpAddr| {
            asked.borrow_mut().push(ip);
            (ip.to_string() == "192.0.2.21").then(|| "laptop.local".to_owned())
        };
        let mut stranger = peer(b"stranger", b"otherses", false, &["192.0.2.33"], 9_000_000);
        stranger.start_stop = Some(StartStop { playing: false, beats: 0, timestamp: 0 });
        let peers = [
            stranger,
            peer(b"laptop!!", b"session1", true, &["192.0.2.21"], 9_750_000),
            // On this machine: heard over loopback and over the LAN.
            peer(b"djay-app", b"session1", true, &["127.0.0.1", "192.0.2.10"], 10_000_000),
            peer(b"own-lan!", b"session1", true, &["192.0.2.10"], 10_000_000),
        ];
        let rows = peer_rows(&peers, 10_000_000, &own, name);
        let summary: Vec<_> = rows
            .iter()
            .map(|r| (r.node.as_str(), r.in_session, r.host.as_deref(), r.hostname.as_deref(), r.local, r.age_ms))
            .collect();
        assert_eq!(
            summary,
            [
                ("djay-app", true, Some("192.0.2.10"), None, true, 0),
                ("own-lan!", true, Some("192.0.2.10"), None, true, 0),
                ("laptop!!", true, Some("192.0.2.21"), Some("laptop.local"), false, 250),
                ("stranger", false, Some("192.0.2.33"), None, false, 1000),
            ]
        );
        let asked: Vec<String> = asked.into_inner().iter().map(IpAddr::to_string).collect();
        assert_eq!(asked, ["192.0.2.33", "192.0.2.21"], "only remote hosts are looked up");
        assert!((rows[0].bpm - 98.0).abs() < 1e-3);
        assert_eq!((rows[0].playing, rows[3].playing), (None, Some(false)));
        // Only loopback: it is that address, and local. No path: no host.
        let lo = peer_rows(&[peer(b"loopback", b"session1", true, &["127.0.0.1"], 0)], 0, &[], |_| None);
        assert_eq!((lo[0].host.as_deref(), lo[0].local), (Some("127.0.0.1"), true));
        let none = peer_rows(&[peer(b"nopath!!", b"session1", true, &[], 0)], 0, &[], |_| None);
        assert_eq!((none[0].host.as_deref(), none[0].local), (None, false));
    }

    #[test]
    fn the_link_section_parses_and_defaults() {
        let c: crate::config::Config = toml::from_str("[link]\ntempo = 97.5\nquantum = 3\n").unwrap();
        assert_eq!(c.link, LinkConfig { tempo: 97.5, quantum: 3.0 });
        let c: crate::config::Config = toml::from_str("").unwrap();
        assert_eq!(c.link, LinkConfig::default());
    }
}
