//! `simply-tussen fake-device` — the device-schema signals at 60 Hz without a
//! headset. Does not register; it only sends.

use std::net::UdpSocket;
use std::time::{Duration, Instant};

use tussen_hub::bundles;
use tussen_hub::fake::{FakeDevice, Pattern};

const RATE_HZ: f64 = 60.0;
const MAX_PACKET: usize = 1200;

pub fn run(name: &str, pattern: Pattern, hub: &str) -> anyhow::Result<()> {
    let hub_addr = super::resolve_hub(hub)?;
    let socket = UdpSocket::bind("0.0.0.0:0")?;
    let mut device = FakeDevice::new(name, pattern);
    eprintln!("fake device {name} → {hub_addr}, pattern {pattern:?}, {RATE_HZ} Hz (not registering)");

    let tick = Duration::from_secs_f64(1.0 / RATE_HZ);
    let mut next = Instant::now() + tick;
    let mut last_report = Instant::now();
    let mut sent = 0u64;
    loop {
        let messages = device.tick(1.0 / RATE_HZ);
        let encoded: Vec<Vec<u8>> =
            messages.iter().filter_map(|m| m.encode().ok()).collect();
        sent += encoded.len() as u64;
        for datagram in bundles::pack(&encoded, MAX_PACKET) {
            let _ = socket.send_to(&datagram, hub_addr);
        }

        let now = Instant::now();
        if now.duration_since(last_report) >= Duration::from_secs(1) {
            let sweeping = device
                .sweeping()
                .map(|s| format!(" · sweeping {s}"))
                .unwrap_or_default();
            eprintln!("{sent} msgs/s{sweeping}");
            sent = 0;
            last_report = now;
        }

        if let Some(wait) = next.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
        next += tick;
    }
}
