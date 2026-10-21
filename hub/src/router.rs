//! Who gets which signal.
//!
//! A destination receives a signal when its address starts with one of
//! the destination's `subscribe` prefixes and with none of its `exclude`
//! prefixes. Static destinations come from config; devices register with
//! `/tussen/hub/hello` and are forgotten after a timeout. A device never
//! receives its own prefix (`/tussen/{name}/`) back — the ownership rule
//! that prevents loops.
//!
//! This module is pure routing state: no sockets, no clock of its own
//! (every time-dependent call takes `now`), so all of it tests in-process.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use simply_osc::{Message, Value};

use crate::config::Config;
use crate::sync::DestKind;

/// A prefix subscription: subscribe minus exclude.
#[derive(Clone, Debug)]
pub struct Subscription {
    pub subscribe: Vec<String>,
    pub exclude: Vec<String>,
}

impl Subscription {
    pub fn wants(&self, address: &str) -> bool {
        self.subscribe.iter().any(|p| address.starts_with(p.as_str()))
            && !self.exclude.iter().any(|p| address.starts_with(p.as_str()))
    }
}

/// A static destination from config: always present, only receives.
#[derive(Clone, Debug)]
pub struct StaticDest {
    pub name: String,
    pub addr: SocketAddr,
    pub sub: Subscription,
    /// engine | td | monitor — the engine is never delayed.
    pub kind: DestKind,
    /// `rtp_port` from config: the audio stream goes here too, on this
    /// destination's host (e.g. TouchDesigner's laptop). None = no audio.
    pub rtp_port: Option<u16>,
    /// The host as configured (an IP or a name like `td-laptop.local`)
    /// and its port: what the hub re-resolves every few seconds.
    pub host: String,
    pub port: u16,
    /// False while the host doesn't resolve: the destination gets
    /// nothing until it does (a laptop booted after the hub).
    pub resolved: bool,
    /// Where its name lookup stands — for the console's status column.
    pub lookup: Lookup,
}

/// A static destination's name lookup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lookup {
    /// An IP literal, or a name that answered on its last lookup.
    Ok,
    /// A name not looked up yet (startup, or after the console's ↻).
    Pending,
    /// The last lookup failed. With `resolved` still true the
    /// destination keeps its last good address.
    Failed,
    /// `host` is empty in config: nothing to look up, nothing is sent.
    NoHost,
}

impl Lookup {
    pub fn as_str(self) -> &'static str {
        match self {
            Lookup::Ok => "ok",
            Lookup::Pending => "pending",
            Lookup::Failed => "failed",
            Lookup::NoHost => "no_host",
        }
    }
}

/// Whether a name is answered without the network (no mDNS, no DNS), so
/// it can be resolved on the startup path.
fn resolves_locally(host: &str) -> bool {
    !is_hostname(host) || host.eq_ignore_ascii_case("localhost")
}

/// Resolve `host:port` to one IPv4 address if it has one (the send
/// socket is IPv4; `.local` names often list an IPv6 link-local first),
/// else whatever comes first. None = doesn't resolve right now.
pub fn resolve_host(host: &str, port: u16) -> Option<SocketAddr> {
    use std::net::ToSocketAddrs;
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Some(SocketAddr::new(ip, port));
    }
    let all: Vec<SocketAddr> = (host, port).to_socket_addrs().ok()?.collect();
    all.iter().find(|a| a.is_ipv4()).or(all.first()).copied()
}

/// Whether `host` is a name (needs re-resolving) rather than an IP literal.
pub fn is_hostname(host: &str) -> bool {
    host.parse::<IpAddr>().is_err()
}

/// One delivery target of an address: who, where, and what kind.
#[derive(Clone, Copy, Debug)]
pub struct Target<'a> {
    pub name: &'a str,
    pub addr: SocketAddr,
    pub kind: DestKind,
}

/// A registered device: announced itself with hello, owns
/// `/tussen/{name}/`, forgotten after the timeout.
#[derive(Clone, Debug)]
pub struct Device {
    pub name: String,
    pub addr: SocketAddr,
    pub sub: Subscription,
    /// The port the device wants the engine's audio on; 0 = no audio.
    /// The router only records it — the audio generator lane reads it.
    pub rtp_port: u16,
    pub last_seen: Instant,
    /// `/tussen/{name}/` — never sent back to this device.
    own_prefix: String,
}

/// What a hello did, for the stats line and the log.
#[derive(Debug, PartialEq, Eq)]
pub enum HelloOutcome {
    Registered,
    Refreshed,
}

#[derive(Debug)]
pub struct Router {
    pub statics: Vec<StaticDest>,
    devices: HashMap<String, Device>,
    timeout: Duration,
}

impl Router {
    /// The config's static destinations. IP literals and `localhost`
    /// resolve here; every other name is left to the hub's background
    /// resolver ([`Router::set_static_addr`]), because a `.local` laptop
    /// that is off the network blocks a lookup for ~5 s, and startup must
    /// not wait for it. Until a name answers, its destination gets nothing.
    pub fn from_config(config: &Config) -> anyhow::Result<Router> {
        let mut statics = Vec::new();
        for d in config.dests.iter().filter(|d| d.has_osc()) {
            let (resolved, lookup) = if d.host.trim().is_empty() {
                log::warn!("destination {}: no host in config; it receives nothing", d.name);
                (None, Lookup::NoHost)
            } else if resolves_locally(&d.host) {
                let r = resolve_host(&d.host, d.port);
                if r.is_none() {
                    log::warn!("destination {}: {} doesn't resolve", d.name, d.host);
                }
                (r, if r.is_some() { Lookup::Ok } else { Lookup::Failed })
            } else {
                (None, Lookup::Pending)
            };
            let addr = resolved.unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], d.port)));
            statics.push(StaticDest {
                host: d.host.clone(),
                port: d.port,
                resolved: resolved.is_some(),
                lookup,
                name: d.name.clone(),
                addr,
                sub: Subscription { subscribe: d.subscribe.clone(), exclude: d.exclude.clone() },
                kind: d.kind(),
                rtp_port: d.rtp_port.filter(|&p| p != 0),
            });
        }
        Ok(Router { statics, devices: HashMap::new(), timeout: Duration::from_millis(config.osc.device_timeout_ms) })
    }

    /// Static destinations whose host is a name: (name, host, port) for
    /// the re-resolver.
    pub fn static_hostnames(&self) -> Vec<(String, String, u16)> {
        self.statics
            .iter()
            .filter(|s| s.lookup != Lookup::NoHost && is_hostname(&s.host))
            .map(|s| (s.name.clone(), s.host.clone(), s.port))
            .collect()
    }

    /// The console's ↻: every name is looked up again, and shows as
    /// pending until it answers. Addresses already known keep working.
    pub fn mark_lookups_pending(&mut self) {
        for s in self.statics.iter_mut().filter(|s| s.lookup != Lookup::NoHost && is_hostname(&s.host)) {
            s.lookup = Lookup::Pending;
        }
    }

    /// Apply a re-resolve result for one static destination. `None`
    /// (doesn't resolve right now) keeps the last good address — a
    /// flaky mDNS answer shouldn't silence TouchDesigner. Returns the
    /// new address when it changed.
    pub fn set_static_addr(&mut self, name: &str, addr: Option<SocketAddr>) -> Option<SocketAddr> {
        let s = self.statics.iter_mut().find(|s| s.name == name)?;
        s.lookup = if addr.is_some() { Lookup::Ok } else { Lookup::Failed };
        let addr = addr?;
        if s.resolved && s.addr == addr {
            return None;
        }
        s.addr = addr;
        s.resolved = true;
        Some(addr)
    }

    pub fn devices(&self) -> impl Iterator<Item = &Device> {
        self.devices.values()
    }

    pub fn device_count(&self) -> usize {
        self.devices.len()
    }

    /// Handle `/tussen/hub/hello`. Arguments: name, OSC port,
    /// comma-separated subscribe prefixes, comma-separated exclude
    /// prefixes, RTP audio port or 0. Lenient about trailing arguments —
    /// a bad hello is logged by the caller, never a crash.
    pub fn hello(
        &mut self,
        from_ip: IpAddr,
        args: &[Value],
        now: Instant,
    ) -> anyhow::Result<HelloOutcome> {
        let name = match args.first() {
            Some(Value::Str(s)) if !s.is_empty() => s.clone(),
            other => anyhow::bail!("hello without a device name: {other:?}"),
        };
        let port = match args.get(1).and_then(Value::as_float) {
            Some(p) if (1.0..=65535.0).contains(&p) => p as u16,
            other => anyhow::bail!("hello from {name} without a usable port: {other:?}"),
        };
        let csv = |i: usize| -> Vec<String> {
            match args.get(i) {
                Some(Value::Str(s)) => s
                    .split(',')
                    .map(str::trim)
                    .filter(|p| !p.is_empty())
                    .map(str::to_owned)
                    .collect(),
                _ => Vec::new(),
            }
        };
        let subscribe = csv(2);
        let exclude = csv(3);
        let rtp_port = match args.get(4).and_then(Value::as_float) {
            Some(p) if (1.0..=65535.0).contains(&p) => p as u16,
            _ => 0,
        };
        let outcome = if self.devices.contains_key(&name) {
            HelloOutcome::Refreshed
        } else {
            HelloOutcome::Registered
        };
        self.devices.insert(
            name.clone(),
            Device {
                own_prefix: format!("/tussen/{name}/"),
                name,
                addr: SocketAddr::new(from_ip, port),
                sub: Subscription { subscribe, exclude },
                rtp_port,
                last_seen: now,
            },
        );
        Ok(outcome)
    }

    /// Handle `/tussen/hub/bye`: the device is gone right now.
    pub fn bye(&mut self, args: &[Value]) -> Option<String> {
        if let Some(Value::Str(name)) = args.first() {
            if self.devices.remove(name).is_some() {
                return Some(name.clone());
            }
        }
        None
    }

    /// Forget devices silent past the timeout; returns who left.
    pub fn prune(&mut self, now: Instant) -> Vec<String> {
        let timeout = self.timeout;
        let gone: Vec<String> = self
            .devices
            .values()
            .filter(|d| now.duration_since(d.last_seen) >= timeout)
            .map(|d| d.name.clone())
            .collect();
        for name in &gone {
            self.devices.remove(name);
        }
        gone
    }

    /// Where to send the RTP audio stream: every registered
    /// device that gave a nonzero rtp_port in its hello, at that port on
    /// the device's IP. The audio generator reads this via the hub loop.
    pub fn rtp_targets(&self) -> Vec<SocketAddr> {
        self.devices
            .values()
            .filter(|d| d.rtp_port != 0)
            .map(|d| SocketAddr::new(d.addr.ip(), d.rtp_port))
            .collect()
    }

    /// Static destinations that asked for the audio stream (`rtp_port`
    /// in config), at that port on the destination's host. They get it
    /// undelayed, each on its own SSRC.
    pub fn static_rtp_targets(&self) -> Vec<SocketAddr> {
        self.statics
            .iter()
            .filter(|s| s.resolved)
            .filter_map(|s| Some(SocketAddr::new(s.addr.ip(), s.rtp_port?)))
            .collect()
    }

    /// TouchDesigner's current IPv4 (the first `td`-kind destination that
    /// resolves), as text; "" when there is none. Carried in every
    /// welcome so a device can point NDI at TD without mDNS
    /// A loopback address is sent as is: TD runs on the
    /// hub's machine, and the device substitutes the hub's address.
    pub fn td_ipv4(&self) -> String {
        self.statics
            .iter()
            .filter(|s| s.kind == DestKind::Td && s.resolved)
            .find_map(|s| match s.addr.ip() {
                IpAddr::V4(v4) if !v4.is_unspecified() => Some(v4.to_string()),
                _ => None,
            })
            .unwrap_or_default()
    }

    /// Every destination this address goes to: subscribe, minus exclude,
    /// minus the ownership rule for devices.
    pub fn targets<'a>(&'a self, address: &'a str) -> impl Iterator<Item = (&'a str, SocketAddr)> {
        self.routes(address).map(|t| (t.name, t.addr))
    }

    /// [`Router::targets`] with each destination's kind — what the delay
    /// and remap stage needs.
    pub fn routes<'a>(&'a self, address: &'a str) -> impl Iterator<Item = Target<'a>> {
        let statics = self
            .statics
            .iter()
            .filter(move |d| d.resolved && d.sub.wants(address))
            .map(|d| Target { name: d.name.as_str(), addr: d.addr, kind: d.kind });
        let devices = self
            .devices
            .values()
            .filter(move |d| !address.starts_with(d.own_prefix.as_str()) && d.sub.wants(address))
            .map(|d| Target { name: d.name.as_str(), addr: d.addr, kind: DestKind::Device });
        statics.chain(devices)
    }

    /// Registered device names, sorted — for the console's `dests`.
    pub fn device_names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.devices.keys().cloned().collect();
        v.sort();
        v
    }
}

/// Whether a message is hub-internal registration traffic, and which kind.
pub enum HubMessage<'a> {
    Hello(&'a [Value]),
    Bye(&'a [Value]),
    Other,
}

pub fn classify(m: &Message) -> HubMessage<'_> {
    match m.address.as_str() {
        "/tussen/hub/hello" => HubMessage::Hello(&m.args),
        "/tussen/hub/bye" => HubMessage::Bye(&m.args),
        _ => HubMessage::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_router() -> Router {
        let config: Config = toml::from_str(
            r#"
            [osc]
            listen_port = 0
            device_timeout_ms = 3000

            [[dest]]
            name = "engine"
            host = "127.0.0.1"
            port = 3928
            subscribe = ["/tussen/", "/ext/", "/djay/"]
            exclude = ["/tussen/midi/", "/tussen/clock/", "/tussen/hub/"]

            [[dest]]
            name = "td"
            host = "127.0.0.1"
            port = 9001
            subscribe = ["/tussen/", "/ext/", "/djay/"]
            exclude = ["/tussen/hub/"]
            "#,
        )
        .unwrap();
        Router::from_config(&config).unwrap()
    }

    #[test]
    fn td_ipv4_is_the_resolved_td_dest_or_empty() {
        let mut r = test_router();
        assert_eq!(r.td_ipv4(), "127.0.0.1");
        r.set_static_addr("td", Some("198.51.100.42:9001".parse().unwrap()));
        assert_eq!(r.td_ipv4(), "198.51.100.42");
        r.statics.retain(|s| s.kind != DestKind::Td);
        assert_eq!(r.td_ipv4(), "");
    }

    fn hello_args(name: &str, port: u16) -> Vec<Value> {
        vec![
            Value::Str(name.into()),
            Value::Int32(port as i32),
            Value::Str("/tussen/,/ext/,/cmd/quest/".into()),
            Value::Str("/tussen/clock/,/tussen/hub/".into()),
            Value::Int32(0),
        ]
    }

    fn names(router: &Router, address: &str) -> Vec<String> {
        let mut v: Vec<String> = router.targets(address).map(|(n, _)| n.to_owned()).collect();
        v.sort();
        v
    }

    #[test]
    fn subscribe_and_exclude_prefixes_select_destinations() {
        let router = test_router();
        assert_eq!(names(&router, "/tussen/quest/head/n/yaw"), ["engine", "td"]);
        assert_eq!(names(&router, "/ext/tablet/fader1"), ["engine", "td"]);
        // Engine excludes the hub's generated prefixes; td takes them.
        assert_eq!(names(&router, "/tussen/midi/cc/74"), ["td"]);
        assert_eq!(names(&router, "/tussen/clock/beat"), ["td"]);
        // Registration traffic reaches nobody that excludes it.
        assert!(names(&router, "/tussen/hub/hello").is_empty());
        // An unrelated prefix reaches nobody.
        assert!(names(&router, "/other/x").is_empty());
    }

    #[test]
    fn a_new_address_needs_no_config_change() {
        // M0 acceptance: `/ext/test/x` reaches every subscriber untouched.
        let router = test_router();
        assert_eq!(names(&router, "/ext/test/x"), ["engine", "td"]);
    }

    #[test]
    fn an_external_tool_publishes_under_its_own_prefix() {
        // an external tool's fixed scheme (djay Pro) needs no
        // renaming — `/djay/…` reaches every subscriber untouched, and a
        // registered device subscribing to `/djay/` gets it too.
        let mut router = test_router();
        assert_eq!(names(&router, "/djay/turntable1/fx/1/type"), ["engine", "td"]);

        let ip: IpAddr = "192.0.2.30".parse().unwrap();
        let args = vec![
            Value::Str("quest".into()),
            Value::Int32(9002),
            Value::Str("/tussen/,/ext/,/djay/".into()),
            Value::Str("/tussen/hub/".into()),
            Value::Int32(0),
        ];
        router.hello(ip, &args, Instant::now()).unwrap();
        assert_eq!(names(&router, "/djay/turntable1/fx/1/type"), ["engine", "quest", "td"]);
    }

    #[test]
    fn a_device_registers_and_never_gets_its_own_prefix_back() {
        let mut router = test_router();
        let now = Instant::now();
        let ip: IpAddr = "192.0.2.30".parse().unwrap();
        assert_eq!(router.hello(ip, &hello_args("quest", 9002), now).unwrap(), HelloOutcome::Registered);
        assert_eq!(router.hello(ip, &hello_args("quest", 9002), now).unwrap(), HelloOutcome::Refreshed);
        assert_eq!(router.device_count(), 1);

        // Its own signals never come back...
        assert_eq!(names(&router, "/tussen/quest/controller/trigger"), ["engine", "td"]);
        // ...but another device's do, and so do commands addressed to it.
        assert_eq!(names(&router, "/tussen/phone/head/yaw"), ["engine", "quest", "td"]);
        assert_eq!(names(&router, "/cmd/quest/recenter"), ["quest"]);
        // Its hello-declared excludes hold.
        assert_eq!(names(&router, "/tussen/clock/beat"), ["td"]);

        let device = router.devices().next().unwrap();
        assert_eq!(device.addr, "192.0.2.30:9002".parse().unwrap());
    }

    #[test]
    fn bye_and_timeout_both_forget_a_device() {
        let mut router = test_router();
        let now = Instant::now();
        let ip: IpAddr = "127.0.0.1".parse().unwrap();

        router.hello(ip, &hello_args("quest", 9002), now).unwrap();
        assert_eq!(router.bye(&[Value::Str("quest".into())]), Some("quest".into()));
        assert_eq!(router.device_count(), 0);
        assert_eq!(router.bye(&[Value::Str("quest".into())]), None);

        router.hello(ip, &hello_args("phone", 9002), now).unwrap();
        // Just under the timeout: still there.
        assert!(router.prune(now + Duration::from_millis(2999)).is_empty());
        // At the timeout: forgotten.
        assert_eq!(router.prune(now + Duration::from_millis(3000)), ["phone"]);
        assert_eq!(router.device_count(), 0);
    }

    #[test]
    fn a_fresh_hello_resets_the_timeout() {
        let mut router = test_router();
        let now = Instant::now();
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        router.hello(ip, &hello_args("quest", 9002), now).unwrap();
        let later = now + Duration::from_millis(2000);
        router.hello(ip, &hello_args("quest", 9002), later).unwrap();
        assert!(router.prune(now + Duration::from_millis(3500)).is_empty());
        assert_eq!(router.prune(later + Duration::from_millis(3000)), ["quest"]);
    }

    #[test]
    fn malformed_hellos_are_errors_not_panics() {
        let mut router = test_router();
        let now = Instant::now();
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        for args in [
            vec![],
            vec![Value::Int32(5)],
            vec![Value::Str("".into())],
            vec![Value::Str("quest".into())],
            vec![Value::Str("quest".into()), Value::Int32(0)],
            vec![Value::Str("quest".into()), Value::Int32(-1)],
            vec![Value::Str("quest".into()), Value::Str("not a port".into())],
        ] {
            assert!(router.hello(ip, &args, now).is_err(), "args {args:?}");
        }
        assert_eq!(router.device_count(), 0);
    }

    #[test]
    fn static_hostnames_follow_re_resolution() {
        let config: Config = toml::from_str(
            r#"
            [osc]
            listen_port = 0
            [[dest]]
            name = "td"
            host = "no-such-host.invalid"
            port = 9001
            subscribe = ["/ext/"]
            rtp_port = 5006
            [[dest]]
            name = "engine"
            host = "127.0.0.1"
            port = 3928
            subscribe = ["/ext/"]
            "#,
        )
        .unwrap();
        // Unresolvable at startup: not an error, just silent.
        let mut router = Router::from_config(&config).unwrap();
        assert_eq!(names(&router, "/ext/x"), ["engine"]);
        assert!(router.static_rtp_targets().is_empty());
        // Only names get re-resolved, not IP literals.
        assert_eq!(router.static_hostnames(), [("td".to_owned(), "no-such-host.invalid".to_owned(), 9001)]);

        // The name starts resolving: td receives at the new address.
        let a: SocketAddr = "198.51.100.20:9001".parse().unwrap();
        assert_eq!(router.set_static_addr("td", Some(a)), Some(a));
        assert_eq!(names(&router, "/ext/x"), ["engine", "td"]);
        assert_eq!(router.static_rtp_targets(), ["198.51.100.20:5006".parse().unwrap()]);
        // Same answer: no change reported.
        assert_eq!(router.set_static_addr("td", Some(a)), None);
        // A failed lookup keeps the last good address.
        assert_eq!(router.set_static_addr("td", None), None);
        assert_eq!(router.targets("/ext/x").find(|(n, _)| *n == "td").unwrap().1, a);
        // The laptop's IP changed (DHCP): td follows.
        let b: SocketAddr = "198.51.100.42:9001".parse().unwrap();
        assert_eq!(router.set_static_addr("td", Some(b)), Some(b));
        assert_eq!(router.targets("/ext/x").find(|(n, _)| *n == "td").unwrap().1, b);
    }

    #[test]
    fn resolve_host_prefers_ipv4_and_takes_literals_as_is() {
        assert_eq!(resolve_host("10.0.0.5", 9001), Some("10.0.0.5:9001".parse().unwrap()));
        let local = resolve_host("localhost", 9001).unwrap();
        assert!(local.is_ipv4() && local.ip().is_loopback(), "{local}");
        assert_eq!(resolve_host("no-such-host.invalid", 1), None);
    }

    #[test]
    fn a_hello_with_no_prefix_lists_registers_but_receives_nothing() {
        let mut router = test_router();
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        let args = vec![Value::Str("mute".into()), Value::Int32(9002)];
        router.hello(ip, &args, Instant::now()).unwrap();
        assert_eq!(router.device_count(), 1);
        assert_eq!(names(&router, "/tussen/quest/head/yaw"), ["engine", "td"]);
    }
}
