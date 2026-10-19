//! Hub discovery: a device with no hub address broadcasts
//! its hello; the hub answers every hello with `/tussen/hub/welcome`
//! (hub OSC port, device name) at the device's registered address, which
//! is how the device learns where the hub is. Static destinations given
//! by hostname are re-resolved periodically.

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use simply_osc::{Message, Value};
use tussen_hub::config::Config;
use tussen_hub::hub;

fn hello(name: &str, port: u16) -> Vec<u8> {
    Message::new("/tussen/hub/hello")
        .arg(Value::Str(name.into()))
        .arg(Value::Int32(port as i32))
        .arg(Value::Str("/ext/".into()))
        .arg(Value::Str("/tussen/hub/".into()))
        .arg(Value::Int32(0))
        .encode()
        .unwrap()
}

/// Wait for a welcome on `socket`: (sender, hub port arg, name arg).
fn recv_welcome(socket: &UdpSocket) -> Option<(SocketAddr, i32, String)> {
    recv_welcome_full(socket).map(|(from, port, name, _)| (from, port, name))
}

/// [`recv_welcome`] plus arg 2, TD's IPv4 ("" = no td destination).
fn recv_welcome_full(socket: &UdpSocket) -> Option<(SocketAddr, i32, String, String)> {
    socket.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
    let mut buf = [0u8; 2048];
    for _ in 0..4 {
        let (n, from) = socket.recv_from(&mut buf).ok()?;
        let Ok(packet) = simply_osc::wire::parse(&buf[..n]) else { continue };
        for (_, m) in packet.messages() {
            if m.address == "/tussen/hub/welcome" {
                let port = match m.args.first() {
                    Some(Value::Int32(p)) => *p,
                    other => panic!("welcome without a port: {other:?}"),
                };
                let name = match m.args.get(1) {
                    Some(Value::Str(s)) => s.clone(),
                    other => panic!("welcome without a name: {other:?}"),
                };
                let td = match m.args.get(2) {
                    Some(Value::Str(s)) => s.clone(),
                    other => panic!("welcome without TD's address: {other:?}"),
                };
                return Some((from, port, name, td));
            }
        }
    }
    None
}

fn spawn_hub() -> hub::Hub {
    spawn_hub_with("")
}

fn spawn_hub_with(extra: &str) -> hub::Hub {
    let config: Config = toml::from_str(&format!(
        "[osc]\nlisten_port = 0\nmax_packet = 1200\ndevice_timeout_ms = 3000\n{extra}"
    ))
    .unwrap();
    let (_tx, rx) = crossbeam_channel::bounded::<Message>(1);
    // The receiver outlives `_tx`'s drop: a closed generator channel is fine.
    hub::spawn(config, rx, true).unwrap()
}

#[test]
fn a_broadcast_hello_gets_a_welcome_carrying_the_hub_port() {
    let hub = spawn_hub();
    let port = hub.addr().port();

    // The device: one socket, as a device app's hub link does — it sends the hello
    // from its listen port and hears the welcome there. No hub address:
    // it broadcasts to 255.255.255.255.
    let device = UdpSocket::bind("0.0.0.0:0").unwrap();
    device.set_broadcast(true).unwrap();
    let my_port = device.local_addr().unwrap().port();
    let broadcast_ok = device.send_to(&hello("bcast", my_port), ("255.255.255.255", port)).is_ok();
    let got = if broadcast_ok { recv_welcome(&device) } else { None };
    let got = match got {
        Some(w) => w,
        None => {
            // No broadcast-capable interface (offline CI): the reply path
            // is the same for a unicast hello, so check that instead.
            eprintln!("broadcast hello not delivered here (sent: {broadcast_ok}); checking unicast");
            device.send_to(&hello("bcast", my_port), ("127.0.0.1", port)).unwrap();
            recv_welcome(&device).expect("no welcome for a unicast hello")
        }
    };
    let (from, hub_port, name) = got;
    assert_eq!(hub_port, port as i32, "welcome must carry the hub's OSC port");
    assert_eq!(name, "bcast");
    // The device talks to the replying IP at the welcome's port: that is
    // the hub (the reply itself leaves from an ephemeral send port).
    let hub_at = SocketAddr::new(from.ip(), hub_port as u16);
    let probe = UdpSocket::bind("0.0.0.0:0").unwrap();
    probe.send_to(&hello("probe", probe.local_addr().unwrap().port()), hub_at).unwrap();
    assert!(recv_welcome(&probe).is_some(), "hub not reachable at {hub_at}");

    drop(hub);
}

#[test]
fn the_welcome_carries_tds_address_for_ndi() {
    // No td destination: arg 2 is "".
    let hub = spawn_hub();
    let device = UdpSocket::bind("127.0.0.1:0").unwrap();
    let my_port = device.local_addr().unwrap().port();
    device.send_to(&hello("dev", my_port), ("127.0.0.1", hub.addr().port())).unwrap();
    let (_, _, _, td) = recv_welcome_full(&device).expect("no welcome");
    assert_eq!(td, "");
    drop(hub);

    // A td destination: its resolved IPv4, so the device can hand it to
    // NDI as an extra IP (no mDNS needed).
    let hub = spawn_hub_with(
        "[[dest]]\nname = \"td\"\nhost = \"10.1.2.3\"\nport = 9001\nsubscribe = [\"/ext/\"]\n",
    );
    device.send_to(&hello("dev", my_port), ("127.0.0.1", hub.addr().port())).unwrap();
    let (_, _, name, td) = recv_welcome_full(&device).expect("no welcome");
    assert_eq!(name, "dev");
    assert_eq!(td, "10.1.2.3");
    drop(hub);
}

#[test]
fn every_hello_is_answered_so_a_device_can_tell_the_hub_is_alive() {
    let hub = spawn_hub();
    let device = UdpSocket::bind("127.0.0.1:0").unwrap();
    let my_port = device.local_addr().unwrap().port();
    for _ in 0..3 {
        device.send_to(&hello("dev", my_port), ("127.0.0.1", hub.addr().port())).unwrap();
        assert!(recv_welcome(&device).is_some());
    }
    // A malformed hello registers nothing and gets no welcome.
    let bad = Message::new("/tussen/hub/hello").arg(Value::Str("x".into())).encode().unwrap();
    device.send_to(&bad, ("127.0.0.1", hub.addr().port())).unwrap();
    device.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
    let mut buf = [0u8; 512];
    assert!(device.recv_from(&mut buf).is_err(), "a bad hello was welcomed");
    drop(hub);
}

#[test]
fn the_resolver_re_resolves_hostnames_every_period() {
    // A fake resolver: the name moves from .20 to .42 on the third lookup
    // (a laptop's DHCP lease changing), the IP literal is never asked for.
    let calls = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let rx = hub::spawn_resolver(
        vec![("td".into(), "td-laptop.local".into(), 9001)],
        Duration::from_millis(30),
        Arc::clone(&stop),
        {
            let calls = Arc::clone(&calls);
            move |host, port| {
                assert_eq!(host, "td-laptop.local");
                let n = calls.fetch_add(1, Ordering::SeqCst);
                let ip = if n < 2 { [198, 51, 100, 20] } else { [198, 51, 100, 42] };
                Some(SocketAddr::from((ip, port)))
            }
        },
    );
    let mut seen = Vec::new();
    while seen.len() < 4 {
        let round = rx.recv_timeout(Duration::from_secs(2)).expect("resolver stalled");
        assert_eq!(round.len(), 1);
        assert_eq!(round[0].0, "td");
        seen.push(round[0].1.unwrap());
    }
    stop.store(true, Ordering::Relaxed);
    assert_eq!(seen[0], "198.51.100.20:9001".parse().unwrap());
    assert_eq!(seen[3], "198.51.100.42:9001".parse().unwrap());

    // No hostnames: no thread, nothing ever arrives.
    let none = hub::spawn_resolver(Vec::new(), Duration::from_millis(1), Arc::new(AtomicBool::new(false)), |_, _| None);
    assert!(none.recv_timeout(Duration::from_millis(50)).is_err());
}

#[test]
fn a_static_destination_named_by_host_receives_after_resolving() {
    // "localhost" is a name, not a literal: it goes through the same
    // resolve path a `.local` laptop does, preferring IPv4.
    let engine = simply_osc::Listener::bind("127.0.0.1:0").unwrap();
    let config: Config = toml::from_str(&format!(
        "[osc]\nlisten_port = 0\n[[dest]]\nname = \"engine\"\nhost = \"localhost\"\nport = {}\nsubscribe = [\"/ext/\"]\n",
        engine.local_addr().port()
    ))
    .unwrap();
    let (_tx, rx) = crossbeam_channel::bounded::<Message>(1);
    let hub = hub::spawn(config, rx, true).unwrap();
    let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
    let m = Message::new("/ext/x").arg(Value::Float32(1.0)).encode().unwrap();
    sender.send_to(&m, ("127.0.0.1", hub.addr().port())).unwrap();
    let got = engine.recv_timeout(Duration::from_millis(500)).expect("engine got nothing");
    assert!(got.packet.messages().iter().any(|(_, m)| m.address == "/ext/x"));
    drop(hub);
}

/// Live harness, env-gated: the real router loop on :9000 for
/// `TUSSEN_LIVE_ROUTER_SECS` seconds, with a static destination named by
/// this Mac's `.local` hostname (`TUSSEN_LIVE_DEST_HOST`), no generators
/// (no audio tap, no MIDI, no Link session joined). Pair it with a device
/// app's live discovery check on another machine:
///
///   TUSSEN_LIVE_ROUTER_SECS=60 TUSSEN_LIVE_DEST_HOST=$(scutil --get LocalHostName).local \
///     cargo test --release --test discovery live_router -- --nocapture
#[test]
fn live_router_on_9000() {
    let Some(secs) = std::env::var("TUSSEN_LIVE_ROUTER_SECS").ok().and_then(|s| s.parse::<u64>().ok()) else {
        return;
    };
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).try_init();
    let host = std::env::var("TUSSEN_LIVE_DEST_HOST").unwrap_or_else(|_| "localhost".into());
    let config: Config = toml::from_str(&format!(
        "[osc]\nlisten_port = 9000\ndevice_timeout_ms = 3000\n[[dest]]\nname = \"td\"\nhost = \"{host}\"\nport = 9001\nsubscribe = [\"/tussen/\"]\n"
    ))
    .unwrap();
    let (_tx, rx) = crossbeam_channel::bounded::<Message>(1);
    let hub = hub::spawn(config, rx, false).unwrap();
    eprintln!("live router on {} for {secs} s, td = {host}:9001", hub.addr());
    std::thread::sleep(Duration::from_secs(secs));
    drop(hub);
}
