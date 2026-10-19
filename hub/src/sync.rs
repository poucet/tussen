//! Sync and remap: the two sync points, the delay lines that
//! apply them, and the per-destination address rewrites the hub console
//! edits live.
//!
//! * **Sync point 1, `signal_delay_ms`.** MIDI + OSC — every signal, not
//!   the audio — are held back so they line up with the mix returning
//!   through the mixer. TouchDesigner (and any other `td`-kind static)
//!   gets the signals this late and the audio undelayed.
//! * **Sync point 2, `td_round_trip_ms`.** A device gets everything the
//!   hub sends it — signals *and* its RTP audio copy — held back by this
//!   much more, so it lines up with TouchDesigner's NDI picture on the
//!   device: signals at signal + round trip, audio at round trip.
//! * **Follow, per device.** `td` (the default) is the above; `music`
//!   drops the round trip, so the device gets what TouchDesigner gets.
//! * The engine and the monitor are never delayed: the engine makes the
//!   audio everything else is aligned to. MIDI and OSC get the same delay;
//!   the class only matters to the console's timeline.
//! * **Remap.** `{id, dest, from, to, range?, keep}`: a signal routed to
//!   `dest` whose address matches `from` (OSC pattern; `*` = one segment)
//!   is delivered as `to`, its first numeric argument mapped to
//!   `lo + v·(hi − lo)` when `range` is set. `keep = true` delivers the
//!   original as well. Rules only ever rewrite what routing already sends
//!   to their destination, and are applied before the delay.
//!
//! [`SyncState`] is an immutable snapshot: the console thread owns the
//! authoritative copy, mutates it, and hands the router a fresh
//! `Arc<SyncState>` — the router never holds a lock while it sends.
//! [`DelayLine`] is the timestamped queue the router drains on time.

use std::cmp::Ordering as CmpOrdering;
use std::collections::{BTreeMap, BinaryHeap};
use std::time::{Duration, Instant};

use serde::{Deserialize, Deserializer, Serialize};
use simply_osc::{Message, Value};

/// No delay may exceed this — neither a number nor the sum a device
/// gets: a slider pushed to the end must not turn the hub into a
/// five-minute buffer, and the audio sender's delay ring is this long.
pub const MAX_DELAY_MS: u32 = 5000;
/// Messages queued per destination before new ones are dropped (and
/// counted): 5 s of a busy 60 Hz show is well under this.
pub const DELAY_LINE_CAP: usize = 65536;

/// The two signal classes: drawn differently on the
/// console's timeline, delayed the same.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Midi,
    Osc,
}

/// `/tussen/midi/` and `/tussen/clock/` are MIDI; everything else is OSC.
pub fn class_of(address: &str) -> Class {
    if address.starts_with("/tussen/midi/") || address.starts_with("/tussen/clock/") {
        Class::Midi
    } else {
        Class::Osc
    }
}

/// What kind of destination a name is — decides who may be delayed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DestKind {
    Engine,
    Td,
    Monitor,
    Device,
}

impl DestKind {
    /// A static destination's kind when its config doesn't say: by its
    /// conventional name, else a TouchDesigner-like delayable receiver.
    pub fn from_static_name(name: &str) -> DestKind {
        match name {
            "engine" => DestKind::Engine,
            "monitor" => DestKind::Monitor,
            _ => DestKind::Td,
        }
    }
}

/// Which sync point a device is fed from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Follow {
    /// Sync point 2: in time with TouchDesigner's picture (the default).
    #[default]
    Td,
    /// Sync point 1: in time with the music, like TouchDesigner itself.
    Music,
}

impl Follow {
    pub fn as_str(self) -> &'static str {
        match self {
            Follow::Td => "td",
            Follow::Music => "music",
        }
    }
}

/// One `[sync.device.<name>]` table.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct DeviceSync {
    pub follow: Follow,
}

/// The `[sync]` config section.
///
/// ```toml
/// [sync]
/// signal_delay_ms = 0        # sync point 1
/// td_round_trip_ms = 0       # sync point 2
/// [sync.device.phone]        # per device name; default "td"
/// follow = "music"
/// ```
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct SyncConfig {
    pub signal_delay_ms: u32,
    pub td_round_trip_ms: u32,
    /// Only devices that differ from the default need an entry.
    #[serde(deserialize_with = "device_tables")]
    pub device: BTreeMap<String, DeviceSync>,
}

/// `[sync.device]` entries that are tables; anything else is skipped, so
/// a config saved by the old per-destination model (`[sync.device]
/// midi_delay_ms = 40`) still loads instead of stopping the hub.
fn device_tables<'de, D: Deserializer<'de>>(d: D) -> Result<BTreeMap<String, DeviceSync>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Entry {
        Device(DeviceSync),
        Other(serde::de::IgnoredAny),
    }
    let raw = BTreeMap::<String, Entry>::deserialize(d)?;
    Ok(raw
        .into_iter()
        .filter_map(|(k, v)| match v {
            Entry::Device(s) => Some((k, s)),
            Entry::Other(_) => None,
        })
        .collect())
}

/// One `[[remap]]` rule.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RemapRule {
    /// Assigned by the hub; 0 in a config file means "give me one".
    #[serde(default)]
    pub id: u64,
    pub dest: String,
    pub from: String,
    pub to: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<[f32; 2]>,
    #[serde(default)]
    pub keep: bool,
}

impl RemapRule {
    /// Reject what can never match or never encode.
    pub fn validate(&self) -> Result<(), String> {
        if self.dest.is_empty() {
            return Err("remap without a dest".into());
        }
        if !self.from.starts_with('/') {
            return Err(format!("remap from {:?} is not an address pattern", self.from));
        }
        if !self.to.starts_with('/') || self.to.contains(['*', '?', '[', ']', '{', '}', '#', ' ']) {
            return Err(format!("remap to {:?} is not a plain OSC address", self.to));
        }
        if let Some([lo, hi]) = self.range {
            if !lo.is_finite() || !hi.is_finite() {
                return Err("remap range must be two finite numbers".into());
            }
        }
        Ok(())
    }

    /// The rewritten message: `to`, first numeric argument mapped.
    fn apply(&self, m: &Message) -> Message {
        let mut out = Message { address: self.to.clone(), args: m.args.clone() };
        if let Some([lo, hi]) = self.range {
            if let Some(first) = out.args.iter_mut().find(|v| v.as_float().is_some()) {
                let v = first.as_float().unwrap_or(0.0);
                *first = Value::Float32(lo + v * (hi - lo));
            }
        }
        out
    }
}

/// The live sync + remap state: an immutable snapshot the router reads.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SyncState {
    pub sync: SyncConfig,
    pub rules: Vec<RemapRule>,
    next_id: u64,
}

impl SyncState {
    /// From the config's `[sync]` and `[[remap]]`. Rules without an id
    /// (or with a duplicate) get a fresh one; invalid rules are dropped
    /// with a warning rather than failing the hub's start.
    pub fn new(sync: SyncConfig, rules: Vec<RemapRule>) -> SyncState {
        let mut state = SyncState { sync, rules: Vec::new(), next_id: 1 };
        state.sync.signal_delay_ms = state.sync.signal_delay_ms.min(MAX_DELAY_MS);
        state.sync.td_round_trip_ms = state.sync.td_round_trip_ms.min(MAX_DELAY_MS);
        state.sync.device.retain(|_, d| d.follow != Follow::default());
        state.next_id = rules.iter().map(|r| r.id).max().unwrap_or(0) + 1;
        for mut rule in rules {
            if let Err(e) = rule.validate() {
                log::warn!("ignoring [[remap]] rule: {e}");
                continue;
            }
            if rule.id == 0 || state.rules.iter().any(|r| r.id == rule.id) {
                rule.id = state.next_id;
                state.next_id += 1;
            }
            state.rules.push(rule);
        }
        state
    }

    /// Which sync point device `name` follows.
    pub fn follow(&self, name: &str) -> Follow {
        self.sync.device.get(name).map(|d| d.follow).unwrap_or_default()
    }

    /// The delay the router applies to every signal (MIDI and OSC alike)
    /// for this destination: engine and monitor 0; TouchDesigner the
    /// signal delay; a device signal + round trip, or just the signal
    /// delay when it follows the music.
    pub fn delay_ms(&self, name: &str, kind: DestKind) -> u32 {
        let s = &self.sync;
        match kind {
            DestKind::Engine | DestKind::Monitor => 0,
            DestKind::Td => s.signal_delay_ms,
            DestKind::Device => match self.follow(name) {
                Follow::Td => (s.signal_delay_ms + s.td_round_trip_ms).min(MAX_DELAY_MS),
                Follow::Music => s.signal_delay_ms,
            },
        }
    }

    /// The delay of this destination's RTP audio copy: the round trip for
    /// a device that follows TouchDesigner, 0 for everyone else (the
    /// audio is never delayed at sync point 1).
    pub fn audio_delay_ms(&self, name: &str, kind: DestKind) -> u32 {
        match (kind, self.follow(name)) {
            (DestKind::Device, Follow::Td) => self.sync.td_round_trip_ms,
            _ => 0,
        }
    }

    /// `set_sync`: either number, each optional, clamped.
    pub fn set_sync(&mut self, signal_delay_ms: Option<u32>, td_round_trip_ms: Option<u32>) {
        if let Some(v) = signal_delay_ms {
            self.sync.signal_delay_ms = v.min(MAX_DELAY_MS);
        }
        if let Some(v) = td_round_trip_ms {
            self.sync.td_round_trip_ms = v.min(MAX_DELAY_MS);
        }
    }

    /// `set_follow`: only a device has the switch.
    pub fn set_follow(&mut self, name: &str, kind: DestKind, follow: Follow) -> Result<(), String> {
        if kind != DestKind::Device {
            return Err(format!("{name} is not a device; only devices have the follow switch"));
        }
        if follow == Follow::default() {
            self.sync.device.remove(name);
        } else {
            self.sync.device.insert(name.to_owned(), DeviceSync { follow });
        }
        Ok(())
    }

    /// Insert (id 0 = new) or replace a rule; returns its id.
    pub fn upsert(&mut self, mut rule: RemapRule) -> Result<u64, String> {
        rule.validate()?;
        if rule.id != 0 {
            if let Some(slot) = self.rules.iter_mut().find(|r| r.id == rule.id) {
                *slot = rule.clone();
                return Ok(rule.id);
            }
            self.next_id = self.next_id.max(rule.id + 1);
        } else {
            rule.id = self.next_id.max(1);
            self.next_id = rule.id + 1;
        }
        let id = rule.id;
        self.rules.push(rule);
        Ok(id)
    }

    pub fn delete(&mut self, id: u64) -> bool {
        let before = self.rules.len();
        self.rules.retain(|r| r.id != id);
        self.rules.len() != before
    }

    /// Remap `m` for destination `dest`. `None` = no rule matched: send
    /// the original (the allocation-free common path). `Some((keep,
    /// out))` = send `out`, and the original too when `keep`.
    pub fn remap(&self, dest: &str, m: &Message) -> Option<(bool, Vec<Message>)> {
        if self.rules.is_empty() {
            return None;
        }
        let mut out = Vec::new();
        let mut keep = true;
        for rule in self.rules.iter().filter(|r| r.dest == dest) {
            if simply_osc::matches(&rule.from, &m.address) {
                keep &= rule.keep;
                out.push(rule.apply(m));
            }
        }
        if out.is_empty() {
            None
        } else {
            Some((keep, out))
        }
    }
}

struct Entry {
    due: Instant,
    seq: u64,
    bytes: Vec<u8>,
}

impl PartialEq for Entry {
    fn eq(&self, other: &Self) -> bool {
        self.due == other.due && self.seq == other.seq
    }
}
impl Eq for Entry {}
impl PartialOrd for Entry {
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(other))
    }
}
impl Ord for Entry {
    /// Reversed, so the std max-heap pops the earliest due first; `seq`
    /// keeps equal-time messages in arrival order.
    fn cmp(&self, other: &Self) -> CmpOrdering {
        other.due.cmp(&self.due).then(other.seq.cmp(&self.seq))
    }
}

/// One destination's delay line: encoded messages waiting for their due
/// time. A heap rather than a FIFO, so lowering a delay mid-show lets the
/// new, earlier-due messages out on time instead of behind the old ones.
#[derive(Default)]
pub struct DelayLine {
    heap: BinaryHeap<Entry>,
    seq: u64,
    pub dropped: u64,
}

impl DelayLine {
    /// Queue `bytes` for `due`; a full line drops and counts.
    pub fn push(&mut self, due: Instant, bytes: Vec<u8>) {
        if self.heap.len() >= DELAY_LINE_CAP {
            self.dropped += 1;
            return;
        }
        self.seq += 1;
        self.heap.push(Entry { due, seq: self.seq, bytes });
    }

    pub fn next_due(&self) -> Option<Instant> {
        self.heap.peek().map(|e| e.due)
    }

    pub fn len(&self) -> usize {
        self.heap.len()
    }

    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    /// Move everything due at `now` into `out`, earliest first.
    pub fn pop_due(&mut self, now: Instant, out: &mut Vec<Vec<u8>>) {
        while self.heap.peek().is_some_and(|e| e.due <= now) {
            if let Some(e) = self.heap.pop() {
                out.push(e.bytes);
            }
        }
    }
}

/// `now + ms`.
pub fn due(now: Instant, ms: u32) -> Instant {
    now + Duration::from_millis(ms as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(dest: &str, from: &str, to: &str) -> RemapRule {
        RemapRule { id: 0, dest: dest.into(), from: from.into(), to: to.into(), range: None, keep: false }
    }

    #[test]
    fn midi_and_clock_are_the_midi_class_everything_else_osc() {
        assert_eq!(class_of("/tussen/midi/cc/74"), Class::Midi);
        assert_eq!(class_of("/tussen/midi/note/60"), Class::Midi);
        assert_eq!(class_of("/tussen/clock/beat"), Class::Midi);
        assert_eq!(class_of("/tussen/quest/head/yaw"), Class::Osc);
        assert_eq!(class_of("/ext/tablet/fader"), Class::Osc);
        assert_eq!(class_of("/cmd/quest/recenter"), Class::Osc);
        // A prefix, not a substring.
        assert_eq!(class_of("/tussen/midiish/x"), Class::Osc);
        assert_eq!(class_of("/ext/tussen/midi/x"), Class::Osc);
    }

    fn state(signal: u32, round_trip: u32) -> SyncState {
        SyncState::new(SyncConfig { signal_delay_ms: signal, td_round_trip_ms: round_trip, ..Default::default() }, vec![])
    }

    #[test]
    fn the_engine_and_the_monitor_are_never_delayed() {
        let s = state(40, 120);
        for kind in [DestKind::Engine, DestKind::Monitor] {
            assert_eq!(s.delay_ms("engine", kind), 0);
            assert_eq!(s.audio_delay_ms("engine", kind), 0);
        }
        // No follow switch for statics.
        let mut s = s;
        assert!(s.set_follow("engine", DestKind::Engine, Follow::Music).is_err());
        assert!(s.set_follow("td", DestKind::Td, Follow::Music).is_err());
        assert!(s.sync.device.is_empty());
    }

    #[test]
    fn td_gets_the_signal_delay_and_undelayed_audio() {
        let s = state(40, 120);
        assert_eq!(s.delay_ms("td", DestKind::Td), 40);
        assert_eq!(s.audio_delay_ms("td", DestKind::Td), 0);
    }

    #[test]
    fn a_device_gets_signal_plus_round_trip_audio_included() {
        let mut s = state(40, 120);
        assert_eq!(s.follow("quest"), Follow::Td);
        assert_eq!(s.delay_ms("quest", DestKind::Device), 160);
        assert_eq!(s.audio_delay_ms("quest", DestKind::Device), 120);
        // Both numbers apply live.
        s.set_sync(None, Some(80));
        assert_eq!(s.delay_ms("quest", DestKind::Device), 120);
        assert_eq!(s.audio_delay_ms("quest", DestKind::Device), 80);
        s.set_sync(Some(10), None);
        assert_eq!(s.delay_ms("quest", DestKind::Device), 90);
        assert_eq!(s.delay_ms("td", DestKind::Td), 10);
        // Clamped, each number and the sum.
        s.set_sync(Some(999_999), Some(4000));
        assert_eq!(s.sync.signal_delay_ms, MAX_DELAY_MS);
        assert_eq!(s.delay_ms("quest", DestKind::Device), MAX_DELAY_MS);
    }

    #[test]
    fn follow_music_drops_the_round_trip() {
        let mut s = state(40, 120);
        s.set_follow("phone", DestKind::Device, Follow::Music).unwrap();
        assert_eq!(s.delay_ms("phone", DestKind::Device), s.delay_ms("td", DestKind::Td));
        assert_eq!(s.audio_delay_ms("phone", DestKind::Device), 0);
        // Per device: another one still follows TD.
        assert_eq!(s.delay_ms("quest", DestKind::Device), 160);
        // Back to td removes the entry (the default needs none).
        s.set_follow("phone", DestKind::Device, Follow::Td).unwrap();
        assert!(s.sync.device.is_empty());
        assert_eq!(s.audio_delay_ms("phone", DestKind::Device), 120);
    }

    #[test]
    fn an_old_per_destination_sync_section_still_loads() {
        let c: SyncConfig = toml::from_str(
            "device_trim_ms = 30\nsignal_delay_ms = 25\n[device]\nmidi_delay_ms = 40\n[device.phone]\nfollow = \"music\"\n[dest.td]\nosc_delay_ms = 5\n",
        )
        .unwrap();
        assert_eq!(c.signal_delay_ms, 25);
        assert_eq!(c.device.len(), 1);
        assert_eq!(c.device["phone"].follow, Follow::Music);
    }

    #[test]
    fn remap_wildcard_rename_range_keep() {
        let mut s = SyncState::default();
        let mut r = rule("td", "/tussen/*/controller/trigger", "/td/trigger");
        r.range = Some([10.0, 20.0]);
        let id = s.upsert(r).unwrap();
        assert!(id > 0);

        let m = Message::new("/tussen/quest/controller/trigger").arg(Value::Float32(0.5));
        let (keep, out) = s.remap("td", &m).unwrap();
        assert!(!keep);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].address, "/td/trigger");
        assert_eq!(out[0].args, vec![Value::Float32(15.0)]);

        // `*` is one segment only.
        assert!(s.remap("td", &Message::new("/tussen/a/b/controller/trigger").arg(Value::Float32(1.0))).is_none());
        // Int arguments map as numbers.
        let (_, out) = s.remap("td", &Message::new("/tussen/phone/controller/trigger").arg(Value::Int32(1))).unwrap();
        assert_eq!(out[0].args, vec![Value::Float32(20.0)]);

        // keep=true: the original goes too. Replace by id.
        let mut r = rule("td", "/tussen/*/controller/trigger", "/td/trigger");
        r.id = id;
        r.keep = true;
        assert_eq!(s.upsert(r).unwrap(), id);
        assert_eq!(s.rules.len(), 1);
        let (keep, out) = s.remap("td", &m).unwrap();
        assert!(keep);
        // No range now: the value passes unchanged.
        assert_eq!(out[0].args, vec![Value::Float32(0.5)]);

        assert!(s.delete(id));
        assert!(!s.delete(id));
        assert!(s.remap("td", &m).is_none());
    }

    #[test]
    fn remap_rules_are_per_destination() {
        let mut s = SyncState::default();
        s.upsert(rule("td", "/ext/x", "/td/x")).unwrap();
        s.upsert(rule("quest", "/ext/x", "/q/x")).unwrap();
        let m = Message::new("/ext/x").arg(Value::Float32(1.0));
        assert_eq!(s.remap("td", &m).unwrap().1[0].address, "/td/x");
        assert_eq!(s.remap("quest", &m).unwrap().1[0].address, "/q/x");
        assert!(s.remap("engine", &m).is_none());
    }

    #[test]
    fn invalid_rules_are_refused_and_config_ids_are_fixed_up() {
        let mut s = SyncState::default();
        assert!(s.upsert(rule("", "/a", "/b")).is_err());
        assert!(s.upsert(rule("td", "a", "/b")).is_err());
        assert!(s.upsert(rule("td", "/a", "/b/*")).is_err());
        let rules = vec![
            RemapRule { id: 7, ..rule("td", "/a", "/b") },
            RemapRule { id: 7, ..rule("td", "/c", "/d") },
            rule("td", "/e", "/f"),
            rule("td", "bad", "/f"),
        ];
        let s = SyncState::new(SyncConfig::default(), rules);
        let ids: Vec<u64> = s.rules.iter().map(|r| r.id).collect();
        assert_eq!(ids, [7, 8, 9]);
    }

    #[test]
    fn delay_line_releases_in_due_order() {
        let t0 = Instant::now();
        let mut line = DelayLine::default();
        line.push(due(t0, 30), b"c".to_vec());
        line.push(due(t0, 10), b"a".to_vec());
        line.push(due(t0, 10), b"b".to_vec());
        assert_eq!(line.next_due(), Some(due(t0, 10)));
        let mut out = Vec::new();
        line.pop_due(due(t0, 5), &mut out);
        assert!(out.is_empty());
        line.pop_due(due(t0, 10), &mut out);
        assert_eq!(out, [b"a".to_vec(), b"b".to_vec()]);
        out.clear();
        line.pop_due(due(t0, 100), &mut out);
        assert_eq!(out, [b"c".to_vec()]);
        assert!(line.is_empty());
    }
}
