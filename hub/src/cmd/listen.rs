//! `tussen-hub listen` — register like a device and print
//! signal rates; with `--audio`, also receive the hub's RTP stream and
//! print packet rate, loss (sequence gaps) and arrival jitter (RFC 3550
//! interarrival jitter, reported in ms at the 48 kHz sample clock).

use std::collections::BTreeMap;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use simply_osc::{Listener, Message, Value};
use tussen_hub::generators::audio::rtp;

/// What a real device subscribes to and excludes by default.
const DEFAULT_SUBSCRIBE: &[&str] = &["/tussen/", "/ext/", "/djay/"];
const DEFAULT_EXCLUDE: &[&str] = &["/tussen/hub/"];
const HELLO_PERIOD: Duration = Duration::from_secs(1);

pub fn run(
    name: &str,
    subscribe: Vec<String>,
    hub: &str,
    port: u16,
    audio: bool,
    rtp_port: u16,
) -> anyhow::Result<()> {
    let hub_addr = super::resolve_hub(hub)?;
    let listener = Listener::bind(("0.0.0.0", port))?;
    let local_port = listener.local_addr().port();
    let sender = UdpSocket::bind("0.0.0.0:0")?;

    // --audio: an RTP receiver on its own thread, announced in the hello.
    let (announced_rtp_port, audio_stats) = if audio {
        let socket = UdpSocket::bind(("0.0.0.0", rtp_port))?;
        let bound = socket.local_addr()?.port();
        let stats = Arc::new(RtpStats::default());
        std::thread::Builder::new().name("listen rtp".into()).spawn({
            let stats = Arc::clone(&stats);
            move || rtp_receive_loop(&socket, &stats)
        })?;
        eprintln!("audio: RTP on :{bound}");
        (bound, Some(stats))
    } else {
        (0, None)
    };

    let subscribe = if subscribe.is_empty() {
        DEFAULT_SUBSCRIBE.iter().map(|s| s.to_string()).collect::<Vec<_>>()
    } else {
        subscribe
    };
    let hello = Message::new("/tussen/hub/hello")
        .arg(Value::Str(name.into()))
        .arg(Value::Int32(local_port as i32))
        .arg(Value::Str(subscribe.join(",")))
        .arg(Value::Str(DEFAULT_EXCLUDE.join(",")))
        .arg(Value::Int32(announced_rtp_port as i32)) // 0 = no RTP audio
        .encode()?;
    eprintln!(
        "listening as device {name} on :{local_port}, hello → {hub_addr}, subscribe {subscribe:?}"
    );

    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    let mut total = 0u64;
    let mut audio_packets_before = 0u64;
    let mut next_hello = Instant::now();
    let mut last_report = Instant::now();
    loop {
        if Instant::now() >= next_hello {
            let _ = sender.send_to(&hello, hub_addr);
            next_hello += HELLO_PERIOD;
        }
        if let Some(inbound) = listener.recv_timeout(Duration::from_millis(100)) {
            for (_, m) in inbound.packet.messages() {
                total += 1;
                *counts.entry(m.address.clone()).or_default() += 1;
            }
        }
        let now = Instant::now();
        let elapsed = now.duration_since(last_report);
        if elapsed >= Duration::from_secs(1) {
            let secs = elapsed.as_secs_f64();
            let mut rates: Vec<(&String, &u64)> = counts.iter().collect();
            rates.sort_by(|a, b| b.1.cmp(a.1));
            println!(
                "{:.0} msgs/s over {} address(es)",
                total as f64 / secs,
                counts.len()
            );
            for (address, count) in rates.iter().take(10) {
                println!("  {:<44} {:>6.1}/s", address, **count as f64 / secs);
            }
            if let Some(stats) = &audio_stats {
                let packets = stats.packets.load(Ordering::Relaxed);
                let peak = f32::from_bits(stats.peak_bits.swap(0, Ordering::Relaxed));
                println!(
                    "  audio: {:.1} pkt/s, lost {} total, jitter {:.3} ms, peak {}",
                    (packets - audio_packets_before) as f64 / secs,
                    stats.lost.load(Ordering::Relaxed),
                    f64::from_bits(stats.jitter_ms_bits.load(Ordering::Relaxed)),
                    if peak > 0.0 { format!("{:.1} dBFS", 20.0 * peak.log10()) } else { "silent".into() },
                );
                audio_packets_before = packets;
            }
            counts.clear();
            total = 0;
            last_report = now;
        }
    }
}

/// Shared between the RTP thread and the report loop. Jitter is an f64
/// stored as bits — a Mutex for one float would be ceremony.
#[derive(Default)]
struct RtpStats {
    packets: AtomicU64,
    lost: AtomicU64,
    jitter_ms_bits: AtomicU64,
    /// Largest |sample| since the last report, as f32 bits (0 = silence).
    peak_bits: AtomicU32,
}

/// Receive and account for the hub's RTP stream: packets,
/// losses as sequence-number gaps, and RFC 3550 §6.4.1 interarrival
/// jitter (timestamps are samples at 48 kHz, so 48 units = 1 ms).
fn rtp_receive_loop(socket: &UdpSocket, stats: &RtpStats) {
    const SAMPLE_RATE: f64 = 48000.0;
    let mut buf = [0u8; 2048];
    let mut last_seq: Option<u16> = None;
    let mut last_transit: Option<f64> = None;
    let mut jitter = 0.0f64; // in timestamp (sample) units
    let started = Instant::now();
    loop {
        let Ok((len, _)) = socket.recv_from(&mut buf) else { continue };
        let Some(packet) = rtp::parse(&buf[..len]) else { continue };
        stats.packets.fetch_add(1, Ordering::Relaxed);
        let peak = packet.samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        stats.peak_bits.fetch_max(peak.to_bits(), Ordering::Relaxed); // non-negative floats order as their bits

        if let Some(prev) = last_seq {
            let gap = packet.seq.wrapping_sub(prev);
            // 1 = in order; 2..~32k = that many - 1 lost; the huge
            // "gap" of a reordered (late) packet is not counted.
            if (2..32768).contains(&gap) {
                stats.lost.fetch_add((gap - 1) as u64, Ordering::Relaxed);
            }
        }
        last_seq = Some(packet.seq);

        // RFC 3550: transit = arrival (in timestamp units) − timestamp;
        // J += (|transit_i − transit_{i−1}| − J) / 16.
        let arrival = started.elapsed().as_secs_f64() * SAMPLE_RATE;
        let transit = arrival - packet.timestamp as f64;
        if let Some(prev) = last_transit {
            let d = (transit - prev).abs();
            jitter += (d - jitter) / 16.0;
            stats
                .jitter_ms_bits
                .store((jitter / SAMPLE_RATE * 1000.0).to_bits(), Ordering::Relaxed);
        }
        last_transit = Some(transit);
    }
}
