//! `simply-tussen run` and `simply-tussen replay` — load the config, start
//! the sources, run the router until killed. Both serve the same hub
//! (router, destinations, devices, RTP, console); they differ only in
//! where the signals come from: the live generators, or a take.

use std::path::PathBuf;
use std::sync::Arc;

use tussen_hub::audio_out;
use tussen_hub::config::{Config, Mode};
use tussen_hub::config_file::ConfigFile;
use tussen_hub::console::{self, ConsoleOptions};
use tussen_hub::generators::audio::{self, AudioGenerator, AudioLink, OutputTarget};
use tussen_hub::generators::{self, Generator};
use tussen_hub::hub;
use tussen_hub::link;
use tussen_hub::midi;
use tussen_hub::pad;
use tussen_hub::record;
use tussen_hub::replay;
use tussen_hub::sync::SyncState;
use tussen_hub::take::{self, Take};

/// The default `--fake-midi`/`--fake-pad` seed: fixed, so two runs show
/// the same stream (handy when comparing a monitor against a listener).
const FAKE_MIDI_SEED: u64 = 0x7475_7373; // "tuss"

/// Where the hub's signals come from.
pub enum Sources {
    /// `run`: MIDI, pad, audio capture and Link on this machine.
    Live {
        fake_midi: bool,
        fake_pad: bool,
        audio_device: Option<String>,
        /// `--record [NAME]`: start a take at launch (Some(None): unnamed).
        record: Option<Option<String>>,
    },
    /// `replay`: a take (`latest`, a name or a path), as if live.
    Replay { take: String, opts: replay::Options },
}

pub fn serve(config_file: ConfigFile, sources: Sources, stats: bool, takes_dir: Option<PathBuf>) -> anyhow::Result<()> {
    let config_shown = config_file.to_string();
    let config_text = config_file.text().ok();
    let mut config = Config::from_file(&config_file)?;
    if let Sources::Live { audio_device: Some(device), .. } = &sources {
        config.audio.device = device.clone();
    }
    // A destination inactive in this mode is never created: router,
    // RTP, audio outputs and console all see only the active ones.
    let mode = match &sources {
        Sources::Live { .. } => Mode::Run,
        Sources::Replay { .. } => Mode::Replay,
    };
    let skipped_lines: Vec<String> = config
        .retain_active(mode)
        .iter()
        .map(|d| format!("dest {} → {} — {}, inactive in {}", d.name, d.target_shown(), d.modes_shown(), mode.as_str()))
        .collect();
    log::info!(
        "simply-tussen: OSC on :{}, {} static destination(s), max packet {} B",
        config.osc.listen_port,
        config.dests.len(),
        config.osc.max_packet
    );
    for d in config.dests.iter().filter(|d| d.has_osc()) {
        log::info!("  dest {} → {}:{} subscribe={:?} exclude={:?}", d.name, d.host, d.port, d.subscribe, d.exclude);
    }
    for line in &skipped_lines {
        log::info!("  {line}");
    }
    let replaying = match &sources {
        Sources::Replay { take: which, .. } => {
            let path = tussen_hub::share::resolve(&takes_dir.clone().unwrap_or_default(), which)?;
            Some(Arc::new(Take::open(&path)?))
        }
        Sources::Live { .. } => None,
    };
    let audio_link = Arc::new(AudioLink::default());
    // `audio_out` destinations: local devices on the hub's RTP stream. Live,
    // the output [audio] taps is refused (it would capture itself).
    let tapped = match (&sources, audio::resolve(&audio::list_devices(), &config.audio)) {
        (Sources::Live { .. }, Ok(simply_audio_capture::Source::Tap(name))) => Some(name),
        _ => None,
    };
    let mut _outputs = Vec::new();
    let mut output_lines = Vec::new();
    for (d, wanted) in config.dests.iter().filter_map(|d| Some(d).zip(d.audio_out.as_deref())) {
        match open_output(wanted, &config, tapped.as_deref()) {
            Ok(out) => {
                let line = format!(
                    "dest {} → audio out {:?} ({} Hz), kind {}",
                    d.name,
                    out.device.name,
                    out.device.rate,
                    d.kind().as_str()
                );
                log::info!("  {line}");
                output_lines.push(line);
                if let Ok(mut o) = audio_link.outputs.lock() {
                    o.push(OutputTarget { name: d.name.clone(), kind: d.kind(), addr: out.receiver.addr });
                }
                _outputs.push(out);
            }
            Err(e) => log::warn!("dest {}: audio out {wanted:?} unavailable: {e:#}", d.name),
        }
    }
    // Recording on demand (console Record, `run --record`). A replay has
    // it wired the same way but switched off: the button shows, disabled.
    let recording = Some({
        let meta = take::Meta {
            sample_rate: config.audio.sample_rate,
            channels: config.audio.channels,
            packet_frames: config.audio.packet_frames,
            quantum: config.link.quantum,
            signal_delay_ms: config.sync.signal_delay_ms,
            ..take::Meta::default()
        };
        match replaying {
            None => record::Control::new(takes_dir.clone(), meta, config_text.clone(), Some(Arc::clone(&audio_link))),
            Some(_) => record::Control::switched_off(
                takes_dir.clone(),
                meta,
                Some(Arc::clone(&audio_link)),
                "recording is off during replay",
            ),
        }
    });

    // The hub console: loopback only. A port in use is logged
    // and the hub runs without it — the console is a tool, not the show.
    let mut extras = hub::Extras::default();
    let mut takes_io = hub::Takes { record: recording.as_ref().map(|r| r.slot()), replay: None };
    let mut _console = None;
    let mut console_at = ConsoleAt::Disabled;
    if config.console.enabled {
        let opts = ConsoleOptions {
            port: config.console.port,
            osc_port: config.osc.listen_port,
            config: Some(config_file),
            statics: config.dests.iter().map(|d| (d.name.clone(), d.kind())).collect(),
            sample_rate: config.audio.sample_rate,
            state: SyncState::new(config.sync.clone(), config.remap.clone()),
        };
        match console::start_with_record(opts, recording.clone()) {
            Ok((c, router_side)) => {
                let _ = audio_link.tap.set(Arc::clone(&c.tap));
                extras.console = Some(router_side);
                console_at = ConsoleAt::Up(c.addr);
                _console = Some(c);
            }
            Err(e) => {
                log::warn!("console unavailable on 127.0.0.1:{}: {e:#}", config.console.port);
                console_at = ConsoleAt::Failed;
            }
        }
    }
    // Ableton Link: the console's tempo/start-stop in, the beat grid out.
    let (link_cmds, link_tap) = match &_console {
        Some(c) => (c.link_cmds.clone(), Some(Arc::clone(&c.tap))),
        None => (crossbeam_channel::never(), None),
    };

    // The generator registration point: the audio, MIDI, pad and clock
    // lanes each push a Box<dyn Generator> here, built from their config
    // section. A replay replaces them all with the take's (live network
    // OSC still merges).
    let (tx, rx) = crossbeam_channel::bounded(4096);
    let mut gens: Vec<Box<dyn Generator>> = Vec::new();
    let mut _replay = None;
    let mut doing = None;
    match (&sources, replaying) {
        (Sources::Live { fake_midi, fake_pad, record: at_launch, .. }, _) => {
            gens.push(Box::new(midi::MidiGenerator::new(config.midi.clone(), fake_midi.then_some(FAKE_MIDI_SEED))));
            // The gamepad on the hub laptop.
            if config.pad.enabled {
                gens.push(Box::new(pad::PadGenerator::new(config.pad.clone(), fake_pad.then_some(FAKE_MIDI_SEED))));
            }
            gens.push(Box::new(AudioGenerator::new(config.audio.clone(), Arc::clone(&audio_link))));
            gens.push(Box::new(
                link::LinkGenerator::new(config.link.clone(), link_cmds, link_tap)
                    .recording(recording.as_ref().map(|r| r.slot())),
            ));
            if let (Some(name), Some(r)) = (at_launch, &recording) {
                let rec = r.start(name.as_deref())?;
                doing = Some(format!("{:<19}→ {} (console: Stop)", "recording", rec.dir().display()));
            }
        }
        (Sources::Replay { opts, .. }, Some(take)) => {
            let (rtx, rrx) = crossbeam_channel::bounded(4096);
            let mut opts = opts.clone();
            opts.quantum = config.link.quantum;
            doing = Some(format!(
                "{:<19}→ {}{}{}",
                "replaying",
                take.dir.display(),
                opts.loop_bars.map_or_else(String::new, |(a, b)| format!(", loop bars {a}..{b}")),
                if opts.link == replay::LinkMode::Off { ", Link untouched" } else { ", leading Link" },
            ));
            let r = replay::start(take, &opts, rtx, Some((config.audio.clone(), Arc::clone(&audio_link))), link_tap)?;
            log::info!("replay: {} bar line(s) in the take", r.grid.bars.len());
            takes_io.replay = Some(rrx);
            _replay = Some(r);
        }
        (Sources::Replay { .. }, None) => unreachable!("a replay opens its take above"),
    }
    generators::start_all(&mut gens, &tx);
    drop(tx); // no senders left if there are no generators

    extras.audio = Some(audio_link);
    let hub = hub::spawn_with_takes(config, rx, !stats, extras, takes_io)?;
    let osc_port = hub.addr().port();
    let extra: Vec<String> = doing.into_iter().chain(output_lines).chain(skipped_lines).collect();
    print!("{}", banner(console_at, osc_port, &console::osc_endpoint(osc_port).ips, &config_shown, &extra));
    use std::io::Write;
    let _ = std::io::stdout().flush();
    hub.join();
    Ok(())
}

/// Open an `audio_out` destination's device `wanted` on the hub's stream format.
fn open_output(wanted: &str, config: &Config, tapped: Option<&str>) -> anyhow::Result<audio_out::AudioOut> {
    let name = audio_out::device_name(wanted)?;
    anyhow::ensure!(
        tapped != Some(name.as_str()),
        "{name:?} is the output [audio] device taps; playing the capture there would feed it back into itself"
    );
    let out = audio_out::open(&name, config.audio.sample_rate, config.audio.channels)?;
    if out.device.rate != config.audio.sample_rate {
        log::info!(
            "audio out {:?}: {} Hz {}ch, resampled from {} Hz (linear)",
            out.device.name,
            out.device.rate,
            out.device.channels,
            config.audio.sample_rate
        );
    }
    Ok(out)
}

/// Where the console ended up, for the banner.
#[derive(Clone, Copy, Debug)]
enum ConsoleAt {
    Up(std::net::SocketAddr),
    Disabled,
    Failed,
}

/// The startup banner: plain stdout, after the log lines, saying where
/// to reach the hub. `lan` is the console's own LAN-IP answer
/// ([`console::osc_endpoint`]); empty prints `?`. `extra`: one line each
/// (what the hub is doing, its audio outputs).
fn banner(console: ConsoleAt, osc_port: u16, lan: &[String], config: &str, extra: &[String]) -> String {
    let console = match console {
        ConsoleAt::Up(a) => format!("http://127.0.0.1:{}", a.port()),
        ConsoleAt::Disabled => "disabled ([console] enabled = false)".into(),
        ConsoleAt::Failed => "unavailable (see the warning above)".into(),
    };
    let lan = lan.first().map_or("?", String::as_str);
    let take: String = extra.iter().map(|t| format!("  {t}\n")).collect();
    format!(
        "\n  tussen hub console → {console}\n  hub OSC            → {lan}:{osc_port} (local: 127.0.0.1:{osc_port})\n  config             → {config}\n{take}\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_banner_says_where_to_reach_the_hub() {
        let b = banner(ConsoleAt::Up("127.0.0.1:9098".parse().unwrap()), 9000, &["10.0.0.5".into()], "/c/hub.toml (user)", &[]);
        assert!(b.contains("tussen hub console → http://127.0.0.1:9098\n"), "{b}");
        assert!(b.contains("hub OSC            → 10.0.0.5:9000 (local: 127.0.0.1:9000)\n"), "{b}");
        assert!(b.contains("config             → /c/hub.toml (user)\n"), "{b}");
        let b = banner(ConsoleAt::Disabled, 9000, &[], "x", &[]);
        assert!(b.contains("console → disabled"), "{b}");
        assert!(b.contains("OSC            → ?:9000"), "{b}");
        assert!(banner(ConsoleAt::Failed, 1, &[], "x", &[]).contains("unavailable"));
        let extra = ["recording          → /t/take".to_owned(), "dest phones → audio out \"H\" (48000 Hz), kind engine".to_owned()];
        let b = banner(ConsoleAt::Disabled, 9000, &[], "x", &extra);
        assert!(
            b.contains("config             → x\n  recording          → /t/take\n  dest phones → audio out \"H\" (48000 Hz), kind engine\n"),
            "{b}"
        );
    }
}
