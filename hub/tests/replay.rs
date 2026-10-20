//! Record and replay, in-process: a take recorded from a synthetic
//! session (MIDI from a generator, OSC from a test sender, audio from a
//! test "capture", a Link timeline with a tempo change) plays back to a
//! registered device as if live; bar loops repeat bar-aligned and restore
//! state; a replay leads a Link session a simply-link peer follows.

use std::net::{Ipv4Addr, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use simply_link::{Link, Network};
use simply_osc::{Listener, Message, Value};
use tussen_hub::config::Config;
use tussen_hub::generators::audio::{rtp, stream_ring, AudioLink};
use tussen_hub::hub::{self, Extras, Takes};
use tussen_hub::replay::{self, LinkMode, Options};
use tussen_hub::record;
use tussen_hub::take::{self, Meta, Recorder, Take};

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("tussen-replay-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn config() -> Config {
    toml::from_str("[osc]\nlisten_port = 0\ndevice_timeout_ms = 60000\n").unwrap()
}

fn meta() -> Meta {
    Meta { sample_rate: 48000, channels: 2, packet_frames: 256, quantum: 4.0, ..Meta::default() }
}

fn f(m: &Message) -> f32 {
    m.args.first().and_then(Value::as_float).unwrap_or(f32::NAN)
}

/// A device registered with the hub: OSC in, RTP in, everything stamped
/// on arrival by two threads.
struct Device {
    osc: Arc<Mutex<Vec<(Instant, Message)>>>,
    rtp: Arc<Mutex<Vec<(Instant, rtp::Packet)>>>,
}

impl Device {
    fn register(hub: &hub::Hub, name: &str) -> Device {
        let listener = Listener::bind("127.0.0.1:0").unwrap();
        let rtp_socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        rtp_socket.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
        let hello = Message::new("/tussen/hub/hello")
            .arg(Value::Str(name.into()))
            .arg(Value::Int32(listener.local_addr().port() as i32))
            .arg(Value::Str("/tussen/,/ext/,/djay/".into()))
            .arg(Value::Str("/tussen/hub/".into()))
            .arg(Value::Int32(rtp_socket.local_addr().unwrap().port() as i32));
        UdpSocket::bind("127.0.0.1:0")
            .unwrap()
            .send_to(&hello.encode().unwrap(), ("127.0.0.1", hub.addr().port()))
            .unwrap();
        let osc = Arc::new(Mutex::new(Vec::new()));
        let got = Arc::clone(&osc);
        std::thread::spawn(move || loop {
            if let Some(i) = listener.recv_timeout(Duration::from_millis(100)) {
                let at = Instant::now();
                let mut g = got.lock().unwrap();
                for (_, m) in i.packet.messages() {
                    g.push((at, m.clone()));
                }
            }
            if Arc::strong_count(&got) == 1 {
                return;
            }
        });
        let rtp_got = Arc::new(Mutex::new(Vec::new()));
        let got = Arc::clone(&rtp_got);
        std::thread::spawn(move || {
            let mut buf = [0u8; 2048];
            while Arc::strong_count(&got) > 1 {
                if let Ok((n, _)) = rtp_socket.recv_from(&mut buf) {
                    if let Some(p) = rtp::parse(&buf[..n]) {
                        got.lock().unwrap().push((Instant::now(), p));
                    }
                }
            }
        });
        std::thread::sleep(Duration::from_millis(200));
        Device { osc, rtp: rtp_got }
    }

    fn osc(&self, addr: &str) -> Vec<(Instant, f32)> {
        self.osc.lock().unwrap().iter().filter(|(_, m)| m.address == addr).map(|(t, m)| (*t, f(m))).collect()
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// `a − b` in ms, either sign.
fn signed_ms(a: Instant, b: Instant) -> f64 {
    if a >= b {
        ms(a - b)
    } else {
        -ms(b - a)
    }
}

/// Replay `take` into a fresh hub with a registered device; returns
/// (device, replay, hub) once the device is in.
fn replay(take: Take, opts: Options, audio: bool) -> (Device, replay::Replay, hub::Hub, Arc<AudioLink>) {
    replay_with(config(), take, opts, audio)
}

fn replay_with(config: Config, take: Take, opts: Options, audio: bool) -> (Device, replay::Replay, hub::Hub, Arc<AudioLink>) {
    let cfg = config.audio.clone();
    let link = Arc::new(AudioLink::default());
    let (rtx, rrx) = crossbeam_channel::bounded(4096);
    let (_g, grx) = crossbeam_channel::bounded::<Message>(1);
    let hub = hub::spawn_with_takes(
        config,
        grx,
        true,
        Extras { audio: Some(Arc::clone(&link)), console: None },
        Takes { replay: Some(rrx), ..Takes::default() },
    )
    .unwrap();
    let dev = Device::register(&hub, "shader");
    let r = replay::start(Arc::new(take), &opts, rtx, audio.then(|| (cfg, Arc::clone(&link))), None).unwrap();
    (dev, r, hub, link)
}

fn off() -> Options {
    Options { link: LinkMode::Off, ..Options::default() }
}

/// The acceptance run: record ~3 s of a synthetic session, replay it.
#[test]
fn a_recorded_session_replays_to_a_device_as_if_live() {
    let takes = tmp("session");
    // The live hub, recording (as the console's Record button does);
    // audio from a paced test capture.
    let audio = Arc::new(AudioLink::default());
    let control = record::Control::new(Some(takes.clone()), meta(), None, Some(Arc::clone(&audio)));
    let rec = control.start(None).unwrap();
    let dir = rec.dir().to_owned();
    let t0 = rec.start_instant();
    // The Link timeline: 120 BPM from beat 0, 150 from 1.5 s (beat 3).
    rec.link(t0, 120.0, 0.0, true, 0, 4.0);
    rec.link(t0 + Duration::from_millis(1500), 150.0, 3.0, true, 0, 4.0);

    let cfg = config().audio;
    let (mut ring_tx, ring_rx) = rtrb::RingBuffer::<f32>::new(48000 * 2);
    stream_ring(cfg.clone(), Arc::clone(&audio), ring_rx).unwrap();
    let (gen_tx, gen_rx) = crossbeam_channel::bounded::<Message>(4096);
    let hub = hub::spawn_with_takes(
        config(),
        gen_rx,
        true,
        Extras { audio: Some(Arc::clone(&audio)), console: None },
        Takes { record: Some(control.slot()), ..Takes::default() },
    )
    .unwrap();
    let hub_at = ("127.0.0.1", hub.addr().port());
    let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
    // A device that was at the venue: its hello must never be replayed.
    let ghost = UdpSocket::bind("127.0.0.1:0").unwrap();
    ghost.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
    let hello = Message::new("/tussen/hub/hello")
        .arg(Value::Str("ghost".into()))
        .arg(Value::Int32(ghost.local_addr().unwrap().port() as i32))
        .arg(Value::Str("/tussen/".into()));
    sender.send_to(&hello.encode().unwrap(), hub_at).unwrap();

    // 3 s: MIDI every 50 ms, OSC every 70 ms, a click + /ext/test/hit at 1 s and 2 s.
    let start = Instant::now();
    let (mut frames, mut midi, mut osc, mut hits) = (0u64, 0u32, 0u32, 0u32);
    let mut click_from = u64::MAX;
    while start.elapsed() < Duration::from_secs(3) {
        let el = start.elapsed();
        let mut due = (el.as_secs_f64() * 48000.0) as u64;
        if hits < 2 && el >= Duration::from_millis(1000 + 1000 * hits as u64) {
            // A 256-frame click into the capture, and its OSC hit, at once.
            hits += 1;
            click_from = due;
            due += 256;
            sender.send_to(&Message::new("/ext/test/hit").arg(Value::Int32(hits as i32)).encode().unwrap(), hub_at).unwrap();
        }
        while frames < due {
            let s = if frames >= click_from && frames < click_from + 256 { 0.9 } else { 0.0 };
            let _ = ring_tx.push(s);
            let _ = ring_tx.push(s);
            frames += 1;
        }
        if el >= Duration::from_millis(50 * midi as u64) {
            gen_tx.send(Message::new("/tussen/midi/cc/1").arg(Value::Float32(midi as f32))).unwrap();
            midi += 1;
        }
        if el >= Duration::from_millis(70 * osc as u64) {
            sender.send_to(&Message::new("/ext/test/x").arg(Value::Int32(osc as i32)).encode().unwrap(), hub_at).unwrap();
            osc += 1;
        }
        std::thread::sleep(Duration::from_micros(1000));
    }
    std::thread::sleep(Duration::from_millis(50));
    drop(hub);
    audio.stop.store(true, Ordering::Relaxed);
    control.stop();
    while ghost.recv_from(&mut [0u8; 2048]).is_ok() {} // what the ghost got live

    let take = Take::open(&dir).unwrap();
    let recorded = |a: &str| -> Vec<(u64, f32)> {
        take.events.iter().filter(|e| e.msg.address == a).map(|e| (e.t, f(&e.msg))).collect()
    };
    let (rec_x, rec_cc, rec_hit) = (recorded("/ext/test/x"), recorded("/tussen/midi/cc/1"), recorded("/ext/test/hit"));
    assert_eq!(rec_x.len(), osc as usize, "every OSC message recorded");
    assert_eq!(rec_cc.len(), midi as usize, "every generator message recorded");
    assert_eq!(rec_hit.len(), 2);
    assert!(take.events.iter().any(|e| e.msg.address == "/tussen/hub/hello"), "hellos are recorded");
    assert!(take.origins.values().any(|o| o == "hub") && take.origins.values().any(|o| o.starts_with("127.0.0.1:")));
    assert!(take.frames >= 48000 * 29 / 10, "~3 s of audio: {}", take.frames);
    let len_s = take.meta.duration_us as f64 / 1e6;

    // Replay: whole take, Link left alone.
    let (dev, r, _hub, link) = replay(take, off(), true);
    std::thread::sleep(Duration::from_secs_f64(len_s + 0.3));
    drop(r);
    link.stop.store(true, Ordering::Relaxed);

    // Every event at its recorded offset, ± a few ms, in the first pass.
    for (addr, rec) in [("/ext/test/x", &rec_x), ("/tussen/midi/cc/1", &rec_cc)] {
        let got = dev.osc(addr);
        assert!(got.len() >= rec.len(), "{addr}: {} of {}", got.len(), rec.len());
        let values: Vec<f32> = got.iter().take(rec.len()).map(|g| g.1).collect();
        assert_eq!(values, rec.iter().map(|r| r.1).collect::<Vec<_>>(), "{addr} in order");
        let errs: Vec<f64> = got
            .iter()
            .zip(rec.iter())
            .map(|((at, _), (t, _))| ms(at.duration_since(got[0].0)) - (t - rec[0].0) as f64 / 1000.0)
            .collect();
        let worst = errs.iter().fold(0.0f64, |m, e| m.max(e.abs()));
        assert!(worst < 8.0, "{addr}: worst timing error {worst:.1} ms");
    }
    assert!(ghost.recv_from(&mut [0u8; 2048]).is_err(), "a recorded hello registered nobody");

    // Audio: one SSRC, continuous seq, the click packet with its hit.
    let packets = std::mem::take(&mut *dev.rtp.lock().unwrap());
    assert!(packets.len() > 400, "{} packets", packets.len());
    assert!(packets.iter().all(|p| p.1.ssrc == packets[0].1.ssrc));
    for w in packets.windows(2) {
        assert_eq!(w[1].1.seq, w[0].1.seq.wrapping_add(1), "sequence gap");
    }
    let hits = dev.osc("/ext/test/hit");
    assert_eq!(hits.len(), 2, "both hits");
    for (hit_at, _) in &hits {
        let click = packets
            .iter()
            .filter(|p| p.1.samples.iter().any(|s| *s > 0.5))
            .map(|p| signed_ms(p.0, *hit_at))
            .min_by(|a, b| a.abs().total_cmp(&b.abs()))
            .unwrap();
        assert!(click.abs() < 15.0, "the click lands {click:.1} ms from its hit");
    }

    // The clock follows the recorded tempo change: 1.5 s in = 0.5 s after hit 1.
    let bpm = dev.osc("/tussen/clock/bpm");
    assert!((bpm[0].1 - 120.0).abs() < 1e-3, "starts at 120: {}", bpm[0].1);
    let change = bpm.iter().find(|b| (b.1 - 150.0).abs() < 1e-3).expect("150 BPM published").0;
    let after_hit = ms(change.duration_since(hits[0].0));
    assert!((after_hit - 500.0).abs() < 25.0, "tempo change {after_hit:.0} ms after the first hit");
    let _ = std::fs::remove_dir_all(&takes);
}

/// A take built from synthetic stamps: (ms, message).
fn synthetic(name: &str, bpm: &[(u64, f64, f64)], events: &[(u64, Message)], end_ms: u64) -> Take {
    let dir = tmp(name);
    let rec = Recorder::start(&dir, meta(), None).unwrap();
    let t0 = rec.start_instant();
    let at = |ms: u64| t0 + Duration::from_millis(ms);
    for &(t, b, beat) in bpm {
        rec.link(at(t), b, beat, true, 0, 4.0);
    }
    for (t, m) in events {
        rec.osc(at(*t), None, m);
    }
    rec.osc(at(end_ms), None, &Message::new("/ext/end").arg(Value::Int32(0)));
    rec.finish();
    Take::open(&dir).unwrap()
}

fn m(addr: &str, v: f32) -> Message {
    Message::new(addr).arg(Value::Float32(v))
}

#[test]
fn a_take_cut_mid_write_replays_up_to_the_cut() {
    let events: Vec<(u64, Message)> = (0..20).map(|i| (i * 10, m("/ext/n", i as f32))).collect();
    let take = synthetic("cut", &[(0, 120.0, 0.0)], &events, 200);
    let log = take.dir.join("events.bin");
    let len = std::fs::metadata(&log).unwrap().len();
    take::truncate(&log, len * 2 / 3).unwrap();
    let cut = Take::open(&take.dir).unwrap();
    let n = cut.events.iter().filter(|e| e.msg.address == "/ext/n").count();
    assert!(n > 0 && n < 20, "{n}");
    let (dev, r, _hub, _) = replay(cut, off(), false);
    std::thread::sleep(Duration::from_millis(500));
    drop(r);
    let got: Vec<f32> = dev.osc("/ext/n").iter().map(|g| g.1).collect();
    assert_eq!(got, (0..n).map(|i| i as f32).collect::<Vec<_>>(), "everything up to the cut, once");
}

/// 240 BPM, 4 beats a bar: bar n starts at (n−1) s. Loop bar 2..3.
#[test]
fn a_bar_loop_repeats_on_the_bar_restores_state_and_never_refires_events() {
    let mut events: Vec<(u64, Message)> = (0..40).map(|i| (i * 100 + 50, m("/tussen/midi/cc/1", i as f32))).collect();
    events.push((150, m("/djay/x", 7.0)));
    events.push((500, m("/tussen/midi/note/60", 1.0)));
    events.push((1500, m("/tussen/midi/note/60", 0.0)));
    events.push((1700, m("/tussen/midi/note/62", 1.0)));
    events.push((1200, m("/tussen/dev/a/press", 1.0)));
    events.push((100, Message::new("/tussen/hub/hello").arg(Value::Str("ghost".into())).arg(Value::Int32(9))));
    events.sort_by_key(|e| e.0);
    let take = synthetic("loop", &[(0, 240.0, 0.0)], &events, 4000);
    let opts = Options { loop_bars: Some((2, 3)), ..off() };
    let (dev, r, _hub, _) = replay(take, opts, false);
    std::thread::sleep(Duration::from_millis(3800));
    drop(r);

    // The press at bar 2 + 0.2 s, once per pass, a bar apart.
    let press = dev.osc("/tussen/dev/a/press");
    assert_eq!(press.len(), 4, "once per pass: {press:?}");
    for w in press.windows(2) {
        let d = ms(w[1].0.duration_since(w[0].0));
        assert!((d - 1000.0).abs() < 6.0, "presses {d:.1} ms apart");
    }
    // State at A on every pass: CC 1 at its value before 1 s (9), the note held.
    let cc = dev.osc("/tussen/midi/cc/1");
    let restores: Vec<f32> = cc.iter().map(|c| c.1).filter(|v| *v == 9.0).collect();
    assert_eq!(restores.len(), 4, "CC restored at each pass start: {cc:?}");
    let in_pass: Vec<f32> = cc.iter().map(|c| c.1).take(11).collect();
    assert_eq!(in_pass, [9.0, 10.0, 11.0, 12.0, 13.0, 14.0, 15.0, 16.0, 17.0, 18.0, 19.0]);
    let note: Vec<f32> = dev.osc("/tussen/midi/note/60").iter().map(|n| n.1).collect();
    assert_eq!(note[..8], [1.0, 0.0, 1.0, 0.0, 1.0, 0.0, 1.0, 0.0], "held at A, released at 1.5 s, every pass");
    // A note first played inside the loop is let go at the seam.
    let n62: Vec<f32> = dev.osc("/tussen/midi/note/62").iter().map(|n| n.1).collect();
    assert_eq!(&n62[..4], [1.0, 0.0, 1.0, 0.0]);
    // An unchanged value is sent once, not at every seam.
    assert_eq!(dev.osc("/djay/x").len(), 1);
    // The beat counts up across seams, in the recorded bar phase.
    let beats = dev.osc("/tussen/clock/beat");
    assert!(beats.windows(2).all(|w| w[1].1 > w[0].1), "the beat never goes back");
    for (at, _) in &press {
        let b = beats.iter().rev().find(|b| b.0 <= *at).unwrap().1;
        let phase = (b as f64).rem_euclid(4.0);
        assert!((phase - 0.8).abs() < 0.1, "press at beat phase {phase:.2} (recorded 0.8)");
    }
    let onbeat = dev.osc("/tussen/clock/onbeat");
    assert!(onbeat.windows(2).all(|w| w[1].1 == w[0].1 + 1.0), "onbeat once per beat: {onbeat:?}");
}

fn private_network() -> Network {
    let port = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap().local_addr().unwrap().port();
    Network { group: Ipv4Addr::new(239, 255, 76, 78), port, interface: Ipv4Addr::LOCALHOST, multicast_ttl: 0 }
}

fn wait_for(what: &str, mut ok: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(10);
    while !ok() {
        assert!(Instant::now() < until, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The session beat at `peer` minus the replay's published beat, now.
fn phase_error(peer: &Link, r: &replay::Replay) -> f64 {
    let rc = r.link().unwrap().clock();
    let (hp, hr) = (peer.clock().now(), rc.now());
    peer.state().beat_at(hp) - r.plan.lock().unwrap().clock(hr).beat
}

/// 120 BPM for bar 1, 150 from bar 2 (2 s, beat 4): bar 3 at 3.6 s. Loop 1..3.
#[test]
fn a_replay_founds_a_link_session_a_peer_follows_through_tempo_changes_and_seams() {
    let take = synthetic("link", &[(0, 120.0, 0.0), (2000, 150.0, 4.0)], &[], 6000);
    let net = private_network();
    // An app that only ever joins (TouchDesigner's Link CHOP).
    let peer = Link::join_on(simply_link::Config::default(), net).unwrap();
    let opts = Options { loop_bars: Some((1, 3)), link: LinkMode::Lead, network: net, link_wait: Duration::from_millis(300), ..Options::default() };
    let (_dev, r, _hub, _) = replay(take, opts, false);
    let start = Instant::now();
    wait_for("the peer to join the replay's session", || peer.state().joined());
    assert!((peer.state().tempo() - 120.0).abs() < 0.01, "the take's tempo: {}", peer.state().tempo());
    assert!(phase_error(&peer, &r).abs() < 0.02, "in phase: {}", phase_error(&peer, &r));
    // Into bar 2: the recorded 150 BPM.
    std::thread::sleep(Duration::from_millis(2400).saturating_sub(start.elapsed()));
    assert!((peer.state().tempo() - 150.0).abs() < 0.01, "followed the tempo change: {}", peer.state().tempo());
    assert!(phase_error(&peer, &r).abs() < 0.02, "in phase after the change: {}", phase_error(&peer, &r));
    // Past the seam (3.6 s + start margin): back to 120, still in phase.
    std::thread::sleep(Duration::from_millis(4200).saturating_sub(start.elapsed()));
    assert!((peer.state().tempo() - 120.0).abs() < 0.01, "back to 120 at the seam: {}", peer.state().tempo());
    let e = phase_error(&peer, &r);
    assert!(e.abs() < 0.02, "in phase after the seam: {e}");
}

#[test]
fn a_replay_leads_a_session_it_finds_on_that_sessions_bar_grid() {
    let take = synthetic("lead", &[(0, 120.0, 1.0)], &[], 4000);
    let net = private_network();
    // An app alone at 100 BPM (Ableton founds at once).
    let app = Link::join_on(simply_link::Config { found_after: Some(100_000), bpm: 100.0, ..Default::default() }, net).unwrap();
    wait_for("the app to found", || app.state().joined());
    let opts = Options { link: LinkMode::Lead, network: net, link_wait: Duration::from_secs(3), ..Options::default() };
    let (_dev, r, _hub, _) = replay(take, opts, false);
    let rl = r.link().unwrap();
    assert_eq!(rl.state().session, app.state().session, "joined the app's session, founded none");
    wait_for("the app to take the take's tempo", || (app.state().tempo() - 120.0).abs() < 0.01);
    std::thread::sleep(Duration::from_millis(300));
    let e = phase_error(&app, &r);
    assert!(e.rem_euclid(4.0).min(4.0 - e.rem_euclid(4.0)) < 0.02, "bar phase matches the session's: {e}");
}

/// The visual path, tested with a replay: TouchDesigner (a `td`
/// destination with an RTP port) and a device following TD both get the
/// replayed signals and audio with the sync points as configured — the
/// signal delay for both, the round trip on top for the device, audio
/// undelayed to TD and held back by the round trip for the device — so
/// signals and audio still meet at TD, and at the device as TD's NDI does.
#[test]
fn a_replay_drives_td_and_a_device_through_both_sync_points() {
    // The take: a hit entering the hub at 1.0 s; its music, through a mixer
    // 60 ms late, entering at 1.06 s. The signal delay in force while
    // recording (25 ms here) plays no part: replay is ingress-faithful.
    let dir = tmp("visual");
    let rec = Recorder::start(&dir, meta(), None).unwrap();
    let t0 = rec.start_instant();
    let at = |us: u64| t0 + Duration::from_micros(us);
    rec.sync(t0, 25, 40);
    rec.link(t0, 120.0, 0.0, true, 0, 4.0);
    let mut tap = rec.audio_tap(512);
    let packet_us = 256.0 * 1e6 / 48000.0;
    for i in 0..375u64 {
        let t = (i as f64 * packet_us) as u64;
        let click = (1_060_000..1_060_000 + packet_us as u64).contains(&t);
        tap.push(&vec![if click { 0.9 } else { 0.0 }; 512], at(t));
        std::thread::sleep(Duration::from_micros(200)); // let the writer keep up
    }
    rec.osc(at(1_000_000), None, &Message::new("/ext/hit").arg(Value::Int32(1)));
    rec.osc(at(1_990_000), None, &Message::new("/ext/end").arg(Value::Int32(0)));
    rec.finish();
    let take = Take::open(&dir).unwrap();
    assert!(take.frames >= 375 * 256 - 256, "{} frames", take.frames);

    // The rig, tuned to the mixer: 60 ms signal delay, 40 ms round trip.
    let td_osc = Listener::bind("127.0.0.1:0").unwrap();
    let td_rtp = UdpSocket::bind("127.0.0.1:0").unwrap();
    td_rtp.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
    let config: Config = toml::from_str(&format!(
        r#"
        [osc]
        listen_port = 0
        device_timeout_ms = 60000
        [sync]
        signal_delay_ms = 60
        td_round_trip_ms = 40
        [[dest]]
        name = "td"
        host = "127.0.0.1"
        port = {}
        rtp_port = {}
        subscribe = ["/ext/", "/tussen/"]
        exclude = ["/tussen/hub/"]
        "#,
        td_osc.local_addr().port(),
        td_rtp.local_addr().unwrap().port()
    ))
    .unwrap();
    let td_hit = std::thread::spawn(move || {
        let until = Instant::now() + Duration::from_secs(3);
        while Instant::now() < until {
            if let Some(i) = td_osc.recv_timeout(Duration::from_millis(50)) {
                if i.packet.messages().iter().any(|(_, m)| m.address == "/ext/hit") {
                    return Some(Instant::now());
                }
            }
        }
        None
    });
    let td_click = std::thread::spawn(move || {
        let mut buf = [0u8; 2048];
        let until = Instant::now() + Duration::from_secs(3);
        while Instant::now() < until {
            if let Ok((n, _)) = td_rtp.recv_from(&mut buf) {
                if rtp::parse(&buf[..n]).is_some_and(|p| p.samples.iter().any(|s| *s > 0.5)) {
                    return Some(Instant::now());
                }
            }
        }
        None
    });
    let (dev, r, _hub, link) = replay_with(config, take, off(), true);
    std::thread::sleep(Duration::from_millis(1600));
    drop(r);
    link.stop.store(true, Ordering::Relaxed);
    let td_hit = td_hit.join().unwrap().expect("TD got the replayed hit");
    let td_click = td_click.join().unwrap().expect("TD got the replayed audio");
    let dev_hit = dev.osc("/ext/hit")[0].0;
    let dev_click = dev.rtp.lock().unwrap().iter().find(|p| p.1.samples.iter().any(|s| *s > 0.5)).expect("the device got the audio").0;

    // At TD: the hit (delayed 60 ms) and its music (at its recorded time) together.
    let at_td = signed_ms(td_click, td_hit);
    assert!(at_td.abs() < 8.0, "at TD the music lands {at_td:.1} ms from its hit");
    // At the device: both 40 ms behind TD, together.
    let at_dev = signed_ms(dev_click, dev_hit);
    assert!(at_dev.abs() < 8.0, "at the device the music lands {at_dev:.1} ms from its hit");
    let rt = signed_ms(dev_hit, td_hit);
    assert!((rt - 40.0).abs() < 6.0, "the device's signals run {rt:.1} ms behind TD's (round trip 40)");
    let rt_audio = signed_ms(dev_click, td_click);
    assert!((rt_audio - 40.0).abs() < 8.0, "the device's audio runs {rt_audio:.1} ms behind TD's");
    let _ = std::fs::remove_dir_all(&dir);
}
