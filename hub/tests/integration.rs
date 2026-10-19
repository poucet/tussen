//! In-process integration: fake-device → router → listeners.
//!
//! The hub runs on ephemeral ports via [`tussen_hub::hub::spawn`] — the
//! same loop the CLI runs. A static "engine"-shaped destination and a
//! registered device both listen; the fake device publishes; the routing
//! rules are then observed on real sockets.

use std::collections::HashSet;
use std::net::UdpSocket;
use std::time::{Duration, Instant};

use simply_osc::{Listener, Message, Value};
use tussen_hub::bundles;
use tussen_hub::config::Config;
use tussen_hub::fake::{FakeDevice, Pattern};
use tussen_hub::hub;

/// Collect every address a listener receives within `window`.
fn drain(listener: &Listener, window: Duration) -> Vec<String> {
    let mut out = Vec::new();
    let deadline = Instant::now() + window;
    while let Some(wait) = deadline.checked_duration_since(Instant::now()) {
        if let Some(inbound) = listener.recv_timeout(wait) {
            for (_, m) in inbound.packet.messages() {
                out.push(m.address.clone());
            }
        }
    }
    out
}

#[test]
fn fake_device_signals_route_to_subscribers_but_never_back_to_their_owner() {
    // An "engine"-shaped static destination on an ephemeral port.
    let engine = Listener::bind("127.0.0.1:0").unwrap();
    let config: Config = toml::from_str(&format!(
        r#"
        [osc]
        listen_port = 0
        max_packet = 1200
        device_timeout_ms = 3000

        [[dest]]
        name = "engine"
        host = "127.0.0.1"
        port = {}
        subscribe = ["/tussen/", "/ext/"]
        exclude = ["/tussen/midi/", "/tussen/clock/", "/tussen/hub/"]
        "#,
        engine.local_addr().port()
    ))
    .unwrap();

    let (_tx, rx) = crossbeam_channel::bounded::<Message>(1);
    let hub = hub::spawn(config, rx, true).unwrap();
    let hub_addr = ("127.0.0.1", hub.addr().port());

    // A device named "quest" registers, subscribed to everything under
    // /tussen/ and /ext/ — the ownership rule is what must keep its own
    // signals out.
    let quest = Listener::bind("127.0.0.1:0").unwrap();
    let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
    let hello = Message::new("/tussen/hub/hello")
        .arg(Value::Str("quest".into()))
        .arg(Value::Int32(quest.local_addr().port() as i32))
        .arg(Value::Str("/tussen/,/ext/,/cmd/quest/".into()))
        .arg(Value::Str("/tussen/hub/".into()))
        .arg(Value::Int32(0))
        .encode()
        .unwrap();
    sender.send_to(&hello, hub_addr).unwrap();
    std::thread::sleep(Duration::from_millis(100)); // let the hello land

    // The fake device publishes a few 60 Hz ticks, exactly as the CLI
    // does: encoded, packed at 1200, sent to the hub.
    let mut fake = FakeDevice::new("quest", Pattern::Sweep);
    for _ in 0..6 {
        let encoded: Vec<Vec<u8>> =
            fake.tick(1.0 / 60.0).iter().map(|m| m.encode().unwrap()).collect();
        for datagram in bundles::pack(&encoded, 1200) {
            sender.send_to(&datagram, hub_addr).unwrap();
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    // Plus signals the quest device should receive: another device's and
    // an /ext/ source's — brand-new addresses, no config for them.
    for address in ["/tussen/phone/head/yaw", "/ext/test/x", "/cmd/quest/recenter"] {
        let m = Message::new(address).arg(Value::Float32(0.5)).encode().unwrap();
        sender.send_to(&m, hub_addr).unwrap();
    }

    let engine_got: HashSet<String> = drain(&engine, Duration::from_millis(400)).into_iter().collect();
    let quest_got: HashSet<String> = drain(&quest, Duration::from_millis(100)).into_iter().collect();

    // The engine sees the fake device's signals and the /ext/ source...
    assert!(engine_got.contains("/tussen/quest/head/n/yaw"), "engine missing head: {engine_got:?}");
    assert!(engine_got.contains("/tussen/quest/controller/trigger"));
    assert!(engine_got.contains("/tussen/quest/seq"));
    assert!(engine_got.contains("/tussen/phone/head/yaw"));
    assert!(engine_got.contains("/ext/test/x"));
    // ...but not the registration traffic it excludes, nor /cmd/.
    assert!(!engine_got.contains("/tussen/hub/hello"));
    assert!(!engine_got.contains("/cmd/quest/recenter"));

    // The registered quest device never gets its own prefix back —
    assert!(
        quest_got.iter().all(|a| !a.starts_with("/tussen/quest/")),
        "ownership violated: {quest_got:?}"
    );
    // — but does get the other device, the /ext/ source, and its command.
    assert!(quest_got.contains("/tussen/phone/head/yaw"), "quest missing phone: {quest_got:?}");
    assert!(quest_got.contains("/ext/test/x"));
    assert!(quest_got.contains("/cmd/quest/recenter"));

    drop(hub);
}

#[test]
fn a_device_that_goes_silent_is_forgotten_and_stops_receiving() {
    let config: Config = toml::from_str(
        "[osc]\nlisten_port = 0\nmax_packet = 1200\ndevice_timeout_ms = 300\n",
    )
    .unwrap();
    let (_tx, rx) = crossbeam_channel::bounded::<Message>(1);
    let hub = hub::spawn(config, rx, true).unwrap();
    let hub_addr = ("127.0.0.1", hub.addr().port());

    let device = Listener::bind("127.0.0.1:0").unwrap();
    let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
    let hello = Message::new("/tussen/hub/hello")
        .arg(Value::Str("test".into()))
        .arg(Value::Int32(device.local_addr().port() as i32))
        .arg(Value::Str("/ext/".into()))
        .encode()
        .unwrap();
    sender.send_to(&hello, hub_addr).unwrap();
    std::thread::sleep(Duration::from_millis(100));

    let probe = Message::new("/ext/a").arg(Value::Float32(1.0)).encode().unwrap();
    sender.send_to(&probe, hub_addr).unwrap();
    assert!(
        drain(&device, Duration::from_millis(300)).contains(&"/ext/a".to_string()),
        "registered device should receive /ext/a"
    );

    // Silence past the 300 ms timeout: forgotten, nothing arrives.
    std::thread::sleep(Duration::from_millis(500));
    sender.send_to(&probe, hub_addr).unwrap();
    assert!(
        drain(&device, Duration::from_millis(300)).is_empty(),
        "timed-out device still receiving"
    );

    drop(hub);
}

#[test]
fn malformed_packets_are_dropped_and_the_hub_keeps_routing() {
    let engine = Listener::bind("127.0.0.1:0").unwrap();
    let config: Config = toml::from_str(&format!(
        "[osc]\nlisten_port = 0\n[[dest]]\nname = \"engine\"\nhost = \"127.0.0.1\"\nport = {}\nsubscribe = [\"/\"]\n",
        engine.local_addr().port()
    ))
    .unwrap();
    let (_tx, rx) = crossbeam_channel::bounded::<Message>(1);
    let hub = hub::spawn(config, rx, true).unwrap();
    let hub_addr = ("127.0.0.1", hub.addr().port());
    let sender = UdpSocket::bind("127.0.0.1:0").unwrap();

    // Garbage, a truncated message, a bad hello — then a good signal.
    sender.send_to(b"not osc at all", hub_addr).unwrap();
    sender.send_to(b"/a", hub_addr).unwrap();
    let bad_hello = Message::new("/tussen/hub/hello").arg(Value::Int32(42)).encode().unwrap();
    sender.send_to(&bad_hello, hub_addr).unwrap();
    let good = Message::new("/ext/alive").arg(Value::Float32(1.0)).encode().unwrap();
    sender.send_to(&good, hub_addr).unwrap();

    assert!(
        drain(&engine, Duration::from_millis(400)).contains(&"/ext/alive".to_string()),
        "hub should survive malformed input and keep routing"
    );
    drop(hub);
}

#[test]
fn generator_messages_route_like_received_signals() {
    // The registration point works end to end: a message pushed into the
    // generator channel reaches a subscriber of its prefix.
    let td = Listener::bind("127.0.0.1:0").unwrap();
    let config: Config = toml::from_str(&format!(
        "[osc]\nlisten_port = 0\n[[dest]]\nname = \"td\"\nhost = \"127.0.0.1\"\nport = {}\nsubscribe = [\"/tussen/\"]\nexclude = [\"/tussen/hub/\"]\n",
        td.local_addr().port()
    ))
    .unwrap();
    let (tx, rx) = crossbeam_channel::bounded::<Message>(16);
    let hub = hub::spawn(config, rx, true).unwrap();

    tx.send(Message::new("/tussen/clock/beat").arg(Value::Float32(0.4))).unwrap();
    tx.send(Message::new("/tussen/midi/cc/74").arg(Value::Float32(0.6))).unwrap();

    let got: HashSet<String> = drain(&td, Duration::from_millis(400)).into_iter().collect();
    assert!(got.contains("/tussen/clock/beat"), "{got:?}");
    assert!(got.contains("/tussen/midi/cc/74"));
    drop(hub);
}
