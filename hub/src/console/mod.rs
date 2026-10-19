//! The hub console: a page at `http://127.0.0.1:9098` and one
//! WebSocket at `/ws`, for setting the two sync points, each device's
//! follow switch and the remap rules by eye.
//!
//! Threads and who owns what:
//!
//! * the **console thread** ([`start`]) owns the authoritative
//!   [`SyncState`]. Page commands mutate it; every change goes to the
//!   router as a fresh `Arc<SyncState>` ([`RouterCmd::Sync`]) — the router
//!   swaps a pointer and never holds a lock while sending. Save writes the
//!   same state to disk, so there is no race between a command and Save.
//! * the **router** and the **audio sender** publish timeline taps into
//!   the [`Tap`]: a bounded channel, `try_send`, drop and count. A slow
//!   page can cost itself data; it can never stall routing or audio.
//! * one **thread per page** (`server`) speaks the WebSocket; each has its
//!   own bounded outbound queue, also drop-and-count.
//!
//! Every timestamp is the hub's monotonic clock in microseconds
//! ([`now_us`]), shared by audio, MIDI and OSC so the page can line them up.

mod protocol;
mod save;
mod server;

pub use protocol::{parse_request, OscEndpoint, Request};
pub use save::{save, save_config, write_into};

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TrySendError};
use serde::Serialize;

use crate::config_file::ConfigFile;
use crate::sync::{DestKind, SyncState};

/// Capacity of the tap channel (router + audio → console thread).
const TAP_CAPACITY: usize = 4096;
/// Capacity of each page's outbound queue (JSON frames).
const CLIENT_CAPACITY: usize = 1024;
/// The seen table never evicts (the list is the permanent record of every
/// address this hub run has routed); past this many it logs one warning,
/// because that cardinality means some sender is minting addresses.
pub const SEEN_WARN: usize = 65536;
/// How often the router publishes the seen-address list (~2 Hz).
pub const ADDRS_PERIOD: Duration = Duration::from_millis(500);
/// How long the audio sender batches blocks before publishing (~30 Hz).
pub const AUDIO_BATCH: Duration = Duration::from_millis(33);

static EPOCH: OnceLock<Instant> = OnceLock::new();

/// The hub clock: microseconds since the first call in this process.
pub fn now_us() -> u64 {
    us_at(Instant::now())
}

/// `t` on the hub clock.
pub fn us_at(t: Instant) -> u64 {
    let epoch = *EPOCH.get_or_init(Instant::now);
    t.saturating_duration_since(epoch).as_micros() as u64
}

/// The address arrived on :9000 from outside.
pub const DIR_IN: u8 = 1;
/// The hub made it: a generated signal (/tussen/clock/…, /tussen/midi/…)
/// or a remap rule's output — addresses that never appear inbound.
pub const DIR_OUT: u8 = 2;

/// One seen address, as the page lists it: when, how often, how many in
/// total since hub start, the last value and the min/max ever seen, and
/// which way it flows (`"in"`, `"out"` or `"both"`).
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct AddrInfo {
    pub addr: String,
    pub last_ts: u64,
    pub rate: f32,
    pub v: f32,
    pub min: f32,
    pub max: f32,
    pub count: u64,
    pub dir: &'static str,
}

/// What the router and the audio sender publish for the page.
#[derive(Debug)]
pub enum Event {
    /// Registered device names changed (sent on the reliable channel).
    Devices(Vec<String>),
    /// min/max/rms per RTP packet, `ts` = the first block.
    Audio { ts: u64, blocks: Vec<[f32; 3]> },
    Midi { ts: u64, addr: String, v: f32 },
    Osc { ts: u64, addr: String, v: f32, src: String },
    Addrs(Vec<AddrInfo>),
    /// The Link session, for the beat grid (~4 Hz and on change).
    Link(crate::link::LinkStatus),
}

/// The publish side of the console, shared by the router and the audio
/// sender. Nothing on it blocks.
pub struct Tap {
    events: Sender<Event>,
    control: Sender<Event>,
    clients: AtomicUsize,
    dropped: AtomicU64,
}

impl Tap {
    /// Whether any page is connected — skip building events otherwise.
    pub fn active(&self) -> bool {
        self.clients.load(Ordering::Relaxed) > 0
    }

    /// Publish a timeline event; a full queue drops it and counts.
    pub fn send(&self, ev: Event) {
        if let Err(TrySendError::Full(_)) = self.events.try_send(ev) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Publish a rare state change that must not be dropped.
    pub fn control(&self, ev: Event) {
        let _ = self.control.send(ev);
    }

    /// Events dropped so far (tap queue and page queues together).
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// Live-state updates from the console thread to the router.
#[derive(Debug)]
pub enum RouterCmd {
    Sync(Arc<SyncState>),
    /// The union of every page's OSC watch list.
    Watch(Arc<HashSet<String>>),
}

/// What the router loop holds: commands in, the tap out.
pub struct RouterConsole {
    pub cmds: Receiver<RouterCmd>,
    pub tap: Arc<Tap>,
}

/// The router's table of every address it has seen.
/// Permanent for the life of the process: nothing is ever evicted, so the
/// list never loses a quiet address, and the snapshot is sorted by address
/// so it never reorders under traffic.
pub struct SeenTable {
    map: HashMap<String, Seen>,
    warned: bool,
    last: Instant,
}

struct Seen {
    last_us: u64,
    count: u32,
    total: u64,
    rate: f32,
    v: f32,
    min: f32,
    max: f32,
    dirs: u8,
}

fn dir_str(dirs: u8) -> &'static str {
    match dirs {
        DIR_IN => "in",
        DIR_OUT => "out",
        _ => "both",
    }
}

impl SeenTable {
    pub fn new() -> SeenTable {
        SeenTable { map: HashMap::new(), warned: false, last: Instant::now() }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Record one message at `us` with its value and direction
    /// ([`DIR_IN`] / [`DIR_OUT`]; both accumulate). Known addresses
    /// don't allocate. The first value initialises min and max.
    pub fn note(&mut self, addr: &str, us: u64, v: f32, dir: u8) {
        if let Some(s) = self.map.get_mut(addr) {
            s.last_us = us;
            s.count += 1;
            s.total += 1;
            s.v = v;
            s.min = s.min.min(v);
            s.max = s.max.max(v);
            s.dirs |= dir;
            return;
        }
        if self.map.len() >= SEEN_WARN && !self.warned {
            self.warned = true;
            eprintln!(
                "tussen-hub: {} distinct OSC addresses seen — some sender is minting \
                 addresses; the console's list keeps them all",
                self.map.len() + 1
            );
        }
        self.map.insert(
            addr.to_owned(),
            Seen { last_us: us, count: 1, total: 1, rate: 0.0, v, min: v, max: v, dirs: dir },
        );
    }

    /// Every address, sorted by address — a stable order, so the page's
    /// list never shuffles — with its message rate over the time since
    /// the last snapshot (lightly smoothed).
    pub fn snapshot(&mut self, now: Instant) -> Vec<AddrInfo> {
        let secs = now.duration_since(self.last).as_secs_f32().max(1e-3);
        self.last = now;
        let mut list: Vec<AddrInfo> = self
            .map
            .iter_mut()
            .map(|(addr, s)| {
                let inst = s.count as f32 / secs;
                s.rate = if s.rate == 0.0 { inst } else { 0.5 * s.rate + 0.5 * inst };
                s.count = 0;
                AddrInfo {
                    addr: addr.clone(),
                    last_ts: s.last_us,
                    rate: (s.rate * 10.0).round() / 10.0,
                    v: s.v,
                    min: s.min,
                    max: s.max,
                    count: s.total,
                    dir: dir_str(s.dirs),
                }
            })
            .collect();
        list.sort_by(|a, b| a.addr.cmp(&b.addr));
        list
    }
}

/// The hub's LAN endpoint for OSC senders, shown on the page with a copy
/// button so it can be handed to someone on another laptop (djay Pro).
///
/// The IP is the interface the default route uses, found by connecting a
/// UDP socket to a routable address and reading `local_addr` — nothing is
/// sent. Rebuilt on every `hello`, so a Wi-Fi change never shows a stale
/// address for long. Loopback is excluded; no route at all ⇒ empty list.
pub fn osc_endpoint(port: u16) -> OscEndpoint {
    let ips = std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| {
            s.connect("192.0.2.1:9")?; // TEST-NET-1: never routed to, never sent to
            s.local_addr()
        })
        .ok()
        .map(|a| a.ip())
        .filter(|ip| !ip.is_loopback() && !ip.is_unspecified())
        .map(|ip| vec![ip.to_string()])
        .unwrap_or_default();
    OscEndpoint { port, ips, hostname: hostname() }
}

/// The machine's hostname as `name.local` (the guides address laptops by
/// it). Asked once per process — it does not change mid-show.
fn hostname() -> Option<String> {
    static HOST: OnceLock<Option<String>> = OnceLock::new();
    HOST.get_or_init(|| {
        let out = std::process::Command::new("hostname").output().ok()?;
        let name = String::from_utf8(out.stdout).ok()?.trim().to_owned();
        if name.is_empty() {
            return None;
        }
        Some(if name.ends_with(".local") { name } else { format!("{name}.local") })
    })
    .clone()
}

/// How to start the console.
pub struct ConsoleOptions {
    /// TCP port on 127.0.0.1; 0 = ephemeral.
    pub port: u16,
    /// The port OSC senders send to (`[osc] listen_port`), shown on the
    /// page next to the hub's LAN IP.
    pub osc_port: u16,
    /// The config file the hub was started with — where Save writes
    /// (redirected to the user config if it is an example: repo or built-in).
    pub config: Option<ConfigFile>,
    /// Static destinations in config order, with their kinds.
    pub statics: Vec<(String, DestKind)>,
    /// The audio stream's sample rate (for `hello`).
    pub sample_rate: u32,
    /// The state loaded from config (the router starts with the same).
    pub state: SyncState,
}

/// A running console. Dropping it stops the server threads.
pub struct Console {
    pub addr: SocketAddr,
    pub tap: Arc<Tap>,
    /// The page's tempo and start/stop commands, for the Link generator.
    pub link_cmds: Receiver<crate::link::LinkCmd>,
    stop: Arc<AtomicBool>,
}

impl Drop for Console {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Wake the blocking accept.
        let _ = std::net::TcpStream::connect_timeout(&self.addr, Duration::from_millis(100));
    }
}

/// Messages from page threads to the console thread.
pub(crate) enum ClientMsg {
    /// The bool: read-only — a cross-origin local page, or `?readonly=1`
    /// (server.rs); such a connection may `watch` and nothing else.
    Join(u64, Sender<Arc<str>>, bool),
    Leave(u64),
    Text(u64, String),
}

/// Bind 127.0.0.1:port, start the server and console threads. Hand the
/// returned [`RouterConsole`] to the hub loop and `console.tap` to the
/// audio sender.
pub fn start(opts: ConsoleOptions) -> anyhow::Result<(Console, RouterConsole)> {
    let listener = server::bind(opts.port)?;
    let addr = listener.local_addr()?;
    let _ = now_us(); // pin the epoch before anything timestamps
    let (ev_tx, ev_rx) = crossbeam_channel::bounded(TAP_CAPACITY);
    let (ctl_tx, ctl_rx) = crossbeam_channel::unbounded();
    let tap = Arc::new(Tap {
        events: ev_tx,
        control: ctl_tx,
        clients: AtomicUsize::new(0),
        dropped: AtomicU64::new(0),
    });
    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
    let (inbox_tx, inbox_rx) = crossbeam_channel::unbounded();
    let (link_tx, link_rx) = crossbeam_channel::bounded(64);
    let stop = Arc::new(AtomicBool::new(false));

    std::thread::Builder::new().name("tussen-hub console accept".into()).spawn({
        let stop = Arc::clone(&stop);
        move || server::accept_loop(listener, inbox_tx, stop)
    })?;
    std::thread::Builder::new().name("tussen-hub console".into()).spawn({
        let mut hub = ConsoleHub {
            state: opts.state,
            osc_port: opts.osc_port,
            statics: opts.statics,
            devices: Vec::new(),
            clients: HashMap::new(),
            router: cmd_tx,
            tap: Arc::clone(&tap),
            config: opts.config,
            sample_rate: opts.sample_rate,
            link: link_tx,
        };
        let stop = Arc::clone(&stop);
        move || hub.run(&ev_rx, &ctl_rx, &inbox_rx, &stop)
    })?;
    log::debug!("console: http://{addr}"); // `run` prints it in its banner
    Ok((Console { addr, tap: Arc::clone(&tap), link_cmds: link_rx, stop }, RouterConsole { cmds: cmd_rx, tap }))
}

struct Client {
    tx: Sender<Arc<str>>,
    watch: HashSet<String>,
    /// May `watch` and nothing else (docs/guides/signals-websocket.md).
    readonly: bool,
}

struct ConsoleHub {
    state: SyncState,
    osc_port: u16,
    statics: Vec<(String, DestKind)>,
    devices: Vec<String>,
    clients: HashMap<u64, Client>,
    router: Sender<RouterCmd>,
    tap: Arc<Tap>,
    config: Option<ConfigFile>,
    sample_rate: u32,
    link: Sender<crate::link::LinkCmd>,
}

impl ConsoleHub {
    fn run(
        &mut self,
        events: &Receiver<Event>,
        control: &Receiver<Event>,
        inbox: &Receiver<ClientMsg>,
        stop: &AtomicBool,
    ) {
        while !stop.load(Ordering::Relaxed) {
            crossbeam_channel::select! {
                recv(control) -> ev => if let Ok(ev) = ev { self.event(ev) },
                recv(inbox) -> msg => if let Ok(msg) = msg { self.client(msg) },
                recv(events) -> ev => if let Ok(ev) = ev { self.event(ev) },
                default(Duration::from_millis(100)) => {}
            }
        }
    }

    fn dests(&self) -> Vec<protocol::DestOut> {
        self.statics
            .iter()
            .map(|(n, k)| (n.as_str(), *k))
            .chain(self.devices.iter().map(|n| (n.as_str(), DestKind::Device)))
            .map(|(name, kind)| protocol::DestOut {
                name: name.to_owned(),
                kind,
                follow: (kind == DestKind::Device).then(|| self.state.follow(name)),
            })
            .collect()
    }

    fn kind_of(&self, name: &str) -> DestKind {
        self.statics.iter().find(|(n, _)| n == name).map(|(_, k)| *k).unwrap_or(DestKind::Device)
    }

    fn hello(&self) -> Arc<str> {
        let s = &self.state.sync;
        protocol::hello(
            &self.dests(),
            s.signal_delay_ms,
            s.td_round_trip_ms,
            &self.state.rules,
            self.sample_rate,
            &osc_endpoint(self.osc_port),
        )
        .into()
    }

    fn send_to(&self, client: &Client, frame: &Arc<str>) {
        if client.tx.try_send(Arc::clone(frame)).is_err() {
            self.tap.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn broadcast(&self, frame: Arc<str>) {
        for c in self.clients.values() {
            self.send_to(c, &frame);
        }
    }

    fn push_state(&self) {
        let _ = self.router.send(RouterCmd::Sync(Arc::new(self.state.clone())));
    }

    fn push_watch(&self) {
        let union: HashSet<String> = self.clients.values().flat_map(|c| c.watch.iter().cloned()).collect();
        let _ = self.router.send(RouterCmd::Watch(Arc::new(union)));
    }

    fn event(&mut self, ev: Event) {
        if self.clients.is_empty() && !matches!(ev, Event::Devices(_)) {
            return;
        }
        match ev {
            Event::Devices(list) => {
                self.devices = list;
                self.broadcast(protocol::dests(&self.dests()).into());
            }
            Event::Audio { ts, blocks } => self.broadcast(protocol::audio(ts, &blocks).into()),
            Event::Midi { ts, addr, v } => self.broadcast(protocol::midi(ts, &addr, v).into()),
            Event::Addrs(list) => self.broadcast(protocol::addrs(&list).into()),
            Event::Link(status) => self.broadcast(protocol::link(&status).into()),
            Event::Osc { ts, addr, v, src } => {
                let mut frame: Option<Arc<str>> = None;
                // "*" watches everything (the history tab).
                for c in self.clients.values().filter(|c| c.watch.contains("*") || c.watch.contains(&addr)) {
                    let f = frame.get_or_insert_with(|| protocol::osc(ts, &addr, v, &src).into());
                    self.send_to(c, f);
                }
            }
        }
    }

    fn client(&mut self, msg: ClientMsg) {
        match msg {
            ClientMsg::Join(id, tx, readonly) => {
                let c = Client { tx, watch: HashSet::new(), readonly };
                self.send_to(&c, &self.hello());
                self.clients.insert(id, c);
                self.tap.clients.store(self.clients.len(), Ordering::Relaxed);
            }
            ClientMsg::Leave(id) => {
                if let Some(c) = self.clients.remove(&id) {
                    if !c.watch.is_empty() {
                        self.push_watch();
                    }
                }
                self.tap.clients.store(self.clients.len(), Ordering::Relaxed);
            }
            ClientMsg::Text(id, text) => match parse_request(&text) {
                Ok(Request::Watch(addrs)) => self.request(id, Request::Watch(addrs)),
                Ok(_) if self.clients.get(&id).is_some_and(|c| c.readonly) => {
                    log::warn!("console: read-only page {id} sent a command; ignored: {text}");
                }
                Ok(req) => self.request(id, req),
                Err(e) => log::warn!("console: bad message from page {id}: {e}"),
            },
        }
    }

    fn request(&mut self, id: u64, req: Request) {
        match req {
            Request::Watch(addrs) => {
                if let Some(c) = self.clients.get_mut(&id) {
                    c.watch = addrs.into_iter().collect();
                }
                self.push_watch();
            }
            Request::SetSync { signal_delay_ms, td_round_trip_ms } => {
                self.state.set_sync(signal_delay_ms, td_round_trip_ms);
                self.push_state();
                // Protocol rule: after any sync, follow or remap change every
                // page gets a fresh `hello`.
                self.broadcast(self.hello());
            }
            Request::SetFollow { dest, follow } => {
                match self.state.set_follow(&dest, self.kind_of(&dest), follow) {
                    Ok(()) => self.push_state(),
                    Err(e) => log::info!("console: {e}"),
                }
                // Also what snaps a refused switch back.
                self.broadcast(self.hello());
            }
            Request::Upsert(rule) => {
                match self.state.upsert(rule) {
                    Ok(_) => self.push_state(),
                    Err(e) => log::warn!("console: remap refused: {e}"),
                }
                // hello carries the rules, including a new rule's id.
                self.broadcast(self.hello());
            }
            Request::Delete(rid) => {
                if self.state.delete(rid) {
                    self.push_state();
                }
                self.broadcast(self.hello());
            }
            Request::Link(cmd) => {
                // The generator answers with a fresh `link` at once.
                if self.link.try_send(cmd).is_err() {
                    log::warn!("console: Link command dropped (no Link session)");
                }
            }
            Request::Save => {
                let reply = match &mut self.config {
                    None => protocol::saved(Err("the hub was started without a config file")),
                    Some(config) => match save_config(config, &self.state) {
                        Ok(path) => {
                            let shown = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
                            log::info!("console: saved sync + remap to {}", shown.display());
                            protocol::saved(Ok(&shown.display().to_string()))
                        }
                        Err(e) => {
                            log::warn!("console: save failed: {e:#}");
                            protocol::saved(Err(&format!("{e:#}")))
                        }
                    },
                };
                if let Some(c) = self.clients.get(&id) {
                    self.send_to(c, &reply.into());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_seen_table_keeps_every_address_in_a_stable_order() {
        let mut t = SeenTable::new();
        t.note("/b", 20, 0.2, DIR_IN);
        t.note("/c", 30, 0.3, DIR_OUT);
        t.note("/a", 40, 0.4, DIR_IN);
        t.note("/d", 50, 0.5, DIR_IN);
        assert_eq!(t.len(), 4); // nothing is ever evicted
        let list = t.snapshot(Instant::now());
        let addrs: Vec<&str> = list.iter().map(|a| a.addr.as_str()).collect();
        assert_eq!(addrs, ["/a", "/b", "/c", "/d"]); // by address, not by activity
        assert_eq!(list[0].last_ts, 40);
        assert!(list[0].rate > 0.0);
        // Traffic on one address never moves it: the order is the same
        // after a burst as before it.
        t.note("/d", 60, 0.9, DIR_IN);
        t.note("/d", 61, 0.9, DIR_IN);
        let list = t.snapshot(Instant::now());
        let addrs: Vec<&str> = list.iter().map(|a| a.addr.as_str()).collect();
        assert_eq!(addrs, ["/a", "/b", "/c", "/d"]);
        // The last value and the total survive the snapshot (the rate
        // window resets; the count never does).
        let a = list.iter().find(|x| x.addr == "/a").unwrap();
        assert_eq!((a.v, a.count), (0.4, 1));
        let d = list.iter().find(|x| x.addr == "/d").unwrap();
        assert_eq!((d.v, d.count), (0.9, 3));
    }

    #[test]
    fn an_address_tracks_the_min_and_max_of_its_values() {
        let mut t = SeenTable::new();
        t.note("/x", 1, 0.5, DIR_IN);
        let list = t.snapshot(Instant::now());
        // The first value initialises both.
        assert_eq!((list[0].min, list[0].max), (0.5, 0.5));
        t.note("/x", 2, -0.25, DIR_IN);
        t.note("/x", 3, 2.0, DIR_IN);
        t.note("/x", 4, 1.0, DIR_IN);
        let list = t.snapshot(Instant::now());
        assert_eq!((list[0].min, list[0].max), (-0.25, 2.0));
        assert_eq!(list[0].v, 1.0);
    }

    #[test]
    fn an_address_keeps_every_direction_it_was_seen_with() {
        let mut t = SeenTable::new();
        t.note("/in", 1, 0.0, DIR_IN);
        t.note("/out", 2, 0.0, DIR_OUT);
        t.note("/both", 3, 0.0, DIR_IN);
        t.note("/both", 4, 0.0, DIR_OUT);
        let dir = |list: &[AddrInfo], a: &str| list.iter().find(|x| x.addr == a).unwrap().dir;
        let list = t.snapshot(Instant::now());
        assert_eq!(dir(&list, "/in"), "in");
        assert_eq!(dir(&list, "/out"), "out");
        assert_eq!(dir(&list, "/both"), "both");
    }

    #[test]
    fn the_osc_endpoint_names_a_lan_address_and_the_port() {
        let e = osc_endpoint(9000);
        assert_eq!(e.port, 9000);
        // No LAN at all is legal (an empty list); loopback never is.
        assert!(e.ips.iter().all(|ip| !ip.starts_with("127.") && ip != "::1"), "{:?}", e.ips);
        if let Some(h) = &e.hostname {
            assert!(h.ends_with(".local"), "{h}");
        }
    }
}
