//! The gamepad generator:
//! a controller plugged into the **hub laptop** — a DualSense, say — as
//! signals under `/tussen/pad/…`, no device app involved.
//!
//! The schema is the per-device pad schema (signals.yaml), minus the
//! `{dev}` segment: a shader or preset written against pad addresses
//! works with either source. A controller plugged into a *device* still
//! publishes `/tussen/<dev>/pad/…` through that device's own app; both
//! models coexist.
//!
//! Structure mirrors the MIDI generator:
//!
//! * **[`Engine`]** — pure: fed [`PadInput`]s and a millisecond clock,
//!   returns the messages to publish, so tests need no hardware;
//! * **publish on change**: sticks as `lx,ly,rx,ry` (−1..1) *and*
//!   `n/lx…` (0..1, centre 0.5); triggers `l2,r2` (0..1) with
//!   `…/press` on the `press_on`/`press_off` hysteresis; buttons 0/1
//!   with `…/press` on the down edge; `active` 0/1;
//! * **refresh**: every `refresh_ms` the full current state is resent
//!   (just `active` 0 while nothing is connected), so receivers
//!   self-heal after packet loss, like the MIDI refresh;
//! * **gilrs** reads the controllers (macOS: IOKit on its own thread;
//!   hotplug is gilrs's own Connected/Disconnected events, no re-scan
//!   needed); its default filters give us deadzones, jitter suppression
//!   and d-pad-as-buttons;
//! * `--fake-pad`: a deterministic fake controller ([`FakePad`]) for
//!   testing the whole path without hardware.

use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use serde::Deserialize;
use simply_osc::{Message, Value};

use crate::generators::Generator;
use crate::prng::XorShift;

/// Everything published lives under this prefix. (The ownership rule
/// never collides: no device is named `pad`, as with `midi`/`clock`.)
pub const PREFIX: &str = "/tussen/pad/";

/// Stick axes, in [`PadInput::Axis`] index order.
pub const AXES: [&str; 4] = ["lx", "ly", "rx", "ry"];
/// Analog triggers, in [`PadInput::Trigger`] index order.
pub const TRIGGERS: [&str; 2] = ["l2", "r2"];
/// Digital buttons, in [`PadInput::Button`] index order — the
/// signals.yaml names (DualSense: south/east/west/north = ✕ ○ □ △,
/// start/select = options/create).
pub const BUTTONS: [&str; 14] = [
    "south", "east", "west", "north",
    "l1", "r1", "l3", "r3",
    "up", "down", "left", "right",
    "start", "select",
];

/// The `[pad]` config section.
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct PadConfig {
    /// The off switch: `false` skips the generator entirely.
    pub enabled: bool,
    /// Controller-name filters, case-insensitive substring match;
    /// `"*"` (the default) matches every controller.
    pub controllers: Vec<String>,
    /// How often the full pad state is resent.
    pub refresh_ms: u64,
    /// Trigger `…/press` hysteresis: fire at `press_on`, re-arm at
    /// `press_off` — the same defaults as the devices' publish settings.
    pub press_on: f32,
    pub press_off: f32,
}

impl Default for PadConfig {
    fn default() -> Self {
        PadConfig {
            enabled: true,
            controllers: vec!["*".into()],
            refresh_ms: 500,
            press_on: 0.6,
            press_off: 0.4,
        }
    }
}

/// One controller fact, hardware-free so the engine stays pure.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PadInput {
    /// A matched controller appeared.
    Connected,
    /// A matched controller left.
    Disconnected,
    /// A stick axis, index into [`AXES`], −1..1 (+x right, +y up).
    Axis(usize, f32),
    /// An analog trigger, index into [`TRIGGERS`], 0..1.
    Trigger(usize, f32),
    /// A digital button, index into [`BUTTONS`].
    Button(usize, bool),
}

fn float(path: &str, v: f32) -> Message {
    Message::new(format!("{PREFIX}{path}")).arg(Value::Float32(v))
}

/// −1..1 → 0..1 with the centre exactly 0.5 (the `n/` form).
fn norm(v: f32) -> f32 {
    (v + 1.0) / 2.0
}

/// The merged pad state and its publish rules. Pure — fed inputs and a
/// millisecond clock, returns the messages to publish. Multiple matched
/// controllers merge last-wins, like MIDI ports into one engine.
pub struct Engine {
    connected: u32,
    axes: [f32; 4],
    triggers: [f32; 2],
    buttons: [bool; 14],
    /// Trigger press hysteresis: armed = a press may fire.
    armed: [bool; 2],
    press_on: f32,
    press_off: f32,
    refresh_ms: u64,
    next_refresh: u64,
}

impl Engine {
    pub fn new(config: &PadConfig, now_ms: u64) -> Engine {
        let refresh_ms = config.refresh_ms.max(1);
        Engine {
            connected: 0,
            axes: [0.0; 4],
            triggers: [0.0; 2],
            buttons: [false; 14],
            armed: [true; 2],
            press_on: config.press_on,
            press_off: config.press_off,
            refresh_ms,
            next_refresh: now_ms + refresh_ms,
        }
    }

    /// Apply one input; the returned messages are what changed.
    pub fn apply(&mut self, input: PadInput) -> Vec<Message> {
        let mut out = Vec::new();
        match input {
            PadInput::Connected => {
                self.connected += 1;
                if self.connected == 1 {
                    out.push(float("active", 1.0));
                }
            }
            PadInput::Disconnected => {
                // Only a real 1 → 0 transition says anything; a stray
                // disconnect below zero must not repeat the 0.
                if self.connected > 0 {
                    self.connected -= 1;
                    if self.connected == 0 {
                        out.push(float("active", 0.0));
                    }
                }
            }
            PadInput::Axis(i, v) => {
                let v = v.clamp(-1.0, 1.0);
                if self.axes[i] != v {
                    self.axes[i] = v;
                    out.push(float(AXES[i], v));
                    out.push(float(&format!("n/{}", AXES[i]), norm(v)));
                }
            }
            PadInput::Trigger(i, v) => {
                let v = v.clamp(0.0, 1.0);
                if self.triggers[i] != v {
                    self.triggers[i] = v;
                    out.push(float(TRIGGERS[i], v));
                }
                // Hysteresis on every update: fire once past press_on,
                // re-arm below press_off (same rule as the devices').
                if self.armed[i] && v >= self.press_on {
                    self.armed[i] = false;
                    out.push(float(&format!("{}/press", TRIGGERS[i]), 1.0));
                } else if !self.armed[i] && v <= self.press_off {
                    self.armed[i] = true;
                }
            }
            PadInput::Button(i, down) => {
                if self.buttons[i] != down {
                    self.buttons[i] = down;
                    out.push(float(BUTTONS[i], if down { 1.0 } else { 0.0 }));
                    if down {
                        out.push(float(&format!("{}/press", BUTTONS[i]), 1.0));
                    }
                }
            }
        }
        out
    }

    /// The refresh clock: at each `refresh_ms` boundary resend the full
    /// current state — `active` plus every axis (both forms), trigger
    /// and button while connected; `active` 0 alone while not (the
    /// receivers' "no pad" heartbeat, matching the device apps' gamepad source).
    pub fn tick(&mut self, now_ms: u64) -> Vec<Message> {
        if now_ms < self.next_refresh {
            return Vec::new();
        }
        // One refresh per call, scheduled from now (after a stall,
        // resend once rather than replay missed boundaries).
        self.next_refresh = now_ms + self.refresh_ms;
        if self.connected == 0 {
            return vec![float("active", 0.0)];
        }
        let mut out = Vec::with_capacity(1 + 2 * AXES.len() + TRIGGERS.len() + BUTTONS.len());
        out.push(float("active", 1.0));
        for (i, name) in AXES.iter().enumerate() {
            out.push(float(name, self.axes[i]));
            out.push(float(&format!("n/{name}"), norm(self.axes[i])));
        }
        for (i, name) in TRIGGERS.iter().enumerate() {
            out.push(float(name, self.triggers[i]));
        }
        for (i, name) in BUTTONS.iter().enumerate() {
            out.push(float(name, if self.buttons[i] { 1.0 } else { 0.0 }));
        }
        out
    }
}

/// A deterministic fake controller (`--fake-pad`): sticks random-walking,
/// the left trigger sweeping through the press thresholds, buttons
/// striking and releasing. Same seed, same stream.
pub struct FakePad {
    rng: XorShift,
    started: bool,
    axes: [f32; 4],
    trigger: f32,
    /// +1/−1: the trigger sweeps up and down so presses keep firing.
    trigger_dir: f32,
    down: Vec<usize>,
}

/// How often the fake controller emits a batch.
pub const FAKE_TICK_MS: u64 = 50;

impl FakePad {
    pub fn new(seed: u64) -> FakePad {
        FakePad {
            rng: XorShift::new(seed),
            started: false,
            axes: [0.0; 4],
            trigger: 0.0,
            trigger_dir: 1.0,
            down: Vec::new(),
        }
    }

    /// The inputs for one 50 ms tick: Connected first, then every stick
    /// takes a small step, the trigger sweeps, and about every fourth
    /// tick a button strikes or releases.
    pub fn tick(&mut self) -> Vec<PadInput> {
        let mut out = Vec::new();
        if !self.started {
            self.started = true;
            out.push(PadInput::Connected);
        }
        for i in 0..self.axes.len() {
            let step = ((self.rng.next() % 13) as f32 - 6.0) / 100.0;
            self.axes[i] = (self.axes[i] + step).clamp(-1.0, 1.0);
            out.push(PadInput::Axis(i, self.axes[i]));
        }
        self.trigger += 0.05 * self.trigger_dir;
        if self.trigger >= 1.0 || self.trigger <= 0.0 {
            self.trigger = self.trigger.clamp(0.0, 1.0);
            self.trigger_dir = -self.trigger_dir;
        }
        out.push(PadInput::Trigger(0, self.trigger));
        if self.rng.next() % 4 == 0 {
            if !self.down.is_empty() && self.rng.next() % 2 == 0 {
                let index = self.rng.next() as usize % self.down.len();
                out.push(PadInput::Button(self.down.swap_remove(index), false));
            } else {
                let button = (self.rng.next() % BUTTONS.len() as u64) as usize;
                if !self.down.contains(&button) {
                    self.down.push(button);
                    out.push(PadInput::Button(button, true));
                }
            }
        }
        out
    }
}

/// The pad generator: register in `cmd::run`'s `Vec<Box<dyn Generator>>`
/// (skipped there when `[pad] enabled = false`).
pub struct PadGenerator {
    config: PadConfig,
    /// `Some(seed)` = `--fake-pad`: no controllers, a [`FakePad`] instead.
    fake: Option<u64>,
}

impl PadGenerator {
    pub fn new(config: PadConfig, fake: Option<u64>) -> PadGenerator {
        PadGenerator { config, fake }
    }
}

impl Generator for PadGenerator {
    fn name(&self) -> &'static str {
        "pad"
    }

    fn start(&mut self, out: Sender<Message>) -> anyhow::Result<()> {
        let config = self.config.clone();
        let fake = self.fake;
        std::thread::Builder::new()
            .name("simply-tussen pad".into())
            .spawn(move || run_pad(&config, fake, &out))?;
        Ok(())
    }
}

/// The generator thread: gilrs events (or fake ticks) into the engine,
/// publish what changed, refresh on the clock. Returns when the router
/// is gone.
fn run_pad(config: &PadConfig, fake: Option<u64>, out: &Sender<Message>) {
    let started = Instant::now();
    let now_ms = || started.elapsed().as_millis() as u64;
    let mut engine = Engine::new(config, 0);

    if let Some(seed) = fake {
        let mut fake_pad = FakePad::new(seed);
        let mut next_fake = 0u64;
        loop {
            std::thread::sleep(Duration::from_millis(20));
            let now = now_ms();
            while next_fake <= now {
                next_fake += FAKE_TICK_MS;
                for input in fake_pad.tick() {
                    for m in engine.apply(input) {
                        let _ = out.send(m);
                    }
                }
            }
            for m in engine.tick(now) {
                if out.send(m).is_err() {
                    return;
                }
            }
        }
    }

    // gilrs creates its IOKit/platform listener on its own thread; this
    // thread just polls. A machine where that fails runs without the
    // generator, like a machine with no MIDI.
    let mut gilrs = match gilrs::Gilrs::new() {
        Ok(g) => g,
        Err(e) => {
            log::warn!("pad: controllers unavailable, continuing without: {e}");
            return;
        }
    };
    let mut matched: Vec<gilrs::GamepadId> = Vec::new();
    loop {
        // One event (blocking up to 20 ms), then everything queued.
        if let Some(event) = gilrs.next_event_blocking(Some(Duration::from_millis(20))) {
            for input in translate(&gilrs, &mut matched, &config.controllers, &event) {
                for m in engine.apply(input) {
                    let _ = out.send(m);
                }
            }
            while let Some(event) = gilrs.next_event() {
                for input in translate(&gilrs, &mut matched, &config.controllers, &event) {
                    for m in engine.apply(input) {
                        let _ = out.send(m);
                    }
                }
            }
        }
        // The refresh: if the router is gone, stop.
        for m in engine.tick(now_ms()) {
            if out.send(m).is_err() {
                return;
            }
        }
    }
}

/// One gilrs event → the engine's inputs, tracking which controllers
/// match the `[pad] controllers` filters. Events from unmatched
/// controllers are dropped; `ButtonChanged` alone covers buttons *and*
/// analog triggers (gilrs emits it behind every press/release too).
fn translate(
    gilrs: &gilrs::Gilrs,
    matched: &mut Vec<gilrs::GamepadId>,
    filters: &[String],
    event: &gilrs::Event,
) -> Vec<PadInput> {
    use gilrs::EventType;
    match event.event {
        EventType::Connected => {
            let name = gilrs.gamepad(event.id).name().to_owned();
            if !crate::midi::port_matches(filters, &name) {
                log::info!("pad: ignoring {name} ([pad] controllers filter)");
                return Vec::new();
            }
            if matched.contains(&event.id) {
                return Vec::new();
            }
            log::info!("pad: controller found: {name}");
            matched.push(event.id);
            vec![PadInput::Connected]
        }
        EventType::Disconnected => {
            let Some(i) = matched.iter().position(|id| *id == event.id) else {
                return Vec::new();
            };
            matched.swap_remove(i);
            log::info!("pad: controller gone");
            vec![PadInput::Disconnected]
        }
        EventType::AxisChanged(axis, v, _) if matched.contains(&event.id) => {
            map_axis(axis).map(|i| PadInput::Axis(i, v)).into_iter().collect()
        }
        EventType::ButtonChanged(button, v, _) if matched.contains(&event.id) => {
            map_button(button, v).into_iter().collect()
        }
        _ => Vec::new(),
    }
}

/// gilrs stick axis → [`AXES`] index. gilrs normalizes to −1..1 with
/// right and up positive — the schema's convention already.
fn map_axis(axis: gilrs::Axis) -> Option<usize> {
    use gilrs::Axis;
    Some(match axis {
        Axis::LeftStickX => 0,
        Axis::LeftStickY => 1,
        Axis::RightStickX => 2,
        Axis::RightStickY => 3,
        _ => return None,
    })
}

/// gilrs button → the engine's input. gilrs `LeftTrigger` is the
/// shoulder (L1) and `LeftTrigger2` the analog trigger (L2); `Mode`
/// (the PS button), `C`, `Z` and `Unknown` are not in the schema.
fn map_button(button: gilrs::Button, v: f32) -> Option<PadInput> {
    use gilrs::Button as B;
    let b = |i: usize| Some(PadInput::Button(i, v >= 0.5));
    match button {
        B::LeftTrigger2 => Some(PadInput::Trigger(0, v)),
        B::RightTrigger2 => Some(PadInput::Trigger(1, v)),
        B::South => b(0),
        B::East => b(1),
        B::West => b(2),
        B::North => b(3),
        B::LeftTrigger => b(4),
        B::RightTrigger => b(5),
        B::LeftThumb => b(6),
        B::RightThumb => b(7),
        B::DPadUp => b(8),
        B::DPadDown => b(9),
        B::DPadLeft => b(10),
        B::DPadRight => b(11),
        B::Start => b(12),
        B::Select => b(13),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addresses(msgs: &[Message]) -> Vec<&str> {
        msgs.iter().map(|m| m.address.as_str()).collect()
    }
    fn value_of(msgs: &[Message], address: &str) -> Option<f32> {
        msgs.iter().find(|m| m.address == address).and_then(|m| m.args.first()?.as_float())
    }
    fn engine() -> Engine {
        Engine::new(&PadConfig::default(), 0)
    }

    #[test]
    fn active_follows_the_first_and_last_controller() {
        let mut e = engine();
        let msgs = e.apply(PadInput::Connected);
        assert_eq!(addresses(&msgs), ["/tussen/pad/active"]);
        assert_eq!(value_of(&msgs, "/tussen/pad/active"), Some(1.0));
        // A second controller: still active, nothing new to say.
        assert!(e.apply(PadInput::Connected).is_empty());
        assert!(e.apply(PadInput::Disconnected).is_empty());
        let msgs = e.apply(PadInput::Disconnected);
        assert_eq!(value_of(&msgs, "/tussen/pad/active"), Some(0.0));
        // Disconnects never go negative.
        assert!(e.apply(PadInput::Disconnected).is_empty());
    }

    #[test]
    fn axes_publish_both_forms_on_change_only() {
        let mut e = engine();
        e.apply(PadInput::Connected);
        let msgs = e.apply(PadInput::Axis(0, 1.0));
        assert_eq!(addresses(&msgs), ["/tussen/pad/lx", "/tussen/pad/n/lx"]);
        assert_eq!(value_of(&msgs, "/tussen/pad/lx"), Some(1.0));
        assert_eq!(value_of(&msgs, "/tussen/pad/n/lx"), Some(1.0));
        // Unchanged: silent.
        assert!(e.apply(PadInput::Axis(0, 1.0)).is_empty());
        // An axis already at its default (0) is silent until it moves.
        assert!(e.apply(PadInput::Axis(3, 0.0)).is_empty());
        // Centre: n/ form exactly 0.5; out-of-range input clamps.
        e.apply(PadInput::Axis(3, 0.4));
        let msgs = e.apply(PadInput::Axis(3, 0.0));
        assert_eq!(value_of(&msgs, "/tussen/pad/n/ry"), Some(0.5));
        let msgs = e.apply(PadInput::Axis(1, -2.0));
        assert_eq!(value_of(&msgs, "/tussen/pad/ly"), Some(-1.0));
    }

    #[test]
    fn trigger_press_fires_on_hysteresis_and_rearms() {
        let mut e = engine();
        e.apply(PadInput::Connected);
        // Below press_on: value only.
        let msgs = e.apply(PadInput::Trigger(1, 0.5));
        assert_eq!(addresses(&msgs), ["/tussen/pad/r2"]);
        // Past press_on: value + one press.
        let msgs = e.apply(PadInput::Trigger(1, 0.7));
        assert_eq!(addresses(&msgs), ["/tussen/pad/r2", "/tussen/pad/r2/press"]);
        // Held past the threshold: no second press.
        let msgs = e.apply(PadInput::Trigger(1, 0.9));
        assert_eq!(addresses(&msgs), ["/tussen/pad/r2"]);
        // Down to press_off re-arms; past press_on fires again.
        e.apply(PadInput::Trigger(1, 0.3));
        let msgs = e.apply(PadInput::Trigger(1, 0.8));
        assert!(addresses(&msgs).contains(&"/tussen/pad/r2/press"));
    }

    #[test]
    fn buttons_publish_edges_and_press_on_down() {
        let mut e = engine();
        e.apply(PadInput::Connected);
        let msgs = e.apply(PadInput::Button(0, true));
        assert_eq!(addresses(&msgs), ["/tussen/pad/south", "/tussen/pad/south/press"]);
        assert_eq!(value_of(&msgs, "/tussen/pad/south"), Some(1.0));
        // Held: silent. Released: 0, no press.
        assert!(e.apply(PadInput::Button(0, true)).is_empty());
        let msgs = e.apply(PadInput::Button(0, false));
        assert_eq!(addresses(&msgs), ["/tussen/pad/south"]);
        assert_eq!(value_of(&msgs, "/tussen/pad/south"), Some(0.0));
    }

    #[test]
    fn refresh_resends_the_full_state_or_the_no_pad_heartbeat() {
        let mut e = engine();
        // Nothing connected: the heartbeat alone, at the refresh cadence.
        assert!(e.tick(499).is_empty());
        let msgs = e.tick(500);
        assert_eq!(addresses(&msgs), ["/tussen/pad/active"]);
        assert_eq!(value_of(&msgs, "/tussen/pad/active"), Some(0.0));
        // Connected with some state: everything, both axis forms.
        e.apply(PadInput::Connected);
        e.apply(PadInput::Axis(0, 0.5));
        e.apply(PadInput::Trigger(0, 0.7));
        e.apply(PadInput::Button(2, true));
        let msgs = e.tick(1000);
        assert_eq!(msgs.len(), 1 + 2 * AXES.len() + TRIGGERS.len() + BUTTONS.len());
        assert_eq!(value_of(&msgs, "/tussen/pad/active"), Some(1.0));
        assert_eq!(value_of(&msgs, "/tussen/pad/lx"), Some(0.5));
        assert_eq!(value_of(&msgs, "/tussen/pad/n/lx"), Some(0.75));
        assert_eq!(value_of(&msgs, "/tussen/pad/l2"), Some(0.7));
        assert_eq!(value_of(&msgs, "/tussen/pad/west"), Some(1.0));
        assert_eq!(value_of(&msgs, "/tussen/pad/east"), Some(0.0));
        // The clock re-arms from the tick that fired.
        assert!(e.tick(1499).is_empty());
        assert!(!e.tick(1500).is_empty());
    }

    #[test]
    fn fake_pad_is_deterministic_and_drives_the_whole_engine() {
        let stream = |seed: u64| -> Vec<PadInput> {
            let mut f = FakePad::new(seed);
            (0..100).flat_map(|_| f.tick()).collect()
        };
        assert_eq!(stream(42), stream(42), "same seed, same inputs");
        assert_ne!(stream(42), stream(43), "different seed, different inputs");

        // Through the engine: it connects, moves sticks, presses the
        // trigger past the hysteresis and strikes buttons.
        let mut f = FakePad::new(7);
        let mut e = engine();
        let mut published = Vec::new();
        for _ in 0..100 {
            for input in f.tick() {
                published.extend(e.apply(input));
            }
        }
        assert_eq!(value_of(&published, "/tussen/pad/active"), Some(1.0));
        assert!(published.iter().any(|m| m.address == "/tussen/pad/lx"));
        assert!(published.iter().any(|m| m.address == "/tussen/pad/l2/press"));
        assert!(published
            .iter()
            .any(|m| m.address.starts_with("/tussen/pad/") && m.address.ends_with("/press")
                && !m.address.contains("l2")));
    }

    #[test]
    fn the_gilrs_mapping_covers_the_whole_schema() {
        use gilrs::{Axis, Button};
        for (i, axis) in
            [Axis::LeftStickX, Axis::LeftStickY, Axis::RightStickX, Axis::RightStickY]
                .iter()
                .enumerate()
        {
            assert_eq!(map_axis(*axis), Some(i));
        }
        assert_eq!(map_axis(Axis::LeftZ), None);
        assert_eq!(map_button(Button::LeftTrigger2, 0.3), Some(PadInput::Trigger(0, 0.3)));
        assert_eq!(map_button(Button::RightTrigger2, 1.0), Some(PadInput::Trigger(1, 1.0)));
        // Every digital button lands on its schema index.
        let expect = [
            (Button::South, 0), (Button::East, 1), (Button::West, 2), (Button::North, 3),
            (Button::LeftTrigger, 4), (Button::RightTrigger, 5),
            (Button::LeftThumb, 6), (Button::RightThumb, 7),
            (Button::DPadUp, 8), (Button::DPadDown, 9),
            (Button::DPadLeft, 10), (Button::DPadRight, 11),
            (Button::Start, 12), (Button::Select, 13),
        ];
        for (button, i) in expect {
            assert_eq!(map_button(button, 1.0), Some(PadInput::Button(i, true)), "{button:?}");
            assert_eq!(map_button(button, 0.0), Some(PadInput::Button(i, false)), "{button:?}");
        }
        // The PS button is not in the schema.
        assert_eq!(map_button(Button::Mode, 1.0), None);
    }

    #[test]
    fn the_default_config_is_the_spec_default() {
        let c = PadConfig::default();
        assert!(c.enabled);
        assert_eq!(c.controllers, ["*"]);
        assert_eq!(c.refresh_ms, 500);
        assert_eq!((c.press_on, c.press_off), (0.6, 0.4));
    }
}
