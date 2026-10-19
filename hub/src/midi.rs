//! The MIDI generator: the pad and knob controllers as
//! signals under `/tussen/midi/…`.
//!
//! `simply-midi` opens the input ports (`[midi] inputs`,
//! substring match on the port name) and decodes the bytes; the midir
//! callback pushes the decoded `Copy` event into a bounded channel —
//! no allocation, no lock — and this module's own thread does the rest:
//!
//! * **state, merged across channels and ports** ([`Engine`]): the last
//!   value per CC, the held notes and their poly pressure, bend,
//!   pressure, program;
//! * **publish on change**: `cc/{n}` (value/127), `note/{n}`
//!   (velocity/127, 0 = off), `note/{n}/pressure` (poly aftertouch,
//!   value/127, 0 on release), `bend` (−1..1, 0 at rest), `pressure`
//!   (channel aftertouch, 0..1), `program` (int);
//! * **refresh**: every `refresh_ms` resend every CC ever received and
//!   every held note with its pressure, so receivers self-heal after
//!   packet loss;
//! * **re-scan every 2 s**: ports appearing or disappearing
//!   are picked up without a restart;
//! * `--fake-midi`: a deterministic random controller ([`FakeMidi`]) for
//!   testing the whole path without hardware.
//!
//! Sysex and realtime bytes never reach us (`simply-midi` sets
//! `Ignore::All` and its decoder rejects system messages).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Sender, TrySendError};
use serde::Deserialize;
use simply_midi::MidiMessage;
use simply_osc::{Message, Value};

use crate::generators::Generator;

/// Everything published lives under this prefix.
pub const PREFIX: &str = "/tussen/midi/";
/// How often the port list is re-checked.
pub const RESCAN_MS: u64 = 2000;
/// The bounded event channel's capacity: the midir callback drops (and
/// counts) rather than blocks or allocates when the hub thread stalls.
const EVENT_CAPACITY: usize = 1024;

/// The `[midi]` config section.
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct MidiConfig {
    /// Port-name filters, case-insensitive substring match; `"*"` (the
    /// default) matches every input port.
    pub inputs: Vec<String>,
    /// How often the full CC + held-note state is resent.
    pub refresh_ms: u64,
}

impl Default for MidiConfig {
    fn default() -> Self {
        MidiConfig { inputs: vec!["*".into()], refresh_ms: 500 }
    }
}

/// Does any config filter pick this port? `"*"` matches everything;
/// anything else is `simply_midi::name_matches` (the one port matcher:
/// a case-insensitive substring).
pub fn port_matches(filters: &[String], name: &str) -> bool {
    filters.iter().any(|f| f == "*" || simply_midi::name_matches(name, f))
}

/// The merged controller state and its publish rules. Pure — it is fed
/// events and a millisecond clock, and returns the messages to publish,
/// so the tests need no threads, ports or real time.
pub struct Engine {
    /// Every CC ever received (merged across channels and ports): the
    /// refresh set. BTreeMap so the resend order is stable.
    ccs: BTreeMap<u8, u8>,
    /// The held notes, key → velocity as struck.
    held: BTreeMap<u8, u8>,
    /// Poly aftertouch per key, last value; cleared on that note's release.
    touch: BTreeMap<u8, u8>,
    bend: Option<u16>,
    pressure: Option<u8>,
    program: Option<u8>,
    refresh_ms: u64,
    next_refresh: u64,
}

fn float(path: &str, v: f32) -> Message {
    Message::new(format!("{PREFIX}{path}")).arg(Value::Float32(v))
}

/// Pitch bend 0..=16383 (8192 at rest) → −1..1 with the endpoints exact
/// and 0 exactly at rest.
pub fn bend_value(raw: u16) -> f32 {
    let centred = f32::from(raw) - 8192.0;
    if centred >= 0.0 { centred / 8191.0 } else { centred / 8192.0 }
}

impl Engine {
    pub fn new(refresh_ms: u64, now_ms: u64) -> Engine {
        Engine {
            ccs: BTreeMap::new(),
            held: BTreeMap::new(),
            touch: BTreeMap::new(),
            bend: None,
            pressure: None,
            program: None,
            refresh_ms: refresh_ms.max(1),
            next_refresh: now_ms + refresh_ms.max(1),
        }
    }

    /// Apply one event; the returned messages are what changed.
    pub fn apply(&mut self, event: MidiMessage) -> Vec<Message> {
        let mut out = Vec::new();
        match event {
            MidiMessage::ControlChange { cc, value, .. } => {
                if self.ccs.insert(cc, value) != Some(value) {
                    out.push(float(&format!("cc/{cc}"), f32::from(value) / 127.0));
                }
            }
            // A note strike or release is an event: always published,
            // even at the same velocity as the last strike.
            MidiMessage::NoteOn { key, velocity, .. } => {
                self.held.insert(key, velocity);
                out.push(float(&format!("note/{key}"), f32::from(velocity) / 127.0));
            }
            MidiMessage::NoteOff { key, .. } => {
                self.held.remove(&key);
                out.push(float(&format!("note/{key}"), 0.0));
                if self.touch.remove(&key).is_some_and(|v| v != 0) {
                    out.push(float(&format!("note/{key}/pressure"), 0.0));
                }
            }
            MidiMessage::PitchBend { value, .. } => {
                if self.bend != Some(value) {
                    self.bend = Some(value);
                    out.push(float("bend", bend_value(value)));
                }
            }
            MidiMessage::ChannelPressure { value, .. } => {
                if self.pressure != Some(value) {
                    self.pressure = Some(value);
                    out.push(float("pressure", f32::from(value) / 127.0));
                }
            }
            // Poly aftertouch: per key, merged across channels like notes.
            MidiMessage::PolyPressure { key, value, .. } => {
                if self.touch.insert(key, value) != Some(value) {
                    out.push(float(&format!("note/{key}/pressure"), f32::from(value) / 127.0));
                }
            }
            MidiMessage::ProgramChange { program, .. } => {
                if self.program != Some(program) {
                    self.program = Some(program);
                    out.push(
                        Message::new(format!("{PREFIX}program"))
                            .arg(Value::Int32(i32::from(program))),
                    );
                }
            }
        }
        out
    }

    /// The refresh clock: at each `refresh_ms` boundary, every CC ever
    /// received and every held note with its pressure (the
    /// self-heal set).
    pub fn tick(&mut self, now_ms: u64) -> Vec<Message> {
        if now_ms < self.next_refresh {
            return Vec::new();
        }
        // One refresh per call, scheduled from now: after a stall we
        // resend once rather than replaying missed boundaries.
        self.next_refresh = now_ms + self.refresh_ms;
        let mut out = Vec::with_capacity(self.ccs.len() + self.held.len());
        for (&cc, &value) in &self.ccs {
            out.push(float(&format!("cc/{cc}"), f32::from(value) / 127.0));
        }
        for (&key, &velocity) in &self.held {
            out.push(float(&format!("note/{key}"), f32::from(velocity) / 127.0));
            if let Some(&value) = self.touch.get(&key) {
                out.push(float(&format!("note/{key}/pressure"), f32::from(value) / 127.0));
            }
        }
        out
    }
}

/// A deterministic fake controller: four knobs
/// random-walking, notes striking and releasing, poly pressure on held
/// notes, the odd bend, pressure
/// and program change. Same seed, same stream.
pub struct FakeMidi {
    rng: crate::prng::XorShift,
    /// Current value of each fake knob, walked smoothly.
    knobs: [(u8, u8); 4],
    /// Keys currently down, so releases match strikes.
    down: Vec<u8>,
}

/// The CCs the fake controller moves — 1 (mod), 7 (volume), 10 (pan),
/// 74 (cutoff): plausible on any real controller.
pub const FAKE_CCS: [u8; 4] = [1, 7, 10, 74];
/// How often the fake controller emits a batch.
pub const FAKE_TICK_MS: u64 = 50;

impl FakeMidi {
    pub fn new(seed: u64) -> FakeMidi {
        FakeMidi {
            rng: crate::prng::XorShift::new(seed),
            knobs: [(FAKE_CCS[0], 64), (FAKE_CCS[1], 64), (FAKE_CCS[2], 64), (FAKE_CCS[3], 64)],
            down: Vec::new(),
        }
    }

    fn next(&mut self) -> u64 {
        self.rng.next()
    }

    /// The events for one 50 ms tick: every knob takes a ±3 step, and
    /// about every fourth tick a note strikes or releases; bend,
    /// poly pressure (on a held note), pressure and program appear
    /// occasionally.
    pub fn tick(&mut self) -> Vec<MidiMessage> {
        let mut out = Vec::new();
        for i in 0..self.knobs.len() {
            let step = (self.next() % 7) as i16 - 3;
            let (cc, value) = self.knobs[i];
            let value = (i16::from(value) + step).clamp(0, 127) as u8;
            self.knobs[i] = (cc, value);
            out.push(MidiMessage::ControlChange { channel: 0, cc, value });
        }
        if self.next() % 4 == 0 {
            // Release half the time once something is held, else strike.
            if !self.down.is_empty() && self.next() % 2 == 0 {
                let index = self.next() as usize % self.down.len();
                let key = self.down.swap_remove(index);
                out.push(MidiMessage::NoteOff { channel: 0, key });
            } else {
                let key = 36 + (self.next() % 49) as u8; // C2..C6
                let velocity = 1 + (self.next() % 127) as u8;
                if !self.down.contains(&key) {
                    self.down.push(key);
                    out.push(MidiMessage::NoteOn { channel: 0, key, velocity });
                }
            }
        }
        if self.next() % 8 == 0 {
            out.push(MidiMessage::PitchBend {
                channel: 0,
                value: (self.next() % 16384) as u16,
            });
        }
        if self.next() % 8 == 0 {
            out.push(MidiMessage::ChannelPressure {
                channel: 0,
                value: (self.next() % 128) as u8,
            });
        }
        if !self.down.is_empty() && self.next() % 4 == 0 {
            let index = self.next() as usize % self.down.len();
            let key = self.down[index];
            out.push(MidiMessage::PolyPressure {
                channel: 0,
                key,
                value: (self.next() % 128) as u8,
            });
        }
        if self.next() % 64 == 0 {
            out.push(MidiMessage::ProgramChange { channel: 0, program: (self.next() % 16) as u8 });
        }
        out
    }
}

/// The ports to open, from the full port list and the config filters:
/// the matched names, minus any name another selected name is a
/// substring of — `simply_midi::input::open_inputs` opens *every* port
/// whose name contains its selector, so a selector that is a substring
/// of another selected name would open that port twice.
fn selectors(port_names: &[String], filters: &[String]) -> Option<Vec<String>> {
    if filters.iter().any(|f| f == "*") {
        return None; // open everything with one call
    }
    let mut matched: Vec<&String> =
        port_names.iter().filter(|n| port_matches(filters, n)).collect();
    matched.sort();
    matched.dedup();
    let selected: Vec<String> = matched
        .iter()
        .filter(|name| {
            !matched
                .iter()
                .any(|other| other != *name && simply_midi::name_matches(name, other))
        })
        .map(|n| (*n).clone())
        .collect();
    Some(selected)
}

/// The MIDI generator: register in `cmd::run`'s `Vec<Box<dyn Generator>>`.
pub struct MidiGenerator {
    config: MidiConfig,
    /// `Some(seed)` = `--fake-midi`: no ports, a [`FakeMidi`] instead.
    fake: Option<u64>,
}

impl MidiGenerator {
    pub fn new(config: MidiConfig, fake: Option<u64>) -> MidiGenerator {
        MidiGenerator { config, fake }
    }
}

impl Generator for MidiGenerator {
    fn name(&self) -> &'static str {
        "midi"
    }

    fn start(&mut self, out: Sender<Message>) -> anyhow::Result<()> {
        let config = self.config.clone();
        let fake = self.fake;
        // Probe the ports now so a machine with no MIDI client fails
        // start() visibly (logged; the hub keeps running without us).
        if fake.is_none() {
            simply_midi::input::input_ports("simply-tussen")
                .map_err(|e| anyhow::anyhow!("MIDI unavailable: {e}"))?;
        }
        std::thread::Builder::new()
            .name("simply-tussen midi".into())
            .spawn(move || run_midi(&config, fake, &out))?;
        Ok(())
    }
}

/// The generator thread: drain the event channel into the engine,
/// publish what changed, refresh on the clock, re-scan ports every 2 s.
fn run_midi(config: &MidiConfig, fake: Option<u64>, out: &Sender<Message>) {
    let started = Instant::now();
    let now_ms = || started.elapsed().as_millis() as u64;
    let mut engine = Engine::new(config.refresh_ms, 0);

    // The bounded channel between the midir callbacks and this thread.
    // `try_send` of a `Copy` value: no allocation, no block; overflow is
    // a count, reported with the port log line at the next re-scan.
    let (tx, rx) = crossbeam_channel::bounded::<MidiMessage>(EVENT_CAPACITY);
    let dropped = Arc::new(AtomicU64::new(0));

    let mut fake_source = fake.map(FakeMidi::new);
    let mut next_fake = 0u64;

    // The open connections (dropping closes them) and the port names
    // they were opened on, to detect appear/disappear at each re-scan.
    let mut listeners: Vec<simply_midi::input::MidiListener> = Vec::new();
    let mut open_names: Vec<String> = Vec::new();
    let mut next_rescan = 0u64;
    let mut dropped_reported = 0u64;

    loop {
        // One event (or a 20 ms tick), then everything queued.
        match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(event) => {
                for m in engine.apply(event) {
                    let _ = out.send(m);
                }
                while let Ok(event) = rx.try_recv() {
                    for m in engine.apply(event) {
                        let _ = out.send(m);
                    }
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
        }

        let now = now_ms();

        if let Some(fake) = &mut fake_source {
            while next_fake <= now {
                next_fake += FAKE_TICK_MS;
                for event in fake.tick() {
                    for m in engine.apply(event) {
                        let _ = out.send(m);
                    }
                }
            }
        } else if now >= next_rescan {
            next_rescan = now + RESCAN_MS;
            rescan(config, &tx, &dropped, &mut listeners, &mut open_names);
            let d = dropped.load(Ordering::Relaxed);
            if d > dropped_reported {
                log::warn!("midi: {} event(s) dropped on a full channel", d - dropped_reported);
                dropped_reported = d;
            }
        }

        // The refresh: if the router is gone, stop.
        for m in engine.tick(now) {
            if out.send(m).is_err() {
                return;
            }
        }
    }
}

/// Compare the matched port set with what is open; re-open on any
/// difference (a port appeared, disappeared, or was renamed).
fn rescan(
    config: &MidiConfig,
    tx: &Sender<MidiMessage>,
    dropped: &Arc<AtomicU64>,
    listeners: &mut Vec<simply_midi::input::MidiListener>,
    open_names: &mut Vec<String>,
) {
    let ports = match simply_midi::input::input_ports("simply-tussen") {
        Ok(ports) => ports,
        Err(e) => {
            // The whole MIDI system went away; keep what we hold and retry.
            log::warn!("midi: port scan failed: {e}");
            return;
        }
    };
    let mut matched: Vec<String> = ports
        .iter()
        .filter(|p| port_matches(&config.inputs, &p.name))
        .map(|p| p.name.clone())
        .collect();
    matched.sort();
    if matched == *open_names {
        return;
    }
    for gone in open_names.iter().filter(|n| !matched.contains(n)) {
        log::info!("midi: port gone: {gone}");
    }
    // Nothing open yet: the `listening on` line below names them all.
    let first = open_names.is_empty();
    for new in matched.iter().filter(|n| !first && !open_names.contains(n)) {
        log::info!("midi: port found: {new}");
    }

    // Re-open from scratch: connections are cheap and state lives in the
    // engine, not the ports.
    listeners.clear();
    let names: Vec<String> = ports.iter().map(|p| p.name.clone()).collect();
    let selects = selectors(&names, &config.inputs);
    let handler = |tx: Sender<MidiMessage>, dropped: Arc<AtomicU64>| {
        move |message: MidiMessage| {
            // The midir callback: one Copy value into a bounded channel.
            if let Err(TrySendError::Full(_)) = tx.try_send(message) {
                dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    };
    let mut opened = Vec::new();
    let select_list: Vec<Option<String>> = match selects {
        None => vec![None],
        Some(list) => list.into_iter().map(Some).collect(),
    };
    for select in select_list {
        match simply_midi::input::open_inputs("simply-tussen", select.as_deref(), |_port| {
            handler(tx.clone(), Arc::clone(dropped))
        }) {
            Ok(listener) => {
                for f in &listener.failures {
                    log::warn!("midi: {f}");
                }
                opened.extend(listener.devices.iter().cloned());
                listeners.push(listener);
            }
            Err(e) => log::warn!("midi: opening {select:?}: {e}"),
        }
    }
    opened.sort();
    if !opened.is_empty() {
        let level = if first { log::Level::Info } else { log::Level::Debug };
        log::log!(level, "midi: listening on {}", opened.join(", "));
    }
    *open_names = matched;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cc(channel: u8, cc: u8, value: u8) -> MidiMessage {
        MidiMessage::ControlChange { channel, cc, value }
    }
    fn note_on(channel: u8, key: u8, velocity: u8) -> MidiMessage {
        MidiMessage::NoteOn { channel, key, velocity }
    }
    fn note_off(channel: u8, key: u8) -> MidiMessage {
        MidiMessage::NoteOff { channel, key }
    }

    fn addresses(msgs: &[Message]) -> Vec<&str> {
        msgs.iter().map(|m| m.address.as_str()).collect()
    }
    fn value_of(msgs: &[Message], address: &str) -> Option<f32> {
        msgs.iter().find(|m| m.address == address).and_then(|m| m.args.first()?.as_float())
    }

    #[test]
    fn cc_scales_to_unit_and_publishes_only_on_change() {
        let mut e = Engine::new(500, 0);
        let msgs = e.apply(cc(0, 74, 127));
        assert_eq!(addresses(&msgs), ["/tussen/midi/cc/74"]);
        assert_eq!(value_of(&msgs, "/tussen/midi/cc/74"), Some(1.0));
        // The same value again — merged state unchanged, nothing published.
        assert!(e.apply(cc(0, 74, 127)).is_empty());
        // A new value publishes.
        let msgs = e.apply(cc(0, 74, 0));
        assert_eq!(value_of(&msgs, "/tussen/midi/cc/74"), Some(0.0));
    }

    #[test]
    fn state_merges_across_channels_and_sources() {
        // Two channels (two ports look identical here: the engine is fed
        // one merged stream) write the same CC: one address, last wins.
        let mut e = Engine::new(500, 0);
        e.apply(cc(0, 7, 10));
        let msgs = e.apply(cc(5, 7, 90));
        assert_eq!(value_of(&msgs, "/tussen/midi/cc/7"), Some(90.0 / 127.0));
        let refresh = e.tick(500);
        assert_eq!(addresses(&refresh), ["/tussen/midi/cc/7"]);
        assert_eq!(value_of(&refresh, "/tussen/midi/cc/7"), Some(90.0 / 127.0));
    }

    #[test]
    fn notes_publish_velocity_and_zero_on_release() {
        let mut e = Engine::new(500, 0);
        let msgs = e.apply(note_on(0, 60, 127));
        assert_eq!(value_of(&msgs, "/tussen/midi/note/60"), Some(1.0));
        // A re-strike at the same velocity is an event: published again.
        assert_eq!(e.apply(note_on(0, 60, 127)).len(), 1);
        let msgs = e.apply(note_off(0, 60));
        assert_eq!(value_of(&msgs, "/tussen/midi/note/60"), Some(0.0));
    }

    #[test]
    fn bend_spans_minus_one_to_one_with_zero_at_rest() {
        assert_eq!(bend_value(0), -1.0);
        assert_eq!(bend_value(8192), 0.0);
        assert_eq!(bend_value(16383), 1.0);
        let mut e = Engine::new(500, 0);
        let msgs = e.apply(MidiMessage::PitchBend { channel: 0, value: 0 });
        assert_eq!(value_of(&msgs, "/tussen/midi/bend"), Some(-1.0));
        // Same raw value: no change, no publish.
        assert!(e
            .apply(MidiMessage::PitchBend { channel: 0, value: 0 })
            .is_empty());
    }

    #[test]
    fn pressure_and_program_publish_on_change() {
        let mut e = Engine::new(500, 0);
        let msgs =
            e.apply(MidiMessage::ChannelPressure { channel: 0, value: 127 });
        assert_eq!(value_of(&msgs, "/tussen/midi/pressure"), Some(1.0));
        let msgs = e.apply(MidiMessage::ProgramChange { channel: 0, program: 5 });
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].address, "/tussen/midi/program");
        assert_eq!(msgs[0].args, [Value::Int32(5)]);
        assert!(e.apply(MidiMessage::ProgramChange { channel: 0, program: 5 }).is_empty());
    }

    fn poly(channel: u8, key: u8, value: u8) -> MidiMessage {
        MidiMessage::PolyPressure { channel, key, value }
    }

    #[test]
    fn poly_pressure_publishes_per_note_on_change_and_zeroes_on_release() {
        let mut e = Engine::new(500, 0);
        e.apply(note_on(0, 60, 100));
        let msgs = e.apply(poly(0, 60, 127));
        assert_eq!(addresses(&msgs), ["/tussen/midi/note/60/pressure"]);
        assert_eq!(value_of(&msgs, "/tussen/midi/note/60/pressure"), Some(1.0));
        // Same value (any channel — merged): nothing.
        assert!(e.apply(poly(3, 60, 127)).is_empty());
        let msgs = e.apply(poly(3, 60, 64));
        assert_eq!(value_of(&msgs, "/tussen/midi/note/60/pressure"), Some(64.0 / 127.0));
        // Channel pressure stays its own signal.
        assert!(value_of(&msgs, "/tussen/midi/pressure").is_none());
        // Release (velocity-0 note-on decodes to NoteOff): note and pressure zero.
        let msgs = e.apply(note_off(0, 60));
        assert_eq!(
            addresses(&msgs),
            ["/tussen/midi/note/60", "/tussen/midi/note/60/pressure"]
        );
        assert_eq!(value_of(&msgs, "/tussen/midi/note/60/pressure"), Some(0.0));
        // A release with no pressure ever sent adds nothing.
        e.apply(note_on(0, 62, 100));
        assert_eq!(addresses(&e.apply(note_off(0, 62))), ["/tussen/midi/note/62"]);
    }

    #[test]
    fn refresh_carries_held_notes_pressure_not_released_ones() {
        let mut e = Engine::new(500, 0);
        e.apply(note_on(0, 60, 100));
        e.apply(note_on(0, 64, 80));
        e.apply(poly(0, 60, 50));
        e.apply(poly(0, 64, 90));
        e.apply(note_off(0, 64));
        let refresh = e.tick(500);
        assert_eq!(
            addresses(&refresh),
            ["/tussen/midi/note/60", "/tussen/midi/note/60/pressure"]
        );
        assert_eq!(value_of(&refresh, "/tussen/midi/note/60/pressure"), Some(50.0 / 127.0));
    }

    #[test]
    fn refresh_resends_every_cc_ever_and_only_held_notes() {
        let mut e = Engine::new(500, 0);
        e.apply(cc(0, 1, 11));
        e.apply(cc(0, 74, 22));
        e.apply(note_on(0, 60, 100));
        e.apply(note_on(0, 64, 80));
        e.apply(note_off(0, 60)); // released: out of the refresh set
        // Before the deadline: nothing.
        assert!(e.tick(499).is_empty());
        // At the deadline: both CCs ever received, the one held note.
        let refresh = e.tick(500);
        assert_eq!(
            addresses(&refresh),
            ["/tussen/midi/cc/1", "/tussen/midi/cc/74", "/tussen/midi/note/64"]
        );
        assert_eq!(value_of(&refresh, "/tussen/midi/note/64"), Some(80.0 / 127.0));
        // The clock re-arms from the tick that fired.
        assert!(e.tick(999).is_empty());
        assert_eq!(e.tick(1000).len(), 3);
    }

    #[test]
    fn port_filters_match_case_insensitive_substrings_and_star() {
        let star = vec!["*".to_string()];
        assert!(port_matches(&star, "anything at all"));
        let filters = vec!["push".to_string(), "IAC".to_string()];
        assert!(port_matches(&filters, "Ableton Push 2 Live Port"));
        assert!(port_matches(&filters, "iac driver bus 1"));
        assert!(!port_matches(&filters, "Launchpad Mini"));
    }

    #[test]
    fn selectors_dedup_names_that_contain_each_other() {
        let names: Vec<String> =
            ["Push 2", "Push 2 Live Port", "Launchpad Mini", "IAC Bus 1"]
                .iter()
                .map(|s| s.to_string())
                .collect();
        // "*" anywhere means one open-everything call.
        assert_eq!(selectors(&names, &["*".to_string()]), None);
        // "push" matches both Push ports, but opening by "Push 2" already
        // opens "Push 2 Live Port" (substring), so only one selector stays.
        let sel = selectors(&names, &["push".to_string(), "launch".to_string()]).unwrap();
        assert_eq!(sel, ["Launchpad Mini", "Push 2"]);
        // No match: an empty selector list, not an open-everything None.
        assert_eq!(selectors(&names, &["nope".to_string()]).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn fake_midi_is_deterministic_per_seed() {
        let stream = |seed: u64| -> Vec<MidiMessage> {
            let mut f = FakeMidi::new(seed);
            (0..100).flat_map(|_| f.tick()).collect()
        };
        assert_eq!(stream(42), stream(42), "same seed, same events");
        assert_ne!(stream(42), stream(43), "different seed, different events");
        // And the stream actually moves CCs and strikes notes.
        let events = stream(42);
        assert!(events.iter().any(|e| matches!(e, MidiMessage::NoteOn { .. })));
        assert!(events.iter().any(|e| matches!(e, MidiMessage::PolyPressure { .. })));
        assert!(
            events
                .iter()
                .any(|e| matches!(e, MidiMessage::ControlChange { .. }))
        );
    }

    #[test]
    fn fake_midi_through_the_engine_self_heals_on_refresh() {
        // The full fake path: events through the engine, then a refresh
        // carries the CC state so a late receiver catches up.
        let mut f = FakeMidi::new(7);
        let mut e = Engine::new(500, 0);
        let mut published = Vec::new();
        for _ in 0..20 {
            for event in f.tick() {
                published.extend(e.apply(event));
            }
        }
        assert!(
            published.iter().any(|m| m.address.ends_with("/pressure")
                && m.address.starts_with("/tussen/midi/note/")),
            "fake MIDI should publish poly pressure"
        );
        let refresh = e.tick(500);
        for cc in FAKE_CCS {
            assert!(
                refresh.iter().any(|m| m.address == format!("/tussen/midi/cc/{cc}")),
                "refresh should carry cc/{cc}"
            );
        }
    }

    #[test]
    fn the_default_config_is_the_spec_default() {
        let c = MidiConfig::default();
        assert_eq!(c.inputs, ["*"]);
        assert_eq!(c.refresh_ms, 500);
    }
}
