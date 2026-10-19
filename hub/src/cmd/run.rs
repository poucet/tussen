//! `tussen-hub run` — load the config, start the generators, run the
//! router until killed.

use std::sync::Arc;

use tussen_hub::config::Config;
use tussen_hub::config_file::ConfigFile;
use tussen_hub::console::{self, ConsoleOptions};
use tussen_hub::sync::SyncState;
use tussen_hub::generators::audio::{AudioGenerator, AudioLink};
use tussen_hub::generators::{self, Generator};
use tussen_hub::hub;
use tussen_hub::link;
use tussen_hub::midi;
use tussen_hub::pad;

/// The default `--fake-midi`/`--fake-pad` seed: fixed, so two runs show
/// the same stream (handy when comparing a monitor against a listener).
const FAKE_MIDI_SEED: u64 = 0x7475_7373; // "tuss"

pub fn run(
    config_file: ConfigFile,
    fake_midi: bool,
    fake_pad: bool,
    audio_device: Option<&str>,
) -> anyhow::Result<()> {
    log::info!("config: {config_file}");
    let mut config = Config::load(&config_file.path)?;
    if let Some(device) = audio_device {
        config.audio.device = device.to_owned();
    }
    log::info!(
        "tussen-hub: OSC on :{}, {} static destination(s), max packet {} B",
        config.osc.listen_port,
        config.dests.len(),
        config.osc.max_packet
    );
    for d in &config.dests {
        log::info!("  dest {} → {}:{} subscribe={:?} exclude={:?}", d.name, d.host, d.port, d.subscribe, d.exclude);
    }

    // The generator registration point: the audio, MIDI and
    // clock lanes each push a Box<dyn Generator> here, built from their
    // config section. Core ships none.
    let (tx, rx) = crossbeam_channel::bounded(4096);
    let mut gens: Vec<Box<dyn Generator>> = Vec::new();
    gens.push(Box::new(midi::MidiGenerator::new(
        config.midi.clone(),
        fake_midi.then_some(FAKE_MIDI_SEED),
    )));
    // The gamepad on the hub laptop.
    if config.pad.enabled {
        gens.push(Box::new(pad::PadGenerator::new(
            config.pad.clone(),
            fake_pad.then_some(FAKE_MIDI_SEED),
        )));
    }
    let audio_link = Arc::new(AudioLink::default());

    // The hub console: loopback only. A port in use is logged
    // and the hub runs without it — the console is a tool, not the show.
    let mut extras = hub::Extras::default();
    let mut _console = None;
    if config.console.enabled {
        match console::start(ConsoleOptions {
            port: config.console.port,
            osc_port: config.osc.listen_port,
            config: Some(config_file),
            statics: config.dests.iter().map(|d| (d.name.clone(), d.kind())).collect(),
            sample_rate: config.audio.sample_rate,
            state: SyncState::new(config.sync.clone(), config.remap.clone()),
        }) {
            Ok((c, router_side)) => {
                let _ = audio_link.tap.set(Arc::clone(&c.tap));
                extras.console = Some(router_side);
                _console = Some(c);
            }
            Err(e) => log::warn!("console unavailable on 127.0.0.1:{}: {e:#}", config.console.port),
        }
    }
    gens.push(Box::new(AudioGenerator::new(config.audio.clone(), Arc::clone(&audio_link))));
    // Ableton Link: the console's tempo/start-stop in, the beat grid out.
    let (link_cmds, link_tap) = match &_console {
        Some(c) => (c.link_cmds.clone(), Some(Arc::clone(&c.tap))),
        None => (crossbeam_channel::never(), None),
    };
    gens.push(Box::new(link::LinkGenerator::new(config.link.clone(), link_cmds, link_tap)));
    generators::start_all(&mut gens, &tx);
    drop(tx); // no senders left if there are no generators

    extras.audio = Some(audio_link);
    let hub = hub::spawn_with(config, rx, false, extras)?;
    hub.join();
    Ok(())
}
