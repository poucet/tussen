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
    /// Any audio device by name, as `tussen-hub list` prints it: one with
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
    pub host: String,
    pub port: u16,
    pub subscribe: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
    /// engine | td | monitor; decides who may be delayed.
    /// Default: by name (`engine`, `monitor`), else `td`.
    #[serde(default)]
    pub kind: Option<crate::sync::DestKind>,
    /// Also stream the hub's audio (RTP L16, as to devices) to this
    /// port on `host` — for receivers that send no hello, like the
    /// TouchDesigner laptop's GStreamer. Absent or 0 = no audio.
    #[serde(default)]
    pub rtp_port: Option<u16>,
}

impl DestConfig {
    pub fn kind(&self) -> crate::sync::DestKind {
        self.kind.unwrap_or_else(|| crate::sync::DestKind::from_static_name(&self.name))
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
        toml::from_str(text).with_context(|| format!("parsing {origin}"))
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
