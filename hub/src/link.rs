//! The clock generator: tussen-hub joins Ableton Link and
//! publishes the session's beat under `/tussen/clock/…`.
//!
//! **Founding and following.** Link has no master. The hub founds a
//! session at `[link] tempo` when it hears none for a second
//! ([`FOUND_AFTER_US`], via `simply_link`'s `found_after`) so the rig has
//! a beat at a sane tempo before any player arrives — and from then on it
//! **follows**. A console command ([`LinkCmd`]) proposes tempo or
//! start/stop once, at the moment the human acts; the hub keeps no stance
//! of its own to re-assert, so a session merge or a joining peer never
//! hears the hub's old tempo. [`propose`]'s one call site is the console
//! channel: session changes structurally cannot produce proposals
//! (the earlier design re-proposed the
//! configured tempo at every merge, and a DJ joining Link had their
//! playing track slowed to the hub's 120).
//!
//! **Meeting other sessions** is Link's own rule, so the hub and every
//! other Link app pick the same winner: the older session. A DJ who had
//! Link on first keeps their session and tempo, and the hub joins it; a DJ
//! who switches Link on after the hub founded joins the hub's session at
//! its tempo — what any Link app does when it joins one already running.
//! Either way the hub follows every tempo change from then on. The log
//! says which session the hub is in and every tempo it follows.
//!
//! **Signals** (MIDI class, like `/tussen/midi/…`): `bpm`, `beat` (the
//! session's beat), `phase` (`frac(beat)`) and `running` (0/1), all `f`,
//! at 60 Hz; `onbeat` (`i`, the beat index) when a whole beat passes,
//! sent at the beat's own time rather than on the next 60 Hz tick.
//! `running` is the session's start/stop state; a session nobody starts
//! or stops runs. Nothing is published until the hub is in a session.
//!
//! `beat` goes out as a 32-bit float: a session the hub founds starts at
//! beat 0, so it holds a millisecond's precision for over 9 hours at
//! 120 BPM.
//!
//! The pure parts — [`sample`], [`Beats`], [`next_wake`] — take a
//! [`SessionState`] and a host time, so the tests drive them from a fake
//! Link timeline with no sockets.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use serde::Deserialize;
use simply_link::{Link, Micros, SessionState};
use simply_osc::{Message, Value};

use crate::console::{self, Event, Tap};
use crate::generators::Generator;

/// Everything published lives under this prefix.
pub const PREFIX: &str = "/tussen/clock/";
/// µs between two publishes of the continuous signals (60 Hz).
pub const PERIOD_US: Micros = 16_667;
/// Found a session after hearing none for this long: long enough to hear
/// an existing session's announcements (every 250 ms), short enough that
/// a lone hub has a beat within a second.
const FOUND_AFTER_US: Micros = 1_000_000;
/// How often the console page gets the session state (~4 Hz).
const CONSOLE_PERIOD: Duration = Duration::from_millis(250);

/// `[link]`.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(default)]
pub struct LinkConfig {
    /// The tempo the hub founds a session at when it hears none. Never
    /// proposed at an existing session: a player's tempo wins.
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
}

/// What the console page draws the beat grid from (the `link` frame).
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct LinkStatus {
    /// Hub clock µs ([`console::now_us`]).
    pub ts: u64,
    /// The session's beat at `ts`.
    pub beat: f64,
    pub bpm: f64,
    pub quantum: f64,
    pub playing: bool,
    pub peers: usize,
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
        LinkCmd::Playing(_) => cmd,
    }
}

/// The `simply_link` join config: found a session at `[link] tempo` after
/// hearing none for [`FOUND_AFTER_US`]. `bpm` is only a founding tempo;
/// joining an existing session adopts that session's.
pub fn join_config(config: &LinkConfig) -> simply_link::Config {
    simply_link::Config {
        bpm: config.tempo,
        found_after: Some(FOUND_AFTER_US),
        ..simply_link::Config::default()
    }
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

/// The generator: joins Link in `start`, then runs its own thread.
pub struct LinkGenerator {
    config: LinkConfig,
    cmds: Receiver<LinkCmd>,
    tap: Option<Arc<Tap>>,
}

impl LinkGenerator {
    /// `cmds` and `tap` come from the console, if there is one.
    pub fn new(config: LinkConfig, cmds: Receiver<LinkCmd>, tap: Option<Arc<Tap>>) -> LinkGenerator {
        LinkGenerator { config, cmds, tap }
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
            "link: joined the Link group (founds at {} BPM if alone, follows any session), quantum {}",
            config.tempo,
            config.quantum
        );
        let cmds = self.cmds.clone();
        let tap = self.tap.clone();
        std::thread::Builder::new()
            .name("tussen-hub link".into())
            .spawn(move || run_link(&link, &config, &cmds, tap.as_deref(), &out))?;
        Ok(())
    }
}

fn propose(link: &Link, cmd: LinkCmd) {
    let now = link.clock().now();
    match cmd {
        LinkCmd::Tempo(bpm) => {
            log::info!("link: tempo → {bpm:.2} BPM");
            link.propose_tempo(bpm, now);
        }
        LinkCmd::Playing(p) => {
            log::info!("link: {}", if p { "start" } else { "stop" });
            link.propose_start_stop(p, now);
        }
    }
}

fn run_link(link: &Link, config: &LinkConfig, cmds: &Receiver<LinkCmd>, tap: Option<&Tap>, out: &Sender<Message>) {
    let clock = link.clock();
    let mut beats = Beats::default();
    let mut next_tick = clock.now();
    let mut last_console = Instant::now() - CONSOLE_PERIOD;
    let mut last_status: Option<(String, usize)> = None;
    let mut logged_bpm = (0.0, Instant::now());
    loop {
        let mut changed = false;
        // The only place the hub proposes anything: a human's console
        // command. Session changes never do (module doc).
        while let Ok(cmd) = cmds.try_recv() {
            propose(link, clamp(cmd));
            changed = true;
        }
        let state = link.state();
        let session = state.session.map_or_else(|| "-".into(), |s| s.to_string());
        let status = (format!("{:?} in session {session}", state.status), state.peers);
        if last_status.as_ref() != Some(&status) {
            log::info!("link: {} at {:.2} BPM, {} peer(s)", status.0, state.tempo(), status.1);
            last_status = Some(status);
            logged_bpm = (state.tempo(), Instant::now());
        }
        // A tempo the hub follows, at most twice a second while a DJ rides
        // the fader.
        if state.joined()
            && (state.tempo() - logged_bpm.0).abs() >= 0.01
            && logged_bpm.1.elapsed() >= Duration::from_millis(500)
        {
            log::info!("link: following {:.2} BPM", state.tempo());
            logged_bpm = (state.tempo(), Instant::now());
        }

        let now = clock.now();
        if let Some(s) = sample(&state, now) {
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
                        peers: state.peers,
                    }));
                }
            }
        }
        while next_tick <= now {
            next_tick += PERIOD_US;
        }
        let wake = next_wake(&state, now, next_tick);
        std::thread::sleep(clock.instant(wake).saturating_duration_since(Instant::now()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use simply_link::timeline::GhostXForm;
    use simply_link::{NodeId, StartStop, Status, Timeline};

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
    fn the_hub_founds_at_the_configured_tempo_when_alone() {
        let c = join_config(&LinkConfig { tempo: 97.5, quantum: 3.0 });
        assert_eq!(c.bpm, 97.5);
        // Long enough to hear an existing session before founding one.
        assert_eq!(c.found_after, Some(FOUND_AFTER_US));
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

    #[test]
    fn the_link_section_parses_and_defaults() {
        let c: crate::config::Config = toml::from_str("[link]\ntempo = 97.5\nquantum = 3\n").unwrap();
        assert_eq!(c.link, LinkConfig { tempo: 97.5, quantum: 3.0 });
        let c: crate::config::Config = toml::from_str("").unwrap();
        assert_eq!(c.link, LinkConfig::default());
    }
}
