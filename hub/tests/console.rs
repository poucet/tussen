//! Sync, remap and the console protocol, end to end over real
//! sockets: the hub runs in-process on ephemeral ports, destinations are
//! plain UDP sockets timestamping every arrival on their own thread, and
//! the console is driven by a tungstenite client exactly as the page
//! drives it.

use std::net::{SocketAddr, TcpStream, UdpSocket};
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use serde_json::{json, Value as Json};
use simply_osc::{Message, Value};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::WebSocket;
use tussen_hub::config::Config;
use tussen_hub::config_file::{ConfigFile, Source};
use tussen_hub::console::{self, ConsoleOptions};
use tussen_hub::hub::{self, Extras};
use tussen_hub::sync::SyncState;

/// A destination socket that timestamps every message the moment its
/// datagram arrives.
struct Recorder {
    port: u16,
    rx: Receiver<(Instant, Message)>,
}

impl Recorder {
    fn new() -> Recorder {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
        let port = socket.local_addr().unwrap().port();
        let (tx, rx) = crossbeam_channel::unbounded();
        std::thread::spawn(move || {
            let mut buf = [0u8; 2048];
            loop {
                if let Ok((n, _)) = socket.recv_from(&mut buf) {
                    let at = Instant::now();
                    if let Ok(p) = simply_osc::wire::parse(&buf[..n]) {
                        for (_, m) in p.messages() {
                            if tx.send((at, m.clone())).is_err() {
                                return;
                            }
                        }
                    }
                }
            }
        });
        Recorder { port, rx }
    }

    fn drain(&self, window: Duration) -> Vec<(Instant, Message)> {
        let deadline = Instant::now() + window;
        let mut out = Vec::new();
        while let Some(wait) = deadline.checked_duration_since(Instant::now()) {
            if let Ok(x) = self.rx.recv_timeout(wait) {
                out.push(x);
            }
        }
        out
    }
}

fn first_int(m: &Message) -> Option<i32> {
    match m.args.first() {
        Some(Value::Int32(i)) => Some(*i),
        _ => None,
    }
}

fn hello_bytes(name: &str, port: u16, subscribe: &str) -> Vec<u8> {
    Message::new("/tussen/hub/hello")
        .arg(Value::Str(name.into()))
        .arg(Value::Int32(port as i32))
        .arg(Value::Str(subscribe.into()))
        .arg(Value::Str("/tussen/hub/".into()))
        .arg(Value::Int32(0))
        .encode()
        .unwrap()
}

/// Per-probe lateness against the expected arrival, in ms.
fn lateness(sent: &[Instant], got: &[(Instant, Message)], addr: &str, delay_ms: u64) -> Vec<f64> {
    got.iter()
        .filter(|(_, m)| m.address == addr)
        .filter_map(|(at, m)| first_int(m).map(|i| (at, i as usize)))
        .map(|(at, i)| {
            let due = sent[i] + Duration::from_millis(delay_ms);
            if *at >= due {
                at.duration_since(due).as_secs_f64() * 1e3
            } else {
                -(due.duration_since(*at).as_secs_f64() * 1e3)
            }
        })
        .collect()
}

fn summary(label: &str, v: &[f64]) -> (f64, f64) {
    let min = v.iter().cloned().fold(f64::INFINITY, f64::min);
    let max = v.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let mean = v.iter().sum::<f64>() / v.len() as f64;
    eprintln!("{label}: n={} lateness min={min:.3} mean={mean:.3} max={max:.3} ms", v.len());
    (min, max)
}

#[test]
fn the_two_sync_points_hold_each_destination_and_the_engine_is_never_delayed() {
    let engine = Recorder::new();
    let td = Recorder::new();
    let monitor = Recorder::new();
    let quest = Recorder::new();
    let phone = Recorder::new();
    let config: Config = toml::from_str(&format!(
        r#"
        [osc]
        listen_port = 0
        [[dest]]
        name = "engine"
        host = "127.0.0.1"
        port = {}
        subscribe = ["/ext/", "/tussen/midi/"]
        [[dest]]
        name = "td"
        host = "127.0.0.1"
        port = {}
        subscribe = ["/ext/", "/tussen/"]
        exclude = ["/tussen/hub/"]
        [[dest]]
        name = "monitor"
        host = "127.0.0.1"
        port = {}
        subscribe = ["/ext/"]

        [sync]
        signal_delay_ms = 50        # sync point 1: td and every device
        td_round_trip_ms = 30       # sync point 2: devices that follow td
        [sync.device.phone]
        follow = "music"

        [[remap]]
        dest = "td"
        from = "/ext/*/r"
        to = "/td/r"
        range = [10.0, 20.0]
        "#,
        engine.port, td.port, monitor.port
    ))
    .unwrap();
    let (gen_tx, gen_rx) = crossbeam_channel::bounded::<Message>(64);
    let hub = hub::spawn(config, gen_rx, true).unwrap();
    let hub_addr = SocketAddr::from(([127, 0, 0, 1], hub.addr().port()));
    let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
    sender.send_to(&hello_bytes("quest", quest.port, "/ext/,/tussen/midi/"), hub_addr).unwrap();
    sender.send_to(&hello_bytes("phone", phone.port, "/ext/,/tussen/midi/"), hub_addr).unwrap();
    std::thread::sleep(Duration::from_millis(100));

    // OSC probes over the network and MIDI probes from a generator,
    // interleaved, 7 ms apart (so delayed and immediate traffic overlap).
    const N: usize = 40;
    let mut osc_sent = Vec::new();
    let mut midi_sent = Vec::new();
    for i in 0..N {
        let m = Message::new("/ext/probe").arg(Value::Int32(i as i32)).encode().unwrap();
        osc_sent.push(Instant::now());
        sender.send_to(&m, hub_addr).unwrap();
        midi_sent.push(Instant::now());
        gen_tx.send(Message::new("/tussen/midi/cc/1").arg(Value::Int32(i as i32))).unwrap();
        std::thread::sleep(Duration::from_millis(7));
    }
    // One remap probe: td gets /td/r mapped and delayed; nobody else is
    // affected by td's rule.
    let remap_sent = Instant::now();
    sender
        .send_to(&Message::new("/ext/fader/r").arg(Value::Float32(0.5)).encode().unwrap(), hub_addr)
        .unwrap();

    let window = Duration::from_millis(400);
    let e = engine.drain(window);
    let (t, mo) = (td.drain(Duration::from_millis(50)), monitor.drain(Duration::from_millis(10)));
    let (q, ph) = (quest.drain(Duration::from_millis(50)), phone.drain(Duration::from_millis(10)));

    // Engine 0; monitor 0; TD = signal delay; a device = signal + round
    // trip; a device following the music = signal delay. MIDI and OSC alike.
    let cases = [
        ("engine osc 0ms", lateness(&osc_sent, &e, "/ext/probe", 0)),
        ("engine midi 0ms", lateness(&midi_sent, &e, "/tussen/midi/cc/1", 0)),
        ("monitor osc 0ms", lateness(&osc_sent, &mo, "/ext/probe", 0)),
        ("td osc 50ms", lateness(&osc_sent, &t, "/ext/probe", 50)),
        ("td midi 50ms", lateness(&midi_sent, &t, "/tussen/midi/cc/1", 50)),
        ("quest osc 50+30ms", lateness(&osc_sent, &q, "/ext/probe", 80)),
        ("quest midi 50+30ms", lateness(&midi_sent, &q, "/tussen/midi/cc/1", 80)),
        ("phone osc 50ms (music)", lateness(&osc_sent, &ph, "/ext/probe", 50)),
        ("phone midi 50ms (music)", lateness(&midi_sent, &ph, "/tussen/midi/cc/1", 50)),
    ];
    for (label, v) in &cases {
        assert_eq!(v.len(), N, "{label}: lost probes");
        let (min, max) = summary(label, v);
        let undelayed = label.starts_with("engine") || label.starts_with("monitor");
        assert!(min >= 0.0 || undelayed, "{label}: early by {:.3} ms", -min);
        // Typically ~0.2 ms; 5 ms leaves headroom for a loaded machine (2 ms flaked).
        assert!(max <= 5.0, "{label}: late by {max:.3} ms");
    }

    // Remap → delay: td sees only the mapped signal, ≥ 50 ms later.
    let mapped: Vec<_> = t.iter().filter(|(_, m)| m.address == "/td/r").collect();
    assert_eq!(mapped.len(), 1, "td remap: {:?}", t.iter().map(|(_, m)| &m.address).collect::<Vec<_>>());
    assert_eq!(mapped[0].1.args, vec![Value::Float32(15.0)]);
    let late = mapped[0].0.duration_since(remap_sent);
    assert!(late >= Duration::from_millis(50) && late <= Duration::from_millis(52), "remap delay {late:?}");
    assert!(!t.iter().any(|(_, m)| m.address == "/ext/fader/r"), "keep=false must replace");
    // Per-destination: the engine and the device get the original.
    assert!(e.iter().any(|(_, m)| m.address == "/ext/fader/r"));
    assert!(q.iter().any(|(_, m)| m.address == "/ext/fader/r"));
    assert!(ph.iter().any(|(_, m)| m.address == "/ext/fader/r"));
    assert!(!e.iter().chain(q.iter()).any(|(_, m)| m.address == "/td/r"));
    drop(hub);
}

type Ws = WebSocket<MaybeTlsStream<TcpStream>>;

fn connect(addr: SocketAddr) -> Ws {
    connect_as(addr, "/ws", None)
}

/// Connect like a browser page would: `origin` as the `Origin` header
/// (None = a non-browser client), `path` with any query.
fn connect_as(addr: SocketAddr, path: &str, origin: Option<&str>) -> Ws {
    use tungstenite::client::IntoClientRequest;
    let mut req = format!("ws://{addr}{path}").into_client_request().unwrap();
    if let Some(o) = origin {
        req.headers_mut().insert("Origin", o.parse().unwrap());
    }
    let (mut ws, _) = tungstenite::connect(req).unwrap();
    if let MaybeTlsStream::Plain(s) = ws.get_mut() {
        s.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
    }
    ws
}

/// Drain frames for `window`, failing if any passes `bad`.
fn expect_never(ws: &mut Ws, window: Duration, label: &str, bad: impl Fn(&Json) -> bool) {
    let deadline = Instant::now() + window;
    while Instant::now() < deadline {
        match ws.read() {
            Ok(tungstenite::Message::Text(t)) => {
                let v: Json = serde_json::from_str(t.as_str()).unwrap();
                assert!(!bad(&v), "{label}: {v}");
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
            Err(e) => panic!("websocket: {e}"),
        }
    }
}

/// Read frames until one with `t == want` passes `pred`, or time out.
fn expect(ws: &mut Ws, want: &str, within: Duration, pred: impl Fn(&Json) -> bool) -> Json {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        match ws.read() {
            Ok(tungstenite::Message::Text(t)) => {
                let v: Json = serde_json::from_str(t.as_str()).unwrap();
                if v["t"] == want && pred(&v) {
                    return v;
                }
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
            Err(e) => panic!("websocket: {e}"),
        }
    }
    panic!("no {want} within {within:?}");
}

fn send(ws: &mut Ws, v: Json) {
    ws.send(tungstenite::Message::text(v.to_string())).unwrap();
}

#[test]
fn the_websocket_protocol_drives_live_routing() {
    let engine = Recorder::new();
    let td = Recorder::new();
    let dir = std::env::temp_dir().join(format!("tussen-console-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("hub.toml");
    std::fs::write(
        &path,
        format!(
            r#"# my stage — keep this comment
[osc]
listen_port = 0

[[dest]]
name = "engine"
host = "127.0.0.1"
port = {}
subscribe = ["/ext/"]

[[dest]]
name = "td"
host = "127.0.0.1"
port = {}
subscribe = ["/ext/", "/tussen/"]
exclude = ["/tussen/hub/"]

[console]
port = 0
"#,
            engine.port, td.port
        ),
    )
    .unwrap();
    let config = Config::load(&path).unwrap();

    let (console, router_side) = console::start(ConsoleOptions {
        port: 0,
        osc_port: 9000,
        config: Some(ConfigFile { path: path.clone(), source: Source::Flag, user_path: None }),
        statics: config.dests.iter().map(|d| (d.name.clone(), d.kind())).collect(),
        sample_rate: 48000,
        state: SyncState::new(config.sync.clone(), config.remap.clone()),
    })
    .unwrap();
    let (gen_tx, gen_rx) = crossbeam_channel::bounded::<Message>(64);
    let hub = hub::spawn_with(config, gen_rx, true, Extras { audio: None, console: Some(router_side) }).unwrap();
    let hub_addr = SocketAddr::from(([127, 0, 0, 1], hub.addr().port()));
    let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
    let probe = |addr: &str| {
        sender.send_to(&Message::new(addr).arg(Value::Float32(0.25)).encode().unwrap(), hub_addr).unwrap();
    };

    // The page is served at /.
    {
        use std::io::{Read, Write};
        let mut s = TcpStream::connect(console.addr).unwrap();
        s.write_all(b"GET /?mock=1 HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        let mut body = String::new();
        s.read_to_string(&mut body).unwrap();
        assert!(body.starts_with("HTTP/1.1 200 OK"), "{body}");
        assert!(body.contains("<html"));
    }
    // A foreign origin can't open the socket.
    {
        use tungstenite::client::IntoClientRequest;
        let mut req = format!("ws://{}/ws", console.addr).into_client_request().unwrap();
        req.headers_mut().insert("Origin", "http://evil.example".parse().unwrap());
        assert!(tungstenite::connect(req).is_err());
    }

    let mut ws = connect(console.addr);
    let hello = expect(&mut ws, "hello", Duration::from_secs(2), |_| true);
    assert_eq!(hello["dests"], json!([{"name":"engine","kind":"engine"}, {"name":"td","kind":"td"}]));
    assert_eq!(hello["sync"], json!({"signal_delay_ms":0,"td_round_trip_ms":0}));
    assert_eq!(hello["remaps"], json!([]));
    assert_eq!(hello["sample_rate"], 48000);
    // The OSC endpoint for the copy button: the configured port, LAN IPs
    // (possibly empty on a routeless machine, never loopback), hostname.
    assert_eq!(hello["osc"]["port"], 9000);
    let ips = hello["osc"]["ips"].as_array().expect("osc.ips");
    assert!(ips.iter().all(|ip| !ip.as_str().unwrap().starts_with("127.")), "{ips:?}");

    // A device registering shows up in `dests`.
    let quest = Recorder::new();
    sender.send_to(&hello_bytes("quest", quest.port, "/ext/"), hub_addr).unwrap();
    expect(&mut ws, "dests", Duration::from_secs(2), |v| {
        v["dests"][2] == json!({"name":"quest","kind":"device","follow":"td"})
    });

    // set_sync: a fresh hello carries it; td's OSC is now 60 ms late and
    // the device's 60 + 25.
    send(&mut ws, json!({"t":"set_sync","signal_delay_ms":60}));
    expect(&mut ws, "hello", Duration::from_secs(2), |v| v["sync"]["signal_delay_ms"] == 60);
    send(&mut ws, json!({"t":"set_sync","td_round_trip_ms":25}));
    expect(&mut ws, "hello", Duration::from_secs(2), |v| {
        v["sync"] == json!({"signal_delay_ms":60,"td_round_trip_ms":25})
    });
    // Only devices have the follow switch: one for the engine is refused
    // and the hello that follows says so.
    send(&mut ws, json!({"t":"set_follow","dest":"engine","follow":"music"}));
    expect(&mut ws, "hello", Duration::from_secs(2), |v| v["dests"][0] == json!({"name":"engine","kind":"engine"}));
    std::thread::sleep(Duration::from_millis(50)); // the swap reaches the router
    let drain_all = || {
        engine.drain(Duration::from_millis(10));
        td.drain(Duration::from_millis(10));
        quest.drain(Duration::from_millis(10));
    };
    drain_all();
    let t0 = Instant::now();
    probe("/ext/x");
    let te = engine.drain(Duration::from_millis(150));
    let tt = td.drain(Duration::from_millis(10));
    let tq = quest.drain(Duration::from_millis(10));
    let at = |got: &[(Instant, Message)], a: &str| got.iter().find(|(_, m)| m.address == a).map(|(t, _)| t.duration_since(t0));
    let engine_late = at(&te, "/ext/x").expect("engine got /ext/x");
    let td_late = at(&tt, "/ext/x").expect("td got /ext/x");
    let quest_late = at(&tq, "/ext/x").expect("quest got /ext/x");
    eprintln!("ws set_sync: engine after {engine_late:?}, td after {td_late:?}, quest after {quest_late:?} (want 0 / 60 / 85 ms)");
    assert!(engine_late < Duration::from_millis(5));
    assert!(td_late >= Duration::from_millis(60) && td_late <= Duration::from_millis(62));
    assert!(quest_late >= Duration::from_millis(85) && quest_late <= Duration::from_millis(87));

    // set_follow music: the device drops the round trip.
    send(&mut ws, json!({"t":"set_follow","dest":"quest","follow":"music"}));
    expect(&mut ws, "hello", Duration::from_secs(2), |v| v["dests"][2]["follow"] == "music");
    std::thread::sleep(Duration::from_millis(50));
    drain_all();
    let t0 = Instant::now();
    probe("/ext/x");
    let tq = quest.drain(Duration::from_millis(150));
    let quest_late = tq.iter().find(|(_, m)| m.address == "/ext/x").map(|(t, _)| t.duration_since(t0)).expect("quest got /ext/x");
    assert!(quest_late >= Duration::from_millis(60) && quest_late <= Duration::from_millis(62), "{quest_late:?}");

    // remap_upsert: hello comes back with the rule and its id; td gets the
    // new name, the engine still the old one.
    send(&mut ws, json!({"t":"remap_upsert","dest":"td","from":"/ext/y","to":"/td/y","keep":false}));
    let h = expect(&mut ws, "hello", Duration::from_secs(2), |v| v["remaps"].as_array().is_some_and(|a| a.len() == 1));
    let id = h["remaps"][0]["id"].as_u64().unwrap();
    assert!(id > 0);
    assert_eq!(h["remaps"][0]["to"], "/td/y");
    std::thread::sleep(Duration::from_millis(50));
    probe("/ext/y");
    let te = engine.drain(Duration::from_millis(150));
    let tt = td.drain(Duration::from_millis(10));
    assert!(at(&te, "/ext/y").is_some());
    assert!(at(&tt, "/td/y").is_some(), "td: {:?}", tt.iter().map(|(_, m)| &m.address).collect::<Vec<_>>());
    assert!(at(&tt, "/ext/y").is_none());

    // watch: only watched OSC addresses reach the page as `osc`.
    send(&mut ws, json!({"t":"watch","addrs":["/ext/w"]}));
    std::thread::sleep(Duration::from_millis(50));
    probe("/ext/unwatched");
    probe("/ext/w");
    let o = expect(&mut ws, "osc", Duration::from_secs(2), |_| true);
    assert_eq!(o["addr"], "/ext/w");
    assert_eq!(o["v"], 0.25);
    assert!(o["src"].as_str().unwrap().starts_with("127.0.0.1:"));
    assert!(o["ts"].as_u64().unwrap() > 0);

    // watch "*": every OSC message reaches the page (the history tab).
    send(&mut ws, json!({"t":"watch","addrs":["*"]}));
    std::thread::sleep(Duration::from_millis(50));
    probe("/ext/anything/at/all");
    let o = expect(&mut ws, "osc", Duration::from_secs(2), |v| v["addr"] == "/ext/anything/at/all");
    assert_eq!(o["v"], 0.25);

    // Every MIDI signal reaches the page.
    gen_tx.send(Message::new("/tussen/midi/cc/7").arg(Value::Float32(0.5))).unwrap();
    let m = expect(&mut ws, "midi", Duration::from_secs(2), |v| v["addr"] == "/tussen/midi/cc/7");
    assert_eq!(m["v"], 0.5);

    // addrs: the seen table, newest first, at ~2 Hz.
    let a = expect(&mut ws, "addrs", Duration::from_secs(2), |v| {
        v["list"].as_array().is_some_and(|l| l.iter().any(|e| e["addr"] == "/ext/w"))
    });
    let list = a["list"].as_array().unwrap();
    assert!(list.iter().all(|e| e["last_ts"].is_u64() && e["rate"].is_number()));
    // Every entry carries its last value, min/max and total count (the
    // history tab's "all addresses" table); /ext/w was probed with 0.25,
    // so its first value initialises min and max too.
    assert!(
        list.iter().all(|e| e["v"].is_number()
            && e["min"].is_number()
            && e["max"].is_number()
            && e["count"].as_u64().unwrap() >= 1),
        "{list:?}"
    );
    let w = list.iter().find(|e| e["addr"] == "/ext/w").unwrap();
    assert_eq!(w["v"], 0.25);
    assert_eq!((w["min"].as_f64(), w["max"].as_f64()), (Some(0.25), Some(0.25)));
    // Direction: inbound addresses are "in"; what the hub itself emits is
    // "out" — generated signals and the remap rule's output (/td/y never
    // appears inbound, only at delivery).
    assert_eq!(w["dir"], "in");
    let dir = |a: &str| list.iter().find(|e| e["addr"] == a).map(|e| e["dir"].clone());
    assert_eq!(dir("/td/y"), Some(json!("out")), "remapped address in the set: {list:?}");
    assert_eq!(dir("/tussen/midi/cc/7"), Some(json!("out")), "generated signal: {list:?}");
    assert!(!list.iter().any(|e| e["addr"].as_str().unwrap().starts_with("/tussen/hub/")));
    // The list is permanent and sorted by address, so it never reorders
    // under traffic (addresses used to vanish past a cap and the page
    // reshuffled on every snapshot).
    let addrs: Vec<&str> = list.iter().map(|e| e["addr"].as_str().unwrap()).collect();
    let mut sorted = addrs.clone();
    sorted.sort_unstable();
    assert_eq!(addrs, sorted, "not address-sorted: {addrs:?}");

    // save: the file gets [sync] + [[remap]], its comment survives, and a
    // reload yields the live state.
    send(&mut ws, json!({"t":"save"}));
    let s = expect(&mut ws, "saved", Duration::from_secs(2), |_| true);
    assert_eq!(s["ok"], true, "{s}");
    assert!(s["path"].as_str().unwrap().ends_with("hub.toml"));
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.starts_with("# my stage — keep this comment"), "{text}");
    let back = Config::load(&path).unwrap();
    assert_eq!(back.sync.signal_delay_ms, 60);
    assert_eq!(back.sync.td_round_trip_ms, 25);
    assert_eq!(back.sync.device["quest"].follow, tussen_hub::sync::Follow::Music);
    assert!(!back.sync.device.contains_key("engine"));
    assert_eq!(back.remap.len(), 1);
    assert_eq!(back.remap[0].id, id);
    assert_eq!(back.remap[0].to, "/td/y");

    // remap_delete: the rule is gone from hello and from routing.
    send(&mut ws, json!({"t":"remap_delete","id":id}));
    expect(&mut ws, "hello", Duration::from_secs(2), |v| v["remaps"] == json!([]));
    std::thread::sleep(Duration::from_millis(50));
    probe("/ext/y");
    let tt = td.drain(Duration::from_millis(150));
    assert!(at(&tt, "/ext/y").is_some());

    drop(ws);
    drop(hub);
    drop(console);
}

/// The signals feed for external pages (docs/guides/signals-websocket.md):
/// a local page from another origin — a shader sketchpad on its own dev
/// server — reads everything next to the console without starving it, and
/// cannot command; `?readonly=1` gives a no-Origin client the same guard.
#[test]
fn a_second_local_page_reads_the_feed_but_cannot_command() {
    let engine = Recorder::new();
    let config: Config = toml::from_str(&format!(
        r#"
        [osc]
        listen_port = 0
        [[dest]]
        name = "engine"
        host = "127.0.0.1"
        port = {}
        subscribe = ["/ext/"]
        "#,
        engine.port
    ))
    .unwrap();
    let (console, router_side) = console::start(ConsoleOptions {
        port: 0,
        osc_port: 9000,
        config: None,
        statics: config.dests.iter().map(|d| (d.name.clone(), d.kind())).collect(),
        sample_rate: 48000,
        state: SyncState::new(config.sync.clone(), config.remap.clone()),
    })
    .unwrap();
    let (_gen_tx, gen_rx) = crossbeam_channel::bounded::<Message>(64);
    let hub = hub::spawn_with(config, gen_rx, true, Extras { audio: None, console: Some(router_side) }).unwrap();
    let hub_addr = SocketAddr::from(([127, 0, 0, 1], hub.addr().port()));
    let sender = UdpSocket::bind("127.0.0.1:0").unwrap();

    // The console page (same “origin”: none) and a shader page served by
    // a dev server on another local port.
    let mut console_page = connect(console.addr);
    let mut shader_page = connect_as(console.addr, "/ws", Some("http://127.0.0.1:8787"));
    expect(&mut console_page, "hello", Duration::from_secs(2), |_| true);
    // The read-only page gets the same hello — including the OSC endpoint.
    let h = expect(&mut shader_page, "hello", Duration::from_secs(2), |_| true);
    assert_eq!(h["osc"]["port"], 9000);

    send(&mut console_page, json!({"t":"watch","addrs":["*"]}));
    send(&mut shader_page, json!({"t":"watch","addrs":["*"]}));
    std::thread::sleep(Duration::from_millis(50));

    // Two simultaneous subscribers: every probe reaches both, so neither
    // starves the other.
    const N: usize = 40;
    for i in 0..N {
        let m = Message::new("/ext/smoke").arg(Value::Int32(i as i32)).encode().unwrap();
        sender.send_to(&m, hub_addr).unwrap();
        std::thread::sleep(Duration::from_millis(2));
    }
    let count = |ws: &mut Ws| {
        let mut seen = std::collections::HashSet::new();
        let deadline = Instant::now() + Duration::from_secs(2);
        while seen.len() < N && Instant::now() < deadline {
            match ws.read() {
                Ok(tungstenite::Message::Text(t)) => {
                    let v: Json = serde_json::from_str(t.as_str()).unwrap();
                    if v["t"] == "osc" && v["addr"] == "/ext/smoke" {
                        seen.insert(v["v"].as_f64().unwrap() as i64);
                    }
                }
                Ok(_) => {}
                Err(tungstenite::Error::Io(e))
                    if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                Err(e) => panic!("websocket: {e}"),
            }
        }
        seen.len()
    };
    assert_eq!(count(&mut console_page), N, "the console page lost probes");
    assert_eq!(count(&mut shader_page), N, "the shader page lost probes");

    // The read-only page's commands are ignored: no hello announces its
    // sync change, on either socket…
    send(&mut shader_page, json!({"t":"set_sync","signal_delay_ms":99}));
    expect_never(&mut shader_page, Duration::from_millis(300), "read-only set_sync applied", |v| {
        v["t"] == "hello" && v["sync"]["signal_delay_ms"] == 99
    });
    expect_never(&mut console_page, Duration::from_millis(100), "read-only set_sync applied", |v| {
        v["t"] == "hello" && v["sync"]["signal_delay_ms"] == 99
    });
    // …while the console page's commands still work, and the read-only
    // page sees the resulting hello (it reads everything).
    send(&mut console_page, json!({"t":"set_sync","signal_delay_ms":60}));
    expect(&mut console_page, "hello", Duration::from_secs(2), |v| v["sync"]["signal_delay_ms"] == 60);
    expect(&mut shader_page, "hello", Duration::from_secs(2), |v| v["sync"]["signal_delay_ms"] == 60);

    // ?readonly=1: the same guard for a client that carries no Origin.
    let mut opted = connect_as(console.addr, "/ws?readonly=1", None);
    expect(&mut opted, "hello", Duration::from_secs(2), |_| true);
    send(&mut opted, json!({"t":"set_sync","signal_delay_ms":77}));
    expect_never(&mut opted, Duration::from_millis(300), "?readonly=1 set_sync applied", |v| {
        v["t"] == "hello" && v["sync"]["signal_delay_ms"] == 77
    });
    // …but its watch works.
    send(&mut opted, json!({"t":"watch","addrs":["*"]}));
    std::thread::sleep(Duration::from_millis(50));
    sender.send_to(&Message::new("/ext/opted").arg(Value::Float32(1.0)).encode().unwrap(), hub_addr).unwrap();
    expect(&mut opted, "osc", Duration::from_secs(2), |v| v["addr"] == "/ext/opted");

    drop((console_page, shader_page, opted));
    drop(hub);
    drop(console);
}
