//! Recording from the console, mid-run: the Record button's
//! `record_start` / `record_stop` over the real websocket, the state in
//! `hello`, and a take seeded with the world as it was at the press.

use std::net::{SocketAddr, TcpStream, UdpSocket};
use std::time::{Duration, Instant};

use serde_json::{json, Value as Json};
use simply_osc::{Message, Value};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::WebSocket;
use tussen_hub::config::Config;
use tussen_hub::console::{self, ConsoleOptions};
use tussen_hub::hub::{self, Extras, Takes};
use tussen_hub::record;
use tussen_hub::sync::SyncState;
use tussen_hub::take::{Meta, Take};

type Ws = WebSocket<MaybeTlsStream<TcpStream>>;

fn connect(addr: SocketAddr, path: &str) -> Ws {
    let (mut ws, _) = tungstenite::connect(format!("ws://{addr}{path}")).unwrap();
    if let MaybeTlsStream::Plain(s) = ws.get_mut() {
        s.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
    }
    ws
}

/// The next `hello` whose `record` passes `pred`.
fn hello(ws: &mut Ws, pred: impl Fn(&Json) -> bool) -> Json {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if let Ok(tungstenite::Message::Text(t)) = ws.read() {
            let v: Json = serde_json::from_str(t.as_str()).unwrap();
            if v["t"] == "hello" && pred(&v["record"]) {
                return v["record"].clone();
            }
        }
    }
    panic!("no matching hello");
}

fn send(ws: &mut Ws, v: Json) {
    ws.send(tungstenite::Message::text(v.to_string())).unwrap();
}

#[test]
fn the_record_button_starts_a_take_mid_run_seeded_with_the_current_state() {
    let takes = std::env::temp_dir().join(format!("tussen-record-console-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&takes);
    let config: Config = toml::from_str("[osc]\nlisten_port = 0\n").unwrap();
    let meta = Meta { sample_rate: 48000, channels: 2, packet_frames: 256, quantum: 4.0, ..Meta::default() };
    let control = record::Control::new(Some(takes.clone()), meta, None, None);
    let (console, router_side) = console::start_with_record(
        ConsoleOptions {
            port: 0,
            osc_port: 9000,
            config: None,
            statics: vec![],
            sample_rate: 48000,
            state: SyncState::new(config.sync.clone(), vec![]),
        },
        Some(control.clone()),
    )
    .unwrap();
    let (_g, grx) = crossbeam_channel::bounded::<Message>(1);
    let hub = hub::spawn_with_takes(
        config,
        grx,
        true,
        Extras { audio: None, console: Some(router_side) },
        Takes { record: Some(control.slot()), ..Takes::default() },
    )
    .unwrap();
    let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
    let osc = |addr: &str, v: f32| {
        sender.send_to(&Message::new(addr).arg(Value::Float32(v)).encode().unwrap(), ("127.0.0.1", hub.addr().port())).unwrap();
    };
    // Before the press: a knob set once, a fader moving, an event.
    osc("/ext/knob", 5.0);
    osc("/ext/fader", 1.0);
    osc("/ext/pad/press", 1.0);
    std::thread::sleep(Duration::from_millis(100));

    let mut page = connect(console.addr, "/ws");
    assert_eq!(hello(&mut page, |_| true), json!({"on": false, "elapsed_ms": 0}));
    // A read-only page cannot record.
    let mut reader = connect(console.addr, "/ws?readonly=1");
    send(&mut reader, json!({"t": "record_start"}));
    std::thread::sleep(Duration::from_millis(100));
    assert!(!control.status().on, "read-only pages are ignored");

    send(&mut page, json!({"t": "record_start", "name": "mid run"}));
    let on = hello(&mut page, |r| r["on"] == true);
    let take = on["take"].as_str().unwrap().to_owned();
    assert!(take.ends_with("-mid-run"), "{take}");
    std::thread::sleep(Duration::from_millis(100));
    osc("/ext/fader", 2.0);
    std::thread::sleep(Duration::from_millis(300));
    let s = control.status();
    assert!(s.on && s.elapsed_ms >= 300, "elapsed counts: {s:?}");
    send(&mut page, json!({"t": "record_stop"}));
    let off = hello(&mut page, |r| r["on"] == false);
    assert_eq!(off["take"], json!(take), "the stopped take is still named");

    let t = Take::open(&takes.join(&take)).unwrap();
    let vals = |a: &str| -> Vec<(u64, f32)> {
        t.events.iter().filter(|e| e.msg.address == a).map(|e| (e.t, e.msg.args[0].as_float().unwrap())).collect()
    };
    let knob = vals("/ext/knob");
    assert_eq!(knob.len(), 1, "the knob is seeded at the press: {knob:?}");
    assert!(knob[0].0 < 50_000, "seeded at t ≈ 0: {} µs", knob[0].0);
    assert_eq!(knob[0].1, 5.0);
    let fader: Vec<f32> = vals("/ext/fader").iter().map(|v| v.1).collect();
    assert_eq!(fader, [1.0, 2.0], "seed, then what moved");
    assert!(vals("/ext/pad/press").is_empty(), "events are not seeded");
    assert!(t.origins.values().any(|o| o.starts_with("127.0.0.1:")), "the seed keeps its source");
    assert_eq!(t.syncs.len(), 1, "the sync state at the press");
    drop(hub);
    let _ = std::fs::remove_dir_all(&takes);
}

/// `simply-tussen replay` starts the console with no recorder: `hello`
/// carries no `record` (the page shows no Record button) and
/// `record_start` is refused.
#[test]
fn a_replaying_hub_offers_no_record_button_and_refuses_record_start() {
    let (console, _router_side) = console::start_with_record(
        ConsoleOptions {
            port: 0,
            osc_port: 9000,
            config: None,
            statics: vec![],
            sample_rate: 48000,
            state: SyncState::default(),
        },
        None,
    )
    .unwrap();
    let mut page = connect(console.addr, "/ws");
    let first = hello_frame(&mut page);
    assert!(first.get("record").is_none(), "no record state: {first}");
    send(&mut page, json!({"t": "record_start", "name": "nope"}));
    let after = hello_frame(&mut page);
    assert!(after.get("record").is_none(), "still none after record_start: {after}");
}

fn hello_frame(ws: &mut Ws) -> Json {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if let Ok(tungstenite::Message::Text(t)) = ws.read() {
            let v: Json = serde_json::from_str(t.as_str()).unwrap();
            if v["t"] == "hello" {
                return v;
            }
        }
    }
    panic!("no hello");
}
