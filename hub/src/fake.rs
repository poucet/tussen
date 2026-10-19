//! The fake device: publishes the device-schema signals at 60 Hz
//! plus a button press every 2 s. It does **not** register, so it never
//! receives anything — it exists to exercise routing, the monitor and
//! OSC learn without a headset.
//!
//! Patterns:
//! * `sweep` — every signal holds still except one, which moves 0→1→0
//!   over 2 s, then the next. OSC learn binds the first address that
//!   *moves*, so exactly one moving signal is the point.
//! * `circle` — the head walks a 1 m circle, turning to face along it;
//!   the controller swings. Everything plausible at once.
//! * `still` — every signal constant. A routing and rate check.

use simply_osc::{Message, Value};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Pattern {
    Sweep,
    Circle,
    Still,
}

impl std::str::FromStr for Pattern {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> anyhow::Result<Pattern> {
        match s {
            "sweep" => Ok(Pattern::Sweep),
            "circle" => Ok(Pattern::Circle),
            "still" => Ok(Pattern::Still),
            other => anyhow::bail!("unknown pattern {other:?} (sweep|circle|still)"),
        }
    }
}

/// The normalized signals the sweep moves through, one at a time, in a
/// learnable order. Relative to `/tussen/{dev}/`.
pub const SWEEP_ADDRESSES: &[&str] = &[
    "head/n/yaw",
    "head/n/pitch",
    "head/n/roll",
    "head/n/height",
    "head/n/x",
    "head/n/z",
    "head/n/speed",
    "head/n/turn",
    "controller/trigger",
    "controller/grip",
    "controller/n/side",
    "controller/n/height",
    "controller/n/reach",
    "controller/n/speed",
    "controller/n/roll",
    "controller/n/pitch",
];

/// How long one sweep of one signal takes (0→1→0).
pub const SWEEP_PERIOD: f64 = 2.0;
/// A button press fires this often.
pub const PRESS_PERIOD: f64 = 2.0;

pub struct FakeDevice {
    prefix: String,
    pattern: Pattern,
    t: f64,
    seq: i32,
    next_press: f64,
}

impl FakeDevice {
    pub fn new(name: &str, pattern: Pattern) -> FakeDevice {
        FakeDevice {
            prefix: format!("/tussen/{name}/"),
            pattern,
            t: 0.0,
            seq: 0,
            next_press: PRESS_PERIOD,
        }
    }

    /// The signal the sweep is currently moving, if sweeping.
    pub fn sweeping(&self) -> Option<&'static str> {
        match self.pattern {
            Pattern::Sweep => {
                let index = (self.t / SWEEP_PERIOD) as usize % SWEEP_ADDRESSES.len();
                Some(SWEEP_ADDRESSES[index])
            }
            _ => None,
        }
    }

    /// Advance by `dt` seconds and produce this tick's messages: the full
    /// device-schema signal set, `seq`, and a press event every 2 s.
    pub fn tick(&mut self, dt: f64) -> Vec<Message> {
        self.t += dt;
        self.seq += 1;
        let mut signals = self.signals();
        if let Some(active) = self.sweeping() {
            let phase = (self.t % SWEEP_PERIOD) / SWEEP_PERIOD;
            let value = (1.0 - (2.0 * phase - 1.0).abs()) as f32;
            set(&mut signals, active, value);
        }
        let mut out: Vec<Message> = signals
            .into_iter()
            .map(|(path, v)| {
                Message::new(format!("{}{path}", self.prefix)).arg(Value::Float32(v))
            })
            .collect();
        out.push(Message::new(format!("{}seq", self.prefix)).arg(Value::Int32(self.seq)));
        if self.t >= self.next_press {
            self.next_press += PRESS_PERIOD;
            out.push(
                Message::new(format!("{}controller/a/press", self.prefix)).arg(Value::Float32(1.0)),
            );
        }
        out
    }

    /// The full continuous signal set for the current pattern and time.
    fn signals(&self) -> Vec<(String, f32)> {
        let t = self.t;
        // Head pose: still/sweep hold a neutral standing pose; circle
        // walks a 1 m circle (8 s around), facing along it.
        let (x, z, yaw_deg, turn_dps, speed) = match self.pattern {
            Pattern::Circle => {
                let a = t * std::f64::consts::TAU / 8.0;
                (
                    a.sin() as f32,
                    a.cos() as f32,
                    wrap_degrees(a.to_degrees() as f32),
                    45.0,
                    std::f64::consts::TAU as f32 / 8.0,
                )
            }
            _ => (0.0, 0.0, 0.0, 0.0, 0.0),
        };
        let yaw_rad = (yaw_deg as f64).to_radians();
        let (qy, qw) = ((yaw_rad / 2.0).sin() as f32, (yaw_rad / 2.0).cos() as f32);
        let height = 1.6_f32;

        let mut s: Vec<(String, f32)> = Vec::with_capacity(64);
        let mut put = |path: &str, v: f32| s.push((path.to_owned(), v));

        // head
        put("head/pos/x", x);
        put("head/pos/y", height);
        put("head/pos/z", z);
        put("head/quat/x", 0.0);
        put("head/quat/y", qy);
        put("head/quat/z", 0.0);
        put("head/quat/w", qw);
        put("head/yaw", yaw_deg);
        put("head/pitch", 0.0);
        put("head/roll", 0.0);
        put("head/n/yaw", norm(yaw_deg, -90.0, 90.0));
        put("head/n/pitch", 0.5);
        put("head/n/roll", 0.5);
        put("head/n/height", 0.5);
        put("head/n/x", norm(x, -1.5, 1.5));
        put("head/n/z", norm(z, -1.5, 1.5));
        put("head/n/speed", norm(speed, 0.0, 1.5));
        put("head/n/turn", norm(turn_dps, 0.0, 180.0));

        // the controller, at rest beside the body (circle: it swings
        // as the head walks).
        let swing = match self.pattern {
            Pattern::Circle => (t * std::f64::consts::TAU / 2.0).sin() as f32 * 0.2,
            _ => 0.0,
        };
        put("controller/active", 1.0);
        put("controller/pos/x", x + 0.3);
        put("controller/pos/y", height - 0.6);
        put("controller/pos/z", z + swing);
        put("controller/quat/x", 0.0);
        put("controller/quat/y", qy);
        put("controller/quat/z", 0.0);
        put("controller/quat/w", qw);
        put("controller/trigger", 0.0);
        put("controller/grip", 0.0);
        put("controller/stick/x", 0.0);
        put("controller/stick/y", 0.0);
        put("controller/stick/click", 0.0);
        put("controller/n/side", norm(0.3, -0.8, 0.8));
        put("controller/n/height", norm(-0.6, -0.9, 0.3));
        put("controller/n/reach", norm(0.3, 0.1, 0.8));
        put("controller/n/speed", norm(swing.abs() * 3.0, 0.0, 3.0));
        put("controller/n/roll", 0.5);
        put("controller/n/pitch", 0.5);
        put("controller/a", 0.0);
        put("controller/b", 0.0);
        s
    }
}

fn set(signals: &mut [(String, f32)], path: &str, value: f32) {
    if let Some(entry) = signals.iter_mut().find(|(p, _)| p == path) {
        entry.1 = value;
    }
}

fn norm(v: f32, lo: f32, hi: f32) -> f32 {
    ((v - lo) / (hi - lo)).clamp(0.0, 1.0)
}

fn wrap_degrees(d: f32) -> f32 {
    let mut d = d % 360.0;
    if d > 180.0 {
        d -= 360.0;
    }
    if d <= -180.0 {
        d += 360.0;
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value_of(msgs: &[Message], address: &str) -> Option<f32> {
        msgs.iter().find(|m| m.address == address).and_then(|m| m.args.first()?.as_float())
    }

    #[test]
    fn sweep_moves_exactly_one_signal_through_a_triangle() {
        let mut d = FakeDevice::new("quest", Pattern::Sweep);
        // Quarter through the first sweep window: the first address is at
        // 0.5 on the way up, everything else in the sweep list is parked.
        let msgs = d.tick(0.5);
        assert_eq!(d.sweeping(), Some("head/n/yaw"));
        let v = value_of(&msgs, "/tussen/quest/head/n/yaw").unwrap();
        assert!((v - 0.5).abs() < 1e-6, "triangle at 0.5 s, got {v}");
        for other in &SWEEP_ADDRESSES[1..] {
            let address = format!("/tussen/quest/{other}");
            let before = value_of(&msgs, &address).unwrap();
            assert!(before < 0.75, "{address} should be parked, got {before}");
        }
        // Peak at 1 s, back near zero at the end of the window.
        let msgs = d.tick(0.5);
        assert!((value_of(&msgs, "/tussen/quest/head/n/yaw").unwrap() - 1.0).abs() < 1e-6);
        let msgs = d.tick(0.99);
        assert!(value_of(&msgs, "/tussen/quest/head/n/yaw").unwrap() < 0.02);
        // The next window sweeps the next signal.
        d.tick(0.51);
        assert_eq!(d.sweeping(), Some("head/n/pitch"));
    }

    #[test]
    fn every_tick_carries_the_full_signal_set_and_seq() {
        let mut d = FakeDevice::new("quest", Pattern::Still);
        let msgs = d.tick(1.0 / 60.0);
        for a in ["head/pos/x", "head/quat/w", "head/n/turn", "controller/trigger", "controller/a", "seq"] {
            assert!(
                msgs.iter().any(|m| m.address == format!("/tussen/quest/{a}")),
                "missing {a}"
            );
        }
        let first = value_of(&msgs, "/tussen/quest/seq").unwrap();
        let msgs = d.tick(1.0 / 60.0);
        assert_eq!(value_of(&msgs, "/tussen/quest/seq").unwrap(), first + 1.0);
    }

    #[test]
    fn a_press_fires_every_two_seconds_and_only_then() {
        let mut d = FakeDevice::new("quest", Pattern::Still);
        let mut presses = 0;
        // 6.1 s of 60 Hz ticks (a hair past 6 s: 1/60 doesn't sum exactly).
        for _ in 0..366 {
            let msgs = d.tick(1.0 / 60.0);
            presses += msgs
                .iter()
                .filter(|m| m.address == "/tussen/quest/controller/a/press")
                .count();
        }
        assert_eq!(presses, 3, "~6 s at one press per 2 s");
    }
}
