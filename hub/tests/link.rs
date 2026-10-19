//! The hub's Link config on real sockets, against a Link app that is alone
//! at 126 BPM: the hub, waiting, joins that session at 126 and the app's
//! tempo never moves — the djay case, both orders of switching on.
//!
//! Off the real Link network: a private multicast group on a port of its
//! own, loopback only, ttl 0 (as simply-link's own socket tests).

use std::net::{Ipv4Addr, UdpSocket};
use std::time::{Duration, Instant};

use simply_link::{Config, Link, Network};
use tussen_hub::link::{join_config, should_leave, LinkConfig, Mode};

fn private_network() -> Network {
    let port = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap().local_addr().unwrap().port();
    Network { group: Ipv4Addr::new(239, 255, 76, 77), port, interface: Ipv4Addr::LOCALHOST, multicast_ttl: 0 }
}

fn wait_for(what: &str, mut ok: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(10);
    while !ok() {
        assert!(Instant::now() < until, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn the_hub_joins_a_lone_app_at_its_tempo_and_the_app_keeps_it() {
    // The hub's own tempo is 120: it must never reach the app.
    let hub_config = join_config(&LinkConfig { tempo: 120.0, quantum: 4.0 });
    // An Ableton peer founds the moment Link is on; here after 200 ms alone.
    let app_config = Config { found_after: Some(200_000), bpm: 126.0, ..Config::default() };
    for hub_first in [true, false] {
        let net = private_network();
        let (hub, app);
        if hub_first {
            hub = Link::join_on(hub_config, net).unwrap();
            std::thread::sleep(Duration::from_millis(1500));
            assert_eq!(Mode::of(&hub.state(), false), Mode::Waiting, "a waiting hub founds nothing");
            app = Link::join_on(app_config, net).unwrap();
        } else {
            app = Link::join_on(app_config, net).unwrap();
            wait_for("the app to found", || app.state().joined());
            hub = Link::join_on(hub_config, net).unwrap();
        }
        wait_for("the hub to join", || hub.state().joined() && hub.state().peers == 1);
        let (h, a) = (hub.state(), app.state());
        assert_eq!(h.session, a.session, "hub_first {hub_first}");
        assert!((h.tempo() - 126.0).abs() < 1e-3, "the hub runs at the app's tempo");
        assert!((a.tempo() - 126.0).abs() < 1e-3, "the app's tempo never moved");
        assert_eq!(Mode::of(&h, false), Mode::Joined);
        assert!(!should_leave(&h, false), "the app is still there");
        // The app goes: the joined session empties, and the hub would wait.
        drop(app);
        wait_for("the hub to see the app go", || hub.state().peers == 0);
        assert!(should_leave(&hub.state(), false));
        hub.leave();
        assert_eq!(Mode::of(&hub.state(), false), Mode::Waiting);
    }
}
