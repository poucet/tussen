//! `simply-tussen run` — load the config, start the generators, run the
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
    stats: bool,
) -> anyhow::Result<()> {
    let config_shown = config_file.to_string();
    let mut config = Config::from_file(&config_file)?;
    if let Some(device) = audio_device {
        config.audio.device = device.to_owned();
    }
    log::info!(
        "simply-tussen: OSC on :{}, {} static destination(s), max packet {} B",
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
    let mut console_at = ConsoleAt::Disabled;
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
                console_at = ConsoleAt::Up(c.addr);
                _console = Some(c);
            }
            Err(e) => {
                log::warn!("console unavailable on 127.0.0.1:{}: {e:#}", config.console.port);
                console_at = ConsoleAt::Failed;
            }
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
    let hub = hub::spawn_with(config, rx, !stats, extras)?;
    let osc_port = hub.addr().port();
    print!("{}", banner(console_at, osc_port, &console::osc_endpoint(osc_port).ips, &config_shown));
    use std::io::Write;
    let _ = std::io::stdout().flush();
    hub.join();
    Ok(())
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
/// ([`console::osc_endpoint`]); empty prints `?`.
fn banner(console: ConsoleAt, osc_port: u16, lan: &[String], config: &str) -> String {
    let console = match console {
        ConsoleAt::Up(a) => format!("http://127.0.0.1:{}", a.port()),
        ConsoleAt::Disabled => "disabled ([console] enabled = false)".into(),
        ConsoleAt::Failed => "unavailable (see the warning above)".into(),
    };
    let lan = lan.first().map_or("?", String::as_str);
    format!(
        "\n  tussen hub console → {console}\n  hub OSC            → {lan}:{osc_port} (local: 127.0.0.1:{osc_port})\n  config             → {config}\n\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_banner_says_where_to_reach_the_hub() {
        let b = banner(ConsoleAt::Up("127.0.0.1:9098".parse().unwrap()), 9000, &["10.0.0.5".into()], "/c/hub.toml (user)");
        assert!(b.contains("tussen hub console → http://127.0.0.1:9098\n"), "{b}");
        assert!(b.contains("hub OSC            → 10.0.0.5:9000 (local: 127.0.0.1:9000)\n"), "{b}");
        assert!(b.contains("config             → /c/hub.toml (user)\n"), "{b}");
        let b = banner(ConsoleAt::Disabled, 9000, &[], "x");
        assert!(b.contains("console → disabled"), "{b}");
        assert!(b.contains("OSC            → ?:9000"), "{b}");
        assert!(banner(ConsoleAt::Failed, 1, &[], "x").contains("unavailable"));
    }
}
