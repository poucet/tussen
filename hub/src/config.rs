//! The hub's TOML config: the OSC listener's parameters,
//! the static destinations, and the generator sections each generator
//! module defines ([midi] here via `crate::midi::MidiConfig`; [audio],
//! [clock] when those land). Unknown sections are ignored so an older
//! hub can read a newer config.

use std::path::Path;

use anyhow::Context;
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub osc: OscConfig,
    /// Static destinations: they only receive, and they are always there.
    #[serde(default, rename = "dest")]
    pub dests: Vec<DestConfig>,
    /// The MIDI generator.
    #[serde(default)]
    pub midi: crate::midi::MidiConfig,
    /// The gamepad generator — a controller plugged
    /// into the hub laptop.
    #[serde(default)]
    pub pad: crate::pad::PadConfig,
    /// The RTP audio stream
    #[serde(default)]
    pub audio: AudioConfig,
    /// The two sync points + per-device follow.
    #[serde(default)]
    pub sync: crate::sync::SyncConfig,
    /// Per-destination address rewrites.
    #[serde(default)]
    pub remap: Vec<crate::sync::RemapRule>,
    /// The hub console's web server.
    #[serde(default)]
    pub console: ConsoleConfig,
    /// Ableton Link: waited for and joined, or started from the console.
    #[serde(default)]
    pub link: crate::link::LinkConfig,
    /// NDI in from TouchDesigner, served to local pages on the console's `/ndi`.
    #[serde(default)]
    pub ndi: crate::ndi::NdiConfig,
}

/// `[console]`: the page + WebSocket, on loopback only.
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct ConsoleConfig {
    pub enabled: bool,
    /// TCP port on 127.0.0.1; 0 binds an ephemeral port (tests).
    pub port: u16,
}

impl Default for ConsoleConfig {
    fn default() -> Self {
        ConsoleConfig { enabled: true, port: 9098 }
    }
}

/// `[audio]`: what the hub streams over RTP.
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct AudioConfig {
    /// Any audio device by name, as `simply-tussen list` prints it: one with
    /// inputs is captured as an input, an output-only one is tapped.
    /// "" = the system default output (tapped).
    pub device: String,
    pub sample_rate: u32,
    pub channels: u16,
    /// Frames per RTP packet: 256 @ 48 kHz = 5.3 ms, 187.5 pkt/s.
    pub packet_frames: usize,
}

impl Default for AudioConfig {
    fn default() -> Self {
        AudioConfig {
            device: String::new(),
            sample_rate: 48000,
            channels: 2,
            packet_frames: 256,
        }
    }
}

impl AudioConfig {
    /// The device to capture by name; `None` = the system default output.
    pub fn device(&self) -> Option<&str> {
        Some(self.device.trim()).filter(|d| !d.is_empty())
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct OscConfig {
    /// Where every OSC sender sends. 0 binds an ephemeral port (tests).
    pub listen_port: u16,
    /// Largest datagram the hub emits; bundles split at this size so they
    /// never fragment over Wi-Fi.
    pub max_packet: usize,
    /// A device that stays silent this long is forgotten.
    pub device_timeout_ms: u64,
}

impl Default for OscConfig {
    fn default() -> Self {
        OscConfig { listen_port: 9000, max_packet: 1200, device_timeout_ms: 3000 }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct DestConfig {
    pub name: String,
    /// Where its OSC goes. A destination without a `port` gets no OSC
    /// (an `audio_out`-only one).
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub port: u16,
    #[serde(default)]
    pub subscribe: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
    /// engine | td | monitor | device; decides who may be delayed.
    /// Default: by name (`engine`, `monitor`), else `td`; an `audio_out`
    /// destination defaults to `engine` (the room: undelayed).
    #[serde(default)]
    pub kind: Option<crate::sync::DestKind>,
    /// Also stream the hub's audio (RTP L16, as to devices) to this
    /// port on `host` — for receivers that send no hello, like the
    /// TouchDesigner laptop's GStreamer. Absent or 0 = no audio.
    #[serde(default)]
    pub rtp_port: Option<u16>,
    /// Play the hub's audio stream on a local output device: a name as
    /// `simply-tussen list` prints it, or a unique part of one; "" = the
    /// system default output. It receives the same RTP stream as any
    /// consumer, delayed as its kind says ([`crate::sync::SyncState::audio_delay_ms`]).
    #[serde(default)]
    pub audio_out: Option<String>,
    /// The modes it is active in: `run`, `replay` (`mesh` is reserved and
    /// never activates yet). Absent: every mode for a network destination,
    /// `replay` only for an `audio_out` one (live, the room already hears
    /// the music). Inactive = not created: no socket, no RTP, no device.
    #[serde(default)]
    pub modes: Option<Vec<String>>,
}

/// What the hub is doing: which destinations exist depends on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Run,
    Replay,
    /// Reserved: accepted in `modes`, but no mode is `Mesh` yet.
    Mesh,
}

impl Mode {
    pub const ALL: [Mode; 3] = [Mode::Run, Mode::Replay, Mode::Mesh];

    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Run => "run",
            Mode::Replay => "replay",
            Mode::Mesh => "mesh",
        }
    }
}

impl DestConfig {
    pub fn kind(&self) -> crate::sync::DestKind {
        self.kind.unwrap_or_else(|| match self.audio_out {
            Some(_) => crate::sync::DestKind::Engine,
            None => crate::sync::DestKind::from_static_name(&self.name),
        })
    }

    /// Whether the router sends it OSC (it has a port).
    pub fn has_osc(&self) -> bool {
        self.port != 0
    }

    /// The modes it is active in, defaults applied (see `modes`).
    pub fn modes(&self) -> Vec<Mode> {
        match (&self.modes, &self.audio_out) {
            (Some(m), _) => Mode::ALL.into_iter().filter(|x| m.iter().any(|s| s == x.as_str())).collect(),
            (None, Some(_)) => vec![Mode::Replay],
            (None, None) => Mode::ALL.to_vec(),
        }
    }

    pub fn active_in(&self, mode: Mode) -> bool {
        self.modes().contains(&mode)
    }

    /// `replay only`, `run + replay`, …: for the startup's skipped line.
    pub fn modes_shown(&self) -> String {
        let m: Vec<&str> = self.modes().into_iter().map(Mode::as_str).collect();
        match m.len() {
            0 => "no mode".to_owned(),
            1 => format!("{} only", m[0]),
            _ => m.join(" + "),
        }
    }

    /// Where it goes, as the startup shows it.
    pub fn target_shown(&self) -> String {
        match &self.audio_out {
            Some(out) => format!("audio out {out:?}"),
            None => format!("{}:{}", self.host, self.port),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Config> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        Config::parse(&text, &path.display().to_string())
    }

    /// Parse config text; `origin` names it in the error.
    pub fn parse(text: &str, origin: &str) -> anyhow::Result<Config> {
        let config: Config = toml::from_str(text).with_context(|| format!("parsing {origin}"))?;
        for d in &config.dests {
            anyhow::ensure!(
                d.has_osc() || d.audio_out.is_some(),
                "{origin}: destination {:?} has neither a port (OSC) nor an audio_out",
                d.name
            );
            for m in d.modes.iter().flatten() {
                anyhow::ensure!(
                    Mode::ALL.iter().any(|x| x.as_str() == m),
                    "{origin}: destination {:?}: unknown mode {m:?} in `modes` (run | replay | mesh)",
                    d.name
                );
            }
        }
        Ok(config)
    }

    /// Keep only the destinations active in `mode`; returns the skipped
    /// ones. Done before the hub starts, so a skipped one is never created.
    pub fn retain_active(&mut self, mode: Mode) -> Vec<DestConfig> {
        let (on, off) = std::mem::take(&mut self.dests).into_iter().partition(|d| d.active_in(mode));
        self.dests = on;
        off
    }

    /// The config `file` names — its text, or the built-in example's.
    pub fn from_file(file: &crate::config_file::ConfigFile) -> anyhow::Result<Config> {
        Config::parse(&file.text()?, &file.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_stage1_config_parses() {
        let text = include_str!("../../hub.toml");
        let config: Config = toml::from_str(text).unwrap();
        assert_eq!(config.osc.listen_port, 9000);
        assert_eq!(config.osc.max_packet, 1200);
        assert_eq!(config.osc.device_timeout_ms, 3000);
        let names: Vec<&str> = config.dests.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["engine", "djay", "td"]);
        // The console defaults: no delays, no rules, loopback port 9098.
        assert_eq!(config.sync, crate::sync::SyncConfig::default());
        assert!(config.remap.is_empty());
        assert!(config.console.enabled);
        assert_eq!(config.console.port, 9098);
        use crate::sync::DestKind;
        let kinds: Vec<DestKind> = config.dests.iter().map(DestConfig::kind).collect();
        // djay makes music too, so it is never delayed: an engine by kind.
        assert_eq!(kinds, [DestKind::Engine, DestKind::Engine, DestKind::Td]);
        // The visuals destination gets the audio stream; the music makers do not.
        let rtp: Vec<Option<u16>> = config.dests.iter().map(|d| d.rtp_port).collect();
        assert_eq!(rtp, [None, None, Some(5006)]);
        assert_eq!(config.link, crate::link::LinkConfig { tempo: 120.0, quantum: 4.0 });
        // No local audio output until someone uncomments the example.
        assert!(config.dests.iter().all(|d| d.audio_out.is_none()));
        let example: String = text
            .lines()
            .skip_while(|l| !l.starts_with("# [[dest]]"))
            .take_while(|l| l.starts_with('#'))
            .map(|l| l.trim_start_matches('#').trim_start_matches(' '))
            .collect::<Vec<_>>()
            .join("\n");
        let out: Config = toml::from_str(&example).expect("the commented audio_out destination parses");
        assert_eq!(out.dests[0].audio_out.as_deref(), Some(""));
        assert_eq!(out.dests[0].kind(), DestKind::Engine);
        assert_eq!(out.dests[0].modes(), [Mode::Replay]);
    }

    #[test]
    fn an_audio_out_destination_needs_no_osc_and_defaults_to_the_room() {
        let c = Config::parse("[[dest]]\nname = \"speakers\"\naudio_out = \"\"\n", "t").unwrap();
        let d = &c.dests[0];
        assert!(!d.has_osc());
        assert_eq!(d.audio_out.as_deref(), Some(""));
        assert_eq!(d.kind(), crate::sync::DestKind::Engine, "the room: undelayed");
        let c = Config::parse("[[dest]]\nname = \"phones\"\naudio_out = \"Head\"\nkind = \"device\"\n", "t").unwrap();
        assert_eq!(c.dests[0].kind(), crate::sync::DestKind::Device);
        let e = Config::parse("[[dest]]\nname = \"x\"\n", "t").unwrap_err().to_string();
        assert!(e.contains("neither a port"), "{e}");
    }

    #[test]
    fn modes_default_by_destination_type_and_explicit_wins() {
        let c = Config::parse(
            r#"
            [[dest]]
            name = "td"
            port = 7000
            [[dest]]
            name = "speakers"
            audio_out = ""
            [[dest]]
            name = "phones"
            audio_out = "Head"
            modes = ["run"]
            [[dest]]
            name = "later"
            port = 7001
            modes = ["mesh"]
            "#,
            "t",
        )
        .unwrap();
        let modes: Vec<Vec<Mode>> = c.dests.iter().map(DestConfig::modes).collect();
        assert_eq!(modes, [Mode::ALL.to_vec(), vec![Mode::Replay], vec![Mode::Run], vec![Mode::Mesh]]);
        assert_eq!(c.dests[1].modes_shown(), "replay only");
        assert_eq!(c.dests[0].modes_shown(), "run + replay + mesh");
        let e = Config::parse("[[dest]]\nname = \"spk\"\naudio_out = \"\"\nmodes = [\"live\"]\n", "t")
            .unwrap_err()
            .to_string();
        assert!(e.contains("\"spk\"") && e.contains("unknown mode \"live\""), "{e}");
    }

    #[test]
    fn run_and_replay_keep_only_their_active_destinations() {
        let text = "[[dest]]\nname = \"td\"\nport = 7000\n[[dest]]\nname = \"speakers\"\naudio_out = \"\"\n\
                    [[dest]]\nname = \"phones\"\naudio_out = \"\"\nmodes = [\"run\"]\n";
        let names = |c: &Config| c.dests.iter().map(|d| d.name.clone()).collect::<Vec<_>>();
        let mut run = Config::parse(text, "t").unwrap();
        let skipped = run.retain_active(Mode::Run);
        assert_eq!(names(&run), ["td", "phones"]);
        assert_eq!(skipped.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(), ["speakers"]);
        let mut replay = Config::parse(text, "t").unwrap();
        replay.retain_active(Mode::Replay);
        assert_eq!(names(&replay), ["td", "speakers"]);
        let mut mesh = Config::parse(text, "t").unwrap();
        mesh.retain_active(Mode::Mesh);
        assert_eq!(names(&mesh), ["td"]);
    }

    #[test]
    fn sync_and_remap_sections_parse() {
        let config: Config = toml::from_str(
            r#"
            [sync]
            signal_delay_ms = 70
            td_round_trip_ms = 140
            [sync.device.phone]
            follow = "music"
            [[remap]]
            dest = "td"
            from = "/tussen/*/controller/trigger"
            to = "/td/trig"
            range = [0.0, 2.0]
            [[remap]]
            id = 4
            dest = "quest"
            from = "/ext/x"
            to = "/ext/y"
            keep = true
            "#,
        )
        .unwrap();
        assert_eq!(config.sync.signal_delay_ms, 70);
        assert_eq!(config.sync.td_round_trip_ms, 140);
        assert_eq!(config.sync.device["phone"].follow, crate::sync::Follow::Music);
        assert_eq!(config.remap.len(), 2);
        assert_eq!(config.remap[0].range, Some([0.0, 2.0]));
        assert!(!config.remap[0].keep);
        assert_eq!(config.remap[1].id, 4);
        assert!(config.remap[1].keep);
    }

    #[test]
    fn unknown_sections_are_ignored() {
        // A newer config with generator sections still parses.
        let config: Config = toml::from_str(
            "[osc]\nlisten_port = 9000\n[audio]\ndevice = \"\"\n[midi]\ninputs = [\"*\"]\n",
        )
        .unwrap();
        assert!(config.dests.is_empty());
        assert_eq!(config.midi.inputs, ["*"]);
    }

    #[test]
    fn audio_device_names_any_device_and_defaults_to_the_default_output() {
        let device = |src: &str| toml::from_str::<Config>(src).unwrap().audio.device().map(str::to_owned);
        assert_eq!(device(""), None);
        assert_eq!(device("[audio]\nsample_rate = 48000"), None);
        assert_eq!(device("[audio]\ndevice = \"  \""), None);
        assert_eq!(device("[audio]\ndevice = \"BlackHole 2ch\""), Some("BlackHole 2ch".into()));
        assert_eq!(device("[audio]\ndevice = \"MacBook Pro Speakers\""), Some("MacBook Pro Speakers".into()));
        // An older config's `source` key is ignored, not an error.
        assert_eq!(device("[audio]\nsource = \"sine\"\ndevice = \"x\""), Some("x".into()));
    }

    #[test]
    fn the_shipped_hub_toml_parses_and_names_its_capture() {
        // The repo-root hub.toml must load through the full Config::load path
        // (an untested edit to it once shipped a red trunk). Structural
        // assertions only — the [audio] device name is the operator's own and
        // changes between machines, so it is not pinned here.
        let config = Config::load(Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../hub.toml"))).unwrap();
        let _ = config.audio.device(); // parses; "" (default output) or a named capture
        // And it carries the [pad] section: the hub reads game controllers.
        assert!(config.pad.enabled);
        assert_eq!(config.pad.controllers, ["*"]);
    }

    #[test]
    fn the_pad_section_parses_and_defaults() {
        let config: Config =
            toml::from_str("[pad]\ncontrollers = [\"dualsense\"]\nenabled = false\n").unwrap();
        assert!(!config.pad.enabled);
        assert_eq!(config.pad.controllers, ["dualsense"]);
        assert_eq!(config.pad.refresh_ms, 500);
        let config: Config = toml::from_str("").unwrap();
        assert!(config.pad.enabled, "the pad generator is on by default");
    }

    #[test]
    fn the_midi_section_parses_and_defaults() {
        let config: Config =
            toml::from_str("[midi]\ninputs = [\"push\", \"iac\"]\nrefresh_ms = 250\n").unwrap();
        assert_eq!(config.midi.inputs, ["push", "iac"]);
        assert_eq!(config.midi.refresh_ms, 250);
        // Omitted entirely: the spec defaults.
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.midi.inputs, ["*"]);
        assert_eq!(config.midi.refresh_ms, 500);
    }
}
