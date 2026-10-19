//! The tussen-hub CLI. The library does the work; this file parses
//! arguments and dispatches.

mod cmd;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "tussen-hub", about = "The tussen OSC router")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the router: OSC in on :9000, delivery by subscription.
    Run {
        /// The hub config. Default: the nearest `hub.toml` in the current
        /// directory or a parent (the repo root's, from anywhere in the repo).
        #[arg(long)]
        config: Option<PathBuf>,
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
    /// it: `tussen-hub sdp > stream.sdp && ffplay -protocol_whitelist
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

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    match Cli::parse().command {
        Command::Run { config, fake_midi, fake_pad, audio_device } => {
            let config = match config {
                Some(path) => path,
                None => find_hub_toml()?,
            };
            cmd::run::run(&config, fake_midi, fake_pad, audio_device.as_deref())
        }
        Command::Monitor { port, filter } => cmd::monitor::run(port, filter.as_deref()),
        Command::FakeDevice { name, pattern, hub } => cmd::fake_device::run(&name, pattern, &hub),
        Command::Listen { name, subscribe, hub, port, audio, rtp_port } => {
            cmd::listen::run(&name, subscribe, &hub, port, audio, rtp_port)
        }
        Command::Sdp { to, sample_rate, channels } => cmd::sdp::run(&to, sample_rate, channels),
        Command::List => cmd::list::run(),
    }
}

/// The nearest `hub.toml` walking up from the current directory.
fn find_hub_toml() -> anyhow::Result<PathBuf> {
    let mut dir = std::env::current_dir()?;
    loop {
        let candidate = dir.join("hub.toml");
        if candidate.is_file() {
            return Ok(candidate);
        }
        if !dir.pop() {
            anyhow::bail!("no hub.toml in this directory or any parent; pass --config <path>");
        }
    }
}
