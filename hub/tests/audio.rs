//! Audio integration: the whole audio path in-process — a
//! test "capture" pushing a tone into the ring in real time → sender
//! thread → RTP → a test receiver registered like a device. The hub computes nothing from the audio.

use std::net::UdpSocket;
use std::sync::Arc;
use std::time::{Duration, Instant};

use simply_osc::{Listener, Message, Value};
use tussen_hub::config::Config;
use tussen_hub::generators::audio::{rtp, stream_ring, AudioLink};
use tussen_hub::hub;

/// The M0 shape of the acceptance check, a few seconds instead of 60:
/// a real-time capture + a registered RTP receiver sees ~187.5 pkt/s
/// with zero loss, monotonic seq/timestamps and the tone that went in.
#[test]
fn a_capture_streams_rtp_at_187_5_packets_per_second_with_zero_loss() {
    let config: Config = toml::from_str(
        r#"
        [osc]
        listen_port = 0
        device_timeout_ms = 60000   # one hello carries the whole test
        "#,
    )
    .unwrap();
    assert_eq!(config.audio.sample_rate, 48000);
    assert_eq!(config.audio.packet_frames, 256);

    // The sender + hub as cmd::run wires them; a thread stands in for
    // the capture callback: a 440 Hz tone at 0.5, stereo, paced by the
    // wall clock at 48 kHz in 64-frame bursts.
    let (_tx, rx) = crossbeam_channel::bounded(4096);
    let link = Arc::new(AudioLink::default());
    let (mut ring_tx, ring_rx) = rtrb::RingBuffer::<f32>::new(48000 * 2);
    stream_ring(config.audio.clone(), Arc::clone(&link), ring_rx).unwrap();
    let capture_link = Arc::clone(&link);
    std::thread::spawn(move || {
        let started = Instant::now();
        let mut frames = 0u64;
        while !capture_link.stop.load(std::sync::atomic::Ordering::Relaxed) {
            let due = (started.elapsed().as_secs_f64() * 48000.0) as u64;
            while frames < due {
                let s = 0.5 * (2.0 * std::f64::consts::PI * 440.0 * frames as f64 / 48000.0).sin() as f32;
                let _ = ring_tx.push(s);
                let _ = ring_tx.push(s);
                frames += 1;
            }
            std::thread::sleep(Duration::from_micros(1333));
        }
    });
    let hub = hub::spawn_with_audio(config, rx, true, Some(Arc::clone(&link))).unwrap();

    // A device that wants audio: OSC listener + RTP socket, hello with
    // the RTP port in argument 5.
    let osc = Listener::bind("127.0.0.1:0").unwrap();
    let rtp_socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    rtp_socket.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
    let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
    let hello = Message::new("/tussen/hub/hello")
        .arg(Value::Str("testdev".into()))
        .arg(Value::Int32(osc.local_addr().port() as i32))
        .arg(Value::Str("/tussen/".into()))
        .arg(Value::Str("/tussen/hub/".into()))
        .arg(Value::Int32(rtp_socket.local_addr().unwrap().port() as i32))
        .encode()
        .unwrap();
    sender.send_to(&hello, ("127.0.0.1", hub.addr().port())).unwrap();

    // Receive for ~4 s from the first packet.
    let mut buf = [0u8; 2048];
    let mut packets: Vec<rtp::Packet> = Vec::new();
    let first_arrival = loop {
        let (len, _) = rtp_socket.recv_from(&mut buf).expect("no RTP within 500 ms of hello");
        if let Some(p) = rtp::parse(&buf[..len]) {
            packets.push(p);
            break Instant::now();
        }
    };
    let window = Duration::from_secs(4);
    while first_arrival.elapsed() < window {
        match rtp_socket.recv_from(&mut buf) {
            Ok((len, _)) => {
                if let Some(p) = rtp::parse(&buf[..len]) {
                    packets.push(p);
                }
            }
            Err(_) => break,
        }
    }
    let elapsed = first_arrival.elapsed().as_secs_f64();

    // Rate ≈ 187.5 pkt/s (the capture is paced by the wall clock, allow 5%).
    let rate = (packets.len() - 1) as f64 / elapsed;
    assert!((rate - 187.5).abs() < 187.5 * 0.05, "rate {rate:.1} pkt/s over {elapsed:.2} s");

    // Zero loss, strictly monotonic seq and sample-counting timestamps.
    for pair in packets.windows(2) {
        assert_eq!(pair[1].seq, pair[0].seq.wrapping_add(1), "sequence gap");
        assert_eq!(pair[1].timestamp, pair[0].timestamp.wrapping_add(256), "timestamp gap");
    }
    assert_eq!(packets[0].payload_type, rtp::PAYLOAD_TYPE);
    assert!(packets.iter().all(|p| p.samples.len() == 512), "256 stereo frames per packet");

    // The payload really is a ~440 Hz tone at amplitude 0.5: both
    // channels equal, correct RMS, and zero crossings at ~2·440/s.
    let audio: Vec<f32> = packets.iter().flat_map(|p| p.samples.chunks(2).map(|c| c[0])).collect();
    for p in packets.iter().take(10) {
        for frame in p.samples.chunks(2) {
            assert!((frame[0] - frame[1]).abs() < 1e-4, "channels differ");
        }
    }
    let rms = (audio.iter().map(|s| s * s).sum::<f32>() / audio.len() as f32).sqrt();
    assert!((rms - 0.5 / 2f32.sqrt()).abs() < 0.01, "rms {rms}");
    let crossings = audio.windows(2).filter(|w| (w[0] >= 0.0) != (w[1] >= 0.0)).count();
    let seconds = audio.len() as f64 / 48000.0;
    let hz = crossings as f64 / seconds / 2.0;
    assert!((hz - 440.0).abs() < 5.0, "tone measured at {hz:.1} Hz");

    // The hub computes nothing from the audio: no feature OSC arrives
    // at the registered device, only the RTP stream above.
    // (The hello's `/tussen/hub/welcome` reply is registration, not a feature.)
    while let Some(inbound) = osc.recv_timeout(Duration::from_millis(200)) {
        for (_, m) in inbound.packet.messages() {
            assert_eq!(m.address, "/tussen/hub/welcome", "unexpected OSC from the hub");
        }
    }

    link.stop.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// A static destination with `rtp_port` (the TouchDesigner laptop, which
/// sends no hello) gets the same audio as a registered device, on its own
/// RTP stream: its own SSRC, zero loss, counting seq/timestamps.
#[test]
fn a_static_dest_with_rtp_port_receives_its_own_rtp_stream() {
    let td_rtp = UdpSocket::bind("127.0.0.1:0").unwrap();
    td_rtp.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
    let td_osc = UdpSocket::bind("127.0.0.1:0").unwrap();
    let config: Config = toml::from_str(&format!(
        r#"
        [osc]
        listen_port = 0
        device_timeout_ms = 60000

        [[dest]]
        name = "td"
        host = "127.0.0.1"
        port = {}
        subscribe = ["/tussen/"]
        rtp_port = {}
        "#,
        td_osc.local_addr().unwrap().port(),
        td_rtp.local_addr().unwrap().port(),
    ))
    .unwrap();
    assert_eq!(config.dests[0].rtp_port, Some(td_rtp.local_addr().unwrap().port()));

    let (_tx, rx) = crossbeam_channel::bounded(4096);
    let link = Arc::new(AudioLink::default());
    let (mut ring_tx, ring_rx) = rtrb::RingBuffer::<f32>::new(48000 * 2);
    // The sender starts before the hub, as in cmd::run.
    stream_ring(config.audio.clone(), Arc::clone(&link), ring_rx).unwrap();
    let hub = hub::spawn_with_audio(config, rx, true, Some(Arc::clone(&link))).unwrap();

    // A registered device too, to compare SSRCs.
    let dev_rtp = UdpSocket::bind("127.0.0.1:0").unwrap();
    dev_rtp.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
    let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
    let hello = Message::new("/tussen/hub/hello")
        .arg(Value::Str("testdev".into()))
        .arg(Value::Int32(sender.local_addr().unwrap().port() as i32))
        .arg(Value::Str("/tussen/".into()))
        .arg(Value::Str("/tussen/hub/".into()))
        .arg(Value::Int32(dev_rtp.local_addr().unwrap().port() as i32))
        .encode()
        .unwrap();
    sender.send_to(&hello, ("127.0.0.1", hub.addr().port())).unwrap();
    std::thread::sleep(Duration::from_millis(100));

    // 64 packets' worth of a ramp, pushed at once (the ring holds 1 s).
    for i in 0..64 * 256 {
        let s = (i % 256) as f32 / 512.0;
        ring_tx.push(s).unwrap();
        ring_tx.push(-s).unwrap();
    }

    let recv = |sock: &UdpSocket, n: usize| -> Vec<rtp::Packet> {
        let mut buf = [0u8; 2048];
        let mut out = Vec::new();
        while out.len() < n {
            let (len, _) = sock.recv_from(&mut buf).expect("RTP stopped arriving");
            out.extend(rtp::parse(&buf[..len]));
        }
        out
    };
    let td = recv(&td_rtp, 32);
    let dev = recv(&dev_rtp, 32);

    assert_eq!(td[0].payload_type, rtp::PAYLOAD_TYPE);
    assert!(td.iter().all(|p| p.ssrc == td[0].ssrc), "one SSRC per stream");
    assert_ne!(td[0].ssrc, dev[0].ssrc, "TD's stream has its own SSRC");
    for pair in td.windows(2) {
        assert_eq!(pair[1].seq, pair[0].seq.wrapping_add(1), "sequence gap");
        assert_eq!(pair[1].timestamp, pair[0].timestamp.wrapping_add(256), "timestamp gap");
    }
    assert!(td.iter().all(|p| p.samples.len() == 512), "256 stereo frames per packet");
    // Same audio as the devices get (both see the ramp, frame for frame).
    let frame = |p: &rtp::Packet| (p.samples[2], p.samples[3]);
    assert!((frame(&td[0]).0 - 1.0 / 512.0).abs() < 1e-4 && (frame(&td[0]).1 + 1.0 / 512.0).abs() < 1e-4);
    assert_eq!(td.last().map(frame), dev.last().map(frame));

    link.stop.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Sync point 2 includes the audio: a device that follows
/// TouchDesigner hears the stream `td_round_trip_ms` behind one that
/// follows the music, packet for packet, with no loss; the signal delay
/// never touches the audio.
#[test]
fn a_device_following_td_hears_the_audio_a_round_trip_late() {
    let config: Config = toml::from_str(
        r#"
        [osc]
        listen_port = 0
        device_timeout_ms = 60000
        [sync]
        signal_delay_ms = 40        # signals only: no effect on the audio
        td_round_trip_ms = 100
        [sync.device.phone]
        follow = "music"
        "#,
    )
    .unwrap();
    let (_tx, rx) = crossbeam_channel::bounded(4096);
    let link = Arc::new(AudioLink::default());
    let (mut ring_tx, ring_rx) = rtrb::RingBuffer::<f32>::new(48000 * 2);
    stream_ring(config.audio.clone(), Arc::clone(&link), ring_rx).unwrap();
    let capture_link = Arc::clone(&link);
    std::thread::spawn(move || {
        let started = Instant::now();
        let mut frames = 0u64;
        while !capture_link.stop.load(std::sync::atomic::Ordering::Relaxed) {
            let due = (started.elapsed().as_secs_f64() * 48000.0) as u64;
            while frames < due {
                let _ = ring_tx.push(0.1);
                let _ = ring_tx.push(0.1);
                frames += 1;
            }
            std::thread::sleep(Duration::from_micros(1333));
        }
    });
    let hub = hub::spawn_with_audio(config, rx, true, Some(Arc::clone(&link))).unwrap();

    // Two devices, each timestamping its packets on its own thread.
    let sender = UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut listeners = Vec::new();
    for name in ["quest", "phone"] {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        sock.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
        let hello = Message::new("/tussen/hub/hello")
            .arg(Value::Str(name.into()))
            .arg(Value::Int32(sender.local_addr().unwrap().port() as i32))
            .arg(Value::Str("/ext/".into()))
            .arg(Value::Str("/tussen/hub/".into()))
            .arg(Value::Int32(sock.local_addr().unwrap().port() as i32))
            .encode()
            .unwrap();
        sender.send_to(&hello, ("127.0.0.1", hub.addr().port())).unwrap();
        listeners.push(std::thread::spawn(move || {
            let mut buf = [0u8; 2048];
            let mut got: Vec<(Instant, u16)> = Vec::new();
            let end = Instant::now() + Duration::from_millis(1200);
            while Instant::now() < end {
                if let Ok((len, _)) = sock.recv_from(&mut buf) {
                    let at = Instant::now();
                    if let Some(p) = rtp::parse(&buf[..len]) {
                        got.push((at, p.seq));
                    }
                }
            }
            got
        }));
    }
    let phone = listeners.pop().unwrap().join().unwrap();
    let quest = listeners.pop().unwrap().join().unwrap();
    link.stop.store(true, std::sync::atomic::Ordering::Relaxed);

    assert!(quest.len() > 100 && phone.len() > 100, "quest {} phone {} packets", quest.len(), phone.len());
    for (name, s) in [("quest", &quest), ("phone", &phone)] {
        assert!(s.windows(2).all(|w| w[1].1 == w[0].1.wrapping_add(1)), "{name}: sequence gap");
    }
    // Packet for packet, quest hears it a round trip after the phone.
    let mut deltas: Vec<f64> = quest
        .iter()
        .filter_map(|(tq, seq)| {
            phone.iter().find(|(_, s)| s == seq).map(|(tp, _)| tq.duration_since(*tp).as_secs_f64() * 1e3)
        })
        .collect();
    assert!(deltas.len() > 50, "only {} shared packets", deltas.len());
    deltas.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let (min, median, max) = (deltas[0], deltas[deltas.len() / 2], deltas[deltas.len() - 1]);
    eprintln!("audio round trip: n={} min={min:.2} median={median:.2} max={max:.2} ms (want 100)", deltas.len());
    assert!((median - 100.0).abs() <= 2.0, "median {median:.2} ms");
    assert!(min >= 98.0 && max <= 106.0, "min {min:.2} max {max:.2} ms");
    drop(hub);
}

#[test]
fn a_static_rtp_port_nobody_listens_on_is_reported() {
    // td's OSC port listens; its rtp_port does not.
    let osc = UdpSocket::bind("127.0.0.1:0").unwrap();
    let dead = UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let config: Config = toml::from_str(&format!(
        "[osc]\nlisten_port = 0\n[[dest]]\nname = \"td\"\nhost = \"127.0.0.1\"\nport = {}\nsubscribe = [\"/ext/\"]\nrtp_port = {dead}\n",
        osc.local_addr().unwrap().port()
    ))
    .unwrap();
    let link = Arc::new(AudioLink::default());
    let (mut ring_tx, ring_rx) = rtrb::RingBuffer::<f32>::new(48000 * 2);
    stream_ring(config.audio.clone(), Arc::clone(&link), ring_rx).unwrap();
    let (_tx, rx) = crossbeam_channel::bounded(16);
    let hub = hub::spawn_with_audio(config, rx, true, Some(Arc::clone(&link))).unwrap();
    let target = std::net::SocketAddr::from(([127, 0, 0, 1], dead));
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut seen = None;
    while Instant::now() < deadline && seen.is_none() {
        for _ in 0..2400 {
            let _ = ring_tx.push(0.1);
        }
        std::thread::sleep(Duration::from_millis(25));
        seen = link.static_reach.lock().unwrap().get(&target).and_then(|r| r.error_at(Instant::now()).map(str::to_owned));
    }
    link.stop.store(true, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(seen.as_deref(), Some(format!("nobody listening on port {dead}").as_str()));
    drop(hub);
}
