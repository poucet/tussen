//! The simply-tussen CLI. The library does the work; this file parses
//! arguments and dispatches.

mod cmd;

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use tussen_hub::config_file::{self, ConfigFile, Env};

#[derive(Parser)]
#[command(name = "simply-tussen", about = "The tussen OSC router")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the router: OSC in on :9000, delivery by subscription.
    Run {
        /// The hub config. Default: the user config
        /// ($XDG_CONFIG_HOME/tussen/hub.toml, else ~/.config/tussen/hub.toml),
        /// else the nearest `hub.toml` in the current directory or a parent
        /// (the repo's shipped example), else that example built in. From
        /// either example, console Save writes the user config.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Print a stats line every second (devices, msgs in/out, audio,
        /// drops). Off by default: problems are warned about on their own.
        #[arg(long)]
        stats: bool,
        /// Publish deterministic random CCs and notes instead of opening
        /// MIDI ports — testing without hardware.
        #[arg(long = "fake-midi")]
        fake_midi: bool,
        /// Publish a deterministic fake gamepad instead of reading
        /// controllers — testing /tussen/pad/… without hardware.
        #[arg(long = "fake-pad")]
        fake_pad: bool,
        /// Override the config's [audio] device ("" = the default output).
        #[arg(long)]
        audio_device: Option<String>,
        /// Start recording a take (optionally named) at launch. The
        /// console's Record button starts and stops takes at any time.
        #[arg(long, value_name = "NAME", num_args = 0..=1, default_missing_value = "")]
        record: Option<String>,
        /// Where takes live. Default: $XDG_DATA_HOME/tussen/takes, else
        /// ~/.local/share/tussen/takes.
        #[arg(long)]
        takes_dir: Option<PathBuf>,
    },
    /// Replay a take as if live: the same hub as `run` (router,
    /// destinations, devices, RTP, console), fed from the take instead of
    /// this machine's MIDI, pad, audio and Link. Loops until stopped.
    Replay {
        /// `latest`, a take's name (or the end of it) in the takes directory, or a path.
        take: String,
        /// Loop bars A..B of the recorded timeline (from bar A's first beat to bar B's).
        #[arg(long = "loop", value_name = "A..B", value_parser = parse_bars)]
        loop_bars: Option<(u32, u32)>,
        /// Start at this bar.
        #[arg(long = "from", value_name = "BAR")]
        from_bar: Option<u32>,
        /// Play the audio this many ms ahead of the signals (negative:
        /// behind). Default: the take's final signal delay minus this
        /// config's, so audio and signals meet at TD with the sync points as set.
        #[arg(long, allow_hyphen_values = true)]
        offset_ms: Option<i32>,
        /// Lead the Link session with the take's timeline (found one if
        /// none), or leave Link alone.
        #[arg(long, value_enum, default_value_t = tussen_hub::replay::LinkMode::Lead)]
        replay_link: tussen_hub::replay::LinkMode,
        /// The hub config, as for `run`.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Where takes live, as for `run`.
        #[arg(long)]
        takes_dir: Option<PathBuf>,
        /// Print a stats line every second.
        #[arg(long)]
        stats: bool,
    },
    /// The recorded takes: name, date, length, size.
    Takes {
        #[arg(long)]
        takes_dir: Option<PathBuf>,
    },
    /// Where the hub config lives: `config path` prints the one `run`
    /// would use, `config init` copies the repo's hub.toml to the user config.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// What this machine offers the generators: the audio devices (inputs
    /// and outputs) and the MIDI input ports.
    List,
    /// Live table of everything the hub routes here: address, value,
    /// msgs/s, source (point a `monitor` destination at this port).
    Monitor {
        #[arg(long, default_value_t = 9099)]
        port: u16,
        /// Only show addresses starting with this prefix.
        #[arg(long)]
        filter: Option<String>,
    },
    /// Publish the device-schema signals at 60 Hz plus a button press every
    /// 2 s. Does not register, so it never receives anything.
    FakeDevice {
        #[arg(long, default_value = "quest")]
        name: String,
        /// sweep (one signal at a time 0→1→0 over 2 s — what OSC learn
        /// needs), circle (everything moves), or still.
        #[arg(long, default_value = "sweep")]
        pattern: tussen_hub::fake::Pattern,
        /// Where the hub listens.
        #[arg(long, default_value = "127.0.0.1:9000")]
        hub: String,
    },
    /// Register like a device and print what arrives, with rates.
    Listen {
        #[arg(long, default_value = "test")]
        name: String,
        /// Prefixes to subscribe to (repeatable). Default: /tussen/ /ext/ /djay/
        #[arg(long = "subscribe")]
        subscribe: Vec<String>,
        /// Where the hub listens.
        #[arg(long, default_value = "127.0.0.1:9000")]
        hub: String,
        /// Local port to receive on; 0 = ephemeral.
        #[arg(long, default_value_t = 0)]
        port: u16,
        /// Also request the RTP audio stream and print packet rate, peak level,
        /// loss (sequence gaps) and arrival jitter.
        #[arg(long)]
        audio: bool,
        /// Local RTP port with --audio; 0 = ephemeral.
        #[arg(long, default_value_t = 0)]
        rtp_port: u16,
    },
    /// Print an SDP for the RTP audio stream, so ffplay or VLC can play
    /// it: `simply-tussen sdp > stream.sdp && ffplay -protocol_whitelist
    /// file,rtp,udp stream.sdp` (the receiver must listen on that port —
    /// `listen --audio --rtp-port 5004` does).
    Sdp {
        /// Where the stream is received (the device's RTP address).
        #[arg(long, default_value = "127.0.0.1:5004")]
        to: String,
        #[arg(long, default_value_t = 48000)]
        sample_rate: u32,
        #[arg(long, default_value_t = 2)]
        channels: u16,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Print the config `run` would use and where it came from (--config,
    /// user, repo, or the built-in example).
    Path {
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Copy the repo's hub.toml (the shipped example outside a checkout)
    /// to the user config. Never overwrites.
    Init,
}

/// `9..17` → (9, 17).
fn parse_bars(s: &str) -> Result<(u32, u32), String> {
    let (a, b) = s.split_once("..").ok_or("expected <bar>..<bar>, e.g. 9..17")?;
    let n = |x: &str| x.trim().parse::<u32>().map_err(|e| format!("{x:?}: {e}"));
    Ok((n(a)?, n(b)?))
}

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    match Cli::parse().command {
        Command::Run {
            config,
            stats,
            fake_midi,
            fake_pad,
            audio_device,
            record,
            takes_dir,
        } => {
            let env = Env::from_process()?;
            let config = ConfigFile::resolve(config, &env)?;
            let sources = cmd::run::Sources::Live {
                fake_midi,
                fake_pad,
                audio_device,
                record: record.map(|n| Some(n).filter(|n| !n.is_empty())),
            };
            cmd::run::serve(config, sources, stats, takes_dir.or_else(|| env.takes_dir()))
        }
        Command::Replay { take, loop_bars, from_bar, offset_ms, replay_link, config, takes_dir, stats } => {
            let env = Env::from_process()?;
            let config = ConfigFile::resolve(config, &env)?;
            let opts = tussen_hub::replay::Options {
                loop_bars,
                from_bar,
                audio_lead_ms: offset_ms,
                link: replay_link,
                ..Default::default()
            };
            cmd::run::serve(config, cmd::run::Sources::Replay { take, opts }, stats, takes_dir.or_else(|| env.takes_dir()))
        }
        Command::Takes { takes_dir } => {
            let env = Env::from_process()?;
            let dir = takes_dir.or_else(|| env.takes_dir()).ok_or_else(|| anyhow::anyhow!("HOME is not set"))?;
            cmd::takes::run(&dir)
        }
        Command::Monitor { port, filter } => cmd::monitor::run(port, filter.as_deref()),
        Command::FakeDevice { name, pattern, hub } => cmd::fake_device::run(&name, pattern, &hub),
        Command::Listen { name, subscribe, hub, port, audio, rtp_port } => {
            cmd::listen::run(&name, subscribe, &hub, port, audio, rtp_port)
        }
        Command::Sdp { to, sample_rate, channels } => cmd::sdp::run(&to, sample_rate, channels),
        Command::List => cmd::list::run(),
        Command::Config { action: ConfigAction::Path { config } } => {
            println!("{}", ConfigFile::resolve(config, &Env::from_process()?)?);
            Ok(())
        }
        Command::Config { action: ConfigAction::Init } => {
            let env = Env::from_process()?;
            let to = env.user_path().ok_or_else(|| anyhow::anyhow!("HOME is not set"))?;
            let from = env.repo_path();
            config_file::init(from.as_deref(), &to)?;
            let what = from.map_or("the shipped example".into(), |p| p.display().to_string());
            println!("copied {what} to {}", to.display());
            Ok(())
        }
    }
}
