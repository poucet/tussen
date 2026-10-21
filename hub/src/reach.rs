//! Whether a static destination is being reached.
//!
//! OSC and RTP are UDP: there is no connection, so "connected" can only
//! mean *sent, and nothing came back saying otherwise*. What can come back
//! is the receiving machine's ICMP "port unreachable" (nobody listening on
//! that port) or the local stack's "host unreachable". The kernel reports
//! those only on a *connected* UDP socket — on the hub's shared send socket
//! they vanish — so each static destination gets a [`Conn`] of its own,
//! and its [`Reach`] keeps what the kernel said.
//!
//! A receiver behind a firewall that drops ICMP looks exactly like one that
//! is listening: green means "no error reported", never "received".

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::router::Lookup;

/// How long a reported error keeps a destination marked as failing. A
/// refused port alternates with successful sends (the kernel reports the
/// ICMP on the next send, which then goes out and draws another), so a
/// hold, not the latest send, decides.
pub const ERROR_HOLD: Duration = Duration::from_secs(3);

/// How long after its last successful send a destination still counts as
/// being sent to.
pub const ACTIVE_HOLD: Duration = Duration::from_secs(3);

/// How often a failed socket open is retried.
const REOPEN_EVERY: Duration = Duration::from_secs(1);

/// What sending to one destination has looked like.
#[derive(Clone, Debug, Default)]
pub struct Reach {
    /// Datagrams handed to the kernel without an error, in total.
    pub sent: u64,
    pub last_ok: Option<Instant>,
    /// The last error, worded for the console, and when it was seen.
    pub last_err: Option<(String, Instant)>,
}

impl Reach {
    /// The error to show at `now`, if one was seen within [`ERROR_HOLD`].
    pub fn error_at(&self, now: Instant) -> Option<&str> {
        self.last_err
            .as_ref()
            .filter(|(_, at)| now.saturating_duration_since(*at) < ERROR_HOLD)
            .map(|(e, _)| e.as_str())
    }

    /// Whether something went out within [`ACTIVE_HOLD`] of `now`.
    pub fn active_at(&self, now: Instant) -> bool {
        self.last_ok.is_some_and(|t| now.saturating_duration_since(t) < ACTIVE_HOLD)
    }
}

/// An I/O error as the console says it.
pub fn describe(e: &io::Error, addr: SocketAddr) -> String {
    match e.kind() {
        io::ErrorKind::ConnectionRefused => format!("nobody listening on port {}", addr.port()),
        io::ErrorKind::HostUnreachable => format!("{} unreachable", addr.ip()),
        io::ErrorKind::NetworkUnreachable => format!("no route to {}", addr.ip()),
        // EHOSTDOWN (macOS: ARP got no answer) has no ErrorKind of its own.
        _ if e.raw_os_error() == Some(64) && cfg!(target_os = "macos") => format!("{} is down", addr.ip()),
        _ => e.to_string(),
    }
}

/// A UDP socket connected to one destination, and what sending on it
/// has looked like.
#[derive(Debug)]
pub struct Conn {
    pub addr: SocketAddr,
    sock: Option<UdpSocket>,
    opened: Option<Instant>,
    pub reach: Reach,
}

impl Conn {
    pub fn new(addr: SocketAddr) -> Conn {
        Conn { addr, sock: None, opened: None, reach: Reach::default() }
    }

    fn open(&mut self, now: Instant) {
        if self.sock.is_some() || self.opened.is_some_and(|t| now.saturating_duration_since(t) < REOPEN_EVERY) {
            return;
        }
        self.opened = Some(now);
        let any = match self.addr.ip() {
            IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
        };
        let opened = UdpSocket::bind((any, 0)).and_then(|s| {
            s.connect(self.addr)?;
            s.set_nonblocking(true)?;
            Ok(s)
        });
        match opened {
            Ok(s) => self.sock = Some(s),
            Err(e) => self.reach.last_err = Some((describe(&e, self.addr), now)),
        }
    }

    /// Send one datagram; never blocks. False when it did not go out —
    /// a full send buffer included, which is a count, not an error shown.
    pub fn send(&mut self, datagram: &[u8], now: Instant) -> bool {
        self.open(now);
        let Some(s) = &self.sock else { return false };
        match s.send(datagram) {
            Ok(_) => {
                self.reach.sent += 1;
                self.reach.last_ok = Some(now);
                true
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => false,
            Err(e) => {
                self.reach.last_err = Some((describe(&e, self.addr), now));
                false
            }
        }
    }

    /// Forget earlier errors (the console's ↻): one the kernel still
    /// holds included. The send history stays, so a destination that is
    /// being sent to stays green.
    pub fn forget_errors(&mut self) {
        if let Some(s) = &self.sock {
            let _ = s.take_error();
        }
        self.reach.last_err = None;
    }

    /// Pick up an error the kernel holds for this socket (an ICMP that
    /// arrived after the last send), so a destination that is sent to
    /// rarely still shows it.
    pub fn poll(&mut self, now: Instant) {
        if let Some(Ok(Some(e))) = self.sock.as_ref().map(UdpSocket::take_error) {
            self.reach.last_err = Some((describe(&e, self.addr), now));
        }
    }
}

/// One static destination as the console's status column shows it
/// (`dest_status` frame, docs/guides/signals-websocket.md).
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct DestStatus {
    pub name: String,
    /// `host:port` as configured.
    pub target: String,
    /// The address it resolved to and is sent to; absent until it resolves.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub addr: Option<String>,
    /// The name lookup: `ok`, `pending`, `failed`, `no_host`.
    pub lookup: &'static str,
    /// `ok` (sending, no error reported), `idle` (nothing to send yet),
    /// `pending` (looking up), `warn`, `error`.
    pub state: &'static str,
    /// One line for a human: why it is in that state.
    pub detail: String,
    /// OSC datagrams per second.
    pub osc_rate: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rtp: Option<RtpStatus>,
}

/// The audio stream of a destination with an `rtp_port`.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct RtpStatus {
    pub port: u16,
    /// RTP packets per second.
    pub rate: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// What one stream (OSC or RTP) of a destination has looked like.
#[derive(Clone, Copy, Debug, Default)]
pub struct Seen<'a> {
    pub reach: Option<&'a Reach>,
    pub rate: f32,
}

/// What a static destination is configured as and where its lookup stands.
#[derive(Clone, Copy, Debug)]
pub struct Dest<'a> {
    pub name: &'a str,
    pub host: &'a str,
    pub port: u16,
    /// The address sent to; None until the host resolves.
    pub addr: Option<SocketAddr>,
    pub lookup: Lookup,
}

/// Decide a destination's state and say why.
pub fn status(d: Dest, osc: Seen, rtp: Option<(u16, Seen)>, now: Instant) -> DestStatus {
    let err = |s: &Seen| s.reach.and_then(|r| r.error_at(now)).map(str::to_owned);
    let osc_err = err(&osc);
    let rtp_err = rtp.as_ref().and_then(|(_, s)| err(s));
    let active = osc.reach.is_some_and(|r| r.active_at(now))
        || rtp.as_ref().is_some_and(|(_, s)| s.reach.is_some_and(|r| r.active_at(now)));
    let (state, detail) = match (d.lookup, d.addr) {
        (Lookup::NoHost, _) => ("error", "no host in config (host = \"…\")".to_owned()),
        (Lookup::Pending, None) => ("pending", format!("looking up {}…", d.host)),
        (_, None) => ("error", format!("{} doesn't resolve", d.host)),
        (lookup, Some(_)) => {
            let errors: Vec<String> = [osc_err.map(|e| format!("OSC: {e}")), rtp_err.clone().map(|e| format!("audio: {e}"))]
                .into_iter()
                .flatten()
                .collect();
            if !errors.is_empty() {
                ("error", errors.join("; "))
            } else if lookup == Lookup::Failed {
                ("warn", format!("{} stopped resolving; still sending to the last address", d.host))
            } else if active {
                ("ok", "sending; no error reported".to_owned())
            } else {
                ("idle", "nothing sent yet (no subscribed traffic)".to_owned())
            }
        }
    };
    let detail = if d.lookup == Lookup::Pending && d.addr.is_some() { format!("{detail} · looking up again…") } else { detail };
    DestStatus {
        name: d.name.to_owned(),
        target: format!("{}:{}", d.host, d.port),
        addr: d.addr.map(|a| a.to_string()),
        lookup: d.lookup.as_str(),
        state,
        detail,
        osc_rate: (osc.rate * 10.0).round() / 10.0,
        rtp: rtp.map(|(port, s)| RtpStatus { port, rate: (s.rate * 10.0).round() / 10.0, error: rtp_err }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dest(lookup: Lookup, addr: Option<&str>) -> Dest<'static> {
        Dest { name: "td", host: "td-laptop.local", port: 9001, addr: addr.map(|a| a.parse().unwrap()), lookup }
    }

    #[test]
    fn the_state_says_why() {
        let now = Instant::now();
        let none = Seen::default();
        let s = |d, osc, rtp| status(d, osc, rtp, now);
        assert_eq!(s(dest(Lookup::NoHost, None), none, None).state, "error");
        let p = s(dest(Lookup::Pending, None), none, None);
        assert_eq!((p.state, p.detail.as_str()), ("pending", "looking up td-laptop.local…"));
        let f = s(dest(Lookup::Failed, None), none, None);
        assert_eq!((f.state, f.detail.as_str(), f.addr), ("error", "td-laptop.local doesn't resolve", None));
        // Resolved, nothing sent: idle.
        let at = Some("198.51.100.4:9001");
        assert_eq!(s(dest(Lookup::Ok, at), none, None).state, "idle");
        // Sending: ok.
        let sending = Reach { sent: 9, last_ok: Some(now), last_err: None };
        let ok = s(dest(Lookup::Ok, at), Seen { reach: Some(&sending), rate: 60.04 }, None);
        assert_eq!((ok.state, ok.osc_rate, ok.addr.as_deref()), ("ok", 60.0, Some("198.51.100.4:9001")));
        // A refused port wins over sending.
        let refused = Reach { sent: 9, last_ok: Some(now), last_err: Some(("nobody listening on port 5006".into(), now)) };
        let e = s(dest(Lookup::Ok, at), Seen { reach: Some(&sending), rate: 60.0 }, Some((5006, Seen { reach: Some(&refused), rate: 187.5 })));
        assert_eq!((e.state, e.detail.as_str()), ("error", "audio: nobody listening on port 5006"));
        assert_eq!(e.rtp, Some(RtpStatus { port: 5006, rate: 187.5, error: Some("nobody listening on port 5006".into()) }));
        // The name stopped answering but the old address still works: warn.
        assert_eq!(s(dest(Lookup::Failed, at), Seen { reach: Some(&sending), rate: 1.0 }, None).state, "warn");
        // After ↻, a known address keeps its state and says it is re-checking.
        let again = s(dest(Lookup::Pending, at), Seen { reach: Some(&sending), rate: 1.0 }, None);
        assert_eq!((again.state, again.detail.as_str()), ("ok", "sending; no error reported · looking up again…"));
    }

    #[test]
    fn a_port_nobody_listens_on_is_reported_as_refused() {
        // Bind and drop: a loopback port that is free right now.
        let port = UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        let mut c = Conn::new(addr);
        let t0 = Instant::now();
        // Loopback answers with ICMP port unreachable at once; the kernel
        // reports it on a later send or to take_error.
        for i in 0..50 {
            c.send(b"x", Instant::now());
            c.poll(Instant::now());
            if c.reach.error_at(Instant::now()).is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5 + i));
        }
        assert_eq!(c.reach.error_at(Instant::now()), Some(format!("nobody listening on port {port}").as_str()));
        // The error fades after the hold.
        assert_eq!(c.reach.error_at(t0 + ERROR_HOLD + Duration::from_secs(5)), None);
    }

    #[test]
    fn a_listening_port_reports_no_error() {
        let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut c = Conn::new(rx.local_addr().unwrap());
        for _ in 0..5 {
            assert!(c.send(b"x", Instant::now()));
            c.poll(Instant::now());
        }
        assert_eq!(c.reach.sent, 5);
        assert!(c.reach.active_at(Instant::now()));
        assert_eq!(c.reach.error_at(Instant::now()), None);
    }
}
