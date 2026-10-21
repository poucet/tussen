//! The router loop: receive on the OSC port, flatten bundles,
//! handle registration, deliver immediately — re-bundled per destination,
//! split at `max_packet` — and never block on a send.
//!
//! [`spawn`] runs the loop on its own thread and hands back a handle, so
//! the CLI and the in-process integration tests drive the identical code.
//!
//! Between routing and sending sit the console's two stages:
//! per-destination remap, then the destination's delay from the two sync
//! points. A destination with no rule and 0 ms delay takes the plain
//! immediate path; a delayed
//! message waits in that destination's [`DelayLine`] and is re-bundled
//! with whatever else is due when its time comes. The loop wakes for the
//! earliest due time, so delays land within about a millisecond.
//!
//! The console's websocket feed (`osc`/`midi` frames) hangs off the same
//! release point: its events wait in a [`DelayLine`] of their own for sync
//! point 1, exactly as td's datagrams do, so a page (the console, a shader
//! lab) gets each signal when the visuals destination does. Their `ts`
//! stays the receipt time.

use std::collections::{HashMap, HashSet};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::Context;
use crossbeam_channel::Receiver;
use simply_osc::{Inbound, Listener, Message};

use crate::bundles;
use crate::config::Config;
use crate::console::{self, Event, RouterCmd, RouterConsole, SeenTable};
use crate::generators::audio::{AudioLink, RtpTarget};
use crate::reach::{self, Conn};
use crate::router::{classify, resolve_host, HelloOutcome, HubMessage, Lookup, Router};
use crate::sync::{self, Class, DelayLine, SyncState};
use crate::record::{self, Slot};
use crate::take::Recorder;

/// A running hub. Dropping it stops the loop.
pub struct Hub {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Hub {
    /// Where the hub is listening — the way to learn the port after
    /// binding port 0 in a test.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Block until the loop ends (it doesn't, short of a stop or a dead
    /// socket) — what the CLI does after spawning.
    pub fn join(mut self) {
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Hub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Bind the OSC port, start the loop. `generated` is the channel the
/// generators publish into; pass a channel with no senders
/// left alive if there are none. Without `quiet` the loop prints a
/// stats line every second (`simply-tussen run --stats`); either way it
/// warns, at most once a minute, when something is going wrong
/// ([`Trouble`]).
pub fn spawn(config: Config, generated: Receiver<Message>, quiet: bool) -> anyhow::Result<Hub> {
    spawn_with(config, generated, quiet, Extras::default())
}

/// [`spawn`], plus the audio generator's [`AudioLink`]: the loop keeps
/// its RTP target list in step with device hellos/byes/timeouts and adds
/// its packet and overflow counters to the stats line.
pub fn spawn_with_audio(
    config: Config,
    generated: Receiver<Message>,
    quiet: bool,
    audio: Option<Arc<AudioLink>>,
) -> anyhow::Result<Hub> {
    spawn_with(config, generated, quiet, Extras { audio, console: None })
}

/// What a full `run` attaches to the router beyond the config.
#[derive(Default)]
pub struct Extras {
    /// The audio generator's link (RTP targets + counters).
    pub audio: Option<Arc<AudioLink>>,
    /// The hub console's live state + timeline taps.
    pub console: Option<RouterConsole>,
}

/// What recording and `simply-tussen replay` attach to the router.
#[derive(Default)]
pub struct Takes {
    /// Recording on demand ([`crate::record`]): while the slot holds a
    /// recorder, every routed message and sync change goes into it.
    pub record: Option<Slot>,
    /// `simply-tussen replay`: the replay's signals, routed as [`Src::Replay`].
    pub replay: Option<Receiver<Message>>,
}

/// Where a routed message came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Src {
    /// Received on the OSC port from this address.
    Net(SocketAddr),
    /// Made by a generator (MIDI, pad, clock).
    Hub,
    /// Played back from a take.
    Replay,
}

/// [`spawn`] with every optional attachment but takes.
pub fn spawn_with(config: Config, generated: Receiver<Message>, quiet: bool, extras: Extras) -> anyhow::Result<Hub> {
    spawn_with_takes(config, generated, quiet, extras, Takes::default())
}

/// [`spawn_with`], recording and/or replaying.
pub fn spawn_with_takes(
    config: Config,
    generated: Receiver<Message>,
    quiet: bool,
    extras: Extras,
    takes: Takes,
) -> anyhow::Result<Hub> {
    let listener = Listener::bind(("0.0.0.0", config.osc.listen_port))
        .with_context(|| format!("binding OSC port {}", config.osc.listen_port))?;
    let addr = listener.local_addr();
    let out = UdpSocket::bind("0.0.0.0:0").context("binding the send socket")?;
    out.set_nonblocking(true).context("nonblocking send socket")?;
    let router = Router::from_config(&config)?;
    if let Some(audio) = &extras.audio {
        let statics = router.static_rtp_targets();
        for t in &statics {
            log::info!("audio: RTP also to static destination {t}");
        }
        if let Ok(mut s) = audio.static_targets.lock() {
            *s = statics;
        }
    }
    let stop = Arc::new(AtomicBool::new(false));

    // `.local` laptops change IP (DHCP) and may boot after the hub:
    // re-resolve every static hostname off-thread (an mDNS miss can take
    // seconds) and let the loop apply the answers.
    // The console's ↻ kicks a round at once.
    let (kick, kicked) = crossbeam_channel::bounded::<()>(1);
    let resolved =
        spawn_resolver_with_kick(router.static_hostnames(), RESOLVE_PERIOD, Arc::clone(&stop), resolve_host, kicked);

    // The listener hands packets over a std channel; forward them into a
    // crossbeam one so the loop can wait on the network, the generators,
    // the console and the next delay-line deadline at once — a generator
    // message or a due delayed message is handled the moment it is ready.
    let (net_tx, net_rx) = crossbeam_channel::bounded::<Inbound>(1024);
    let malformed = Arc::new(AtomicU64::new(0));
    std::thread::Builder::new().name("simply-tussen recv".into()).spawn({
        let stop = Arc::clone(&stop);
        let malformed = Arc::clone(&malformed);
        move || {
            while !stop.load(Ordering::Relaxed) {
                if let Some(inbound) = listener.recv_timeout(Duration::from_millis(100)) {
                    if net_tx.send(inbound).is_err() {
                        break;
                    }
                }
                malformed.store(listener.dropped(), Ordering::Relaxed);
            }
        }
    })?;

    let thread = std::thread::Builder::new().name("simply-tussen router".into()).spawn({
        let stop = Arc::clone(&stop);
        move || {
            if !crate::timing::precise_wakeups() {
                log::warn!("router: precise timer wakeups refused; delays may land a few ms late");
            }
            let mut l = Loop::new(&config, router, extras, takes);
            l.hub_port = addr.port();
            l.resolved = resolved;
            l.kick = Some(kick);
            // The audio_out destinations are there from the start.
            sync_rtp_targets(l.audio.as_deref(), &l.router, &l.sync);
            l.run(&out, net_rx, generated, &malformed, &stop, quiet)
        }
    })?;
    Ok(Hub { addr, stop, thread: Some(thread) })
}

/// How often static destination hostnames are re-resolved.
pub const RESOLVE_PERIOD: Duration = Duration::from_secs(5);

/// How often the console's destination status is sent.
pub const STATUS_PERIOD: Duration = Duration::from_secs(1);

/// One re-resolve round: (destination name, address or None).
pub type Resolved = Vec<(String, Option<SocketAddr>)>;

/// [`spawn_resolver_with_kick`] with nothing to kick it.
pub fn spawn_resolver(
    hosts: Vec<(String, String, u16)>,
    period: Duration,
    stop: Arc<AtomicBool>,
    resolve: impl Fn(&str, u16) -> Option<SocketAddr> + Send + Sync + 'static,
) -> Receiver<Resolved> {
    spawn_resolver_with_kick(hosts, period, stop, resolve, crossbeam_channel::never())
}

/// Resolve `hosts` ((name, host, port)) on a thread of its own: once
/// straight away, then every `period`, and at once whenever `kick`
/// yields (the console's ↻). Each distinct host is looked up once per
/// round, all of them in parallel, and each answer is sent the moment it
/// arrives — so one `.local` laptop that is off the network (a ~5 s
/// miss) holds up nobody else. No hostnames = no thread (the receiver
/// never yields). Stops with `stop`.
pub fn spawn_resolver_with_kick(
    hosts: Vec<(String, String, u16)>,
    period: Duration,
    stop: Arc<AtomicBool>,
    resolve: impl Fn(&str, u16) -> Option<SocketAddr> + Send + Sync + 'static,
    kick: Receiver<()>,
) -> Receiver<Resolved> {
    if hosts.is_empty() {
        return crossbeam_channel::never();
    }
    let mut by_host: Vec<(String, Vec<(String, u16)>)> = Vec::new();
    for (name, host, port) in hosts {
        match by_host.iter_mut().find(|(h, _)| *h == host) {
            Some((_, dests)) => dests.push((name, port)),
            None => by_host.push((host, vec![(name, port)])),
        }
    }
    let (tx, rx) = crossbeam_channel::bounded::<Resolved>(64);
    let spawned = std::thread::Builder::new().name("simply-tussen resolve".into()).spawn(move || {
        let mut kick = kick;
        let mut next = Instant::now();
        while !stop.load(Ordering::Relaxed) {
            let now = Instant::now();
            if now < next {
                match kick.recv_timeout((next - now).min(Duration::from_millis(50))) {
                    Ok(()) => while kick.try_recv().is_ok() {},
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                        kick = crossbeam_channel::never();
                        continue;
                    }
                }
            }
            next = Instant::now() + period;
            let gone = AtomicBool::new(false);
            std::thread::scope(|s| {
                for (host, dests) in &by_host {
                    let (tx, resolve, gone) = (&tx, &resolve, &gone);
                    s.spawn(move || {
                        let ip = resolve(host, 0).map(|a| a.ip());
                        let round = dests.iter().map(|(n, p)| (n.clone(), ip.map(|ip| SocketAddr::new(ip, *p)))).collect();
                        if tx.send(round).is_err() {
                            gone.store(true, Ordering::Relaxed);
                        }
                    });
                }
            });
            if gone.load(Ordering::Relaxed) {
                break;
            }
        }
    });
    if let Err(e) = spawned {
        log::warn!("no hostname re-resolver: {e}");
    }
    rx
}

#[derive(Default)]
struct Stats {
    msgs_in: u64,
    msgs_out: u64,
    send_errors: u64,
    dropped_at_last_report: u64,
    audio_at_last_report: u64,
    trouble: TroubleLog,
}

/// The counters that mean something is wrong, as running totals.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Trouble {
    /// Packets the listener could not parse.
    malformed: u64,
    /// Delivery rounds where a send failed.
    send_errors: u64,
    /// Audio packets lost to a full queue.
    audio_overflows: u64,
    /// Messages a full delay line dropped.
    delay_drops: u64,
    /// Console events dropped because a page fell behind.
    console_drops: u64,
}

impl Trouble {
    fn counts(&self) -> [(u64, &'static str); 5] {
        [
            (self.malformed, "malformed packet(s)"),
            (self.send_errors, "failed send round(s)"),
            (self.audio_overflows, "audio overflow(s)"),
            (self.delay_drops, "delay-line drop(s)"),
            (self.console_drops, "console drop(s)"),
        ]
    }

    /// What grew from `last` to `self`. A total that shrank (a delay
    /// line removed with its count) restarted from zero.
    fn since(&self, last: &Trouble) -> Trouble {
        let d = |now: u64, then: u64| if now >= then { now - then } else { now };
        Trouble {
            malformed: d(self.malformed, last.malformed),
            send_errors: d(self.send_errors, last.send_errors),
            audio_overflows: d(self.audio_overflows, last.audio_overflows),
            delay_drops: d(self.delay_drops, last.delay_drops),
            console_drops: d(self.console_drops, last.console_drops),
        }
    }

    fn add(&mut self, o: &Trouble) {
        self.malformed += o.malformed;
        self.send_errors += o.send_errors;
        self.audio_overflows += o.audio_overflows;
        self.delay_drops += o.delay_drops;
        self.console_drops += o.console_drops;
    }

    /// `3 malformed packet(s), 1 failed send round(s)`; `None` if all zero.
    fn describe(&self) -> Option<String> {
        let parts: Vec<String> =
            self.counts().iter().filter(|(n, _)| *n > 0).map(|(n, what)| format!("{n} {what}")).collect();
        (!parts.is_empty()).then(|| parts.join(", "))
    }
}

/// How often a persisting problem is repeated.
const TROUBLE_EVERY: Duration = Duration::from_secs(60);

/// Turns [`Trouble`] totals into rare warnings: the first growth at once,
/// then whatever piled up, at most every [`TROUBLE_EVERY`].
#[derive(Debug, Default)]
struct TroubleLog {
    last: Trouble,
    pending: Trouble,
    warned: Option<Instant>,
}

impl TroubleLog {
    fn tick(&mut self, totals: Trouble, now: Instant) -> Option<String> {
        self.pending.add(&totals.since(&self.last));
        self.last = totals;
        if self.warned.is_some_and(|w| now.duration_since(w) < TROUBLE_EVERY) {
            return None;
        }
        let what = self.pending.describe()?;
        let window = match self.warned {
            Some(w) => format!("in the last {} s", now.duration_since(w).as_secs()),
            None => "since start".into(),
        };
        self.pending = Trouble::default();
        self.warned = Some(now);
        Some(format!("{what} {window} (simply-tussen run --stats shows the per-second counts)"))
    }
}

/// The router loop's state: routing, the live sync state, the delay
/// lines and the console's view.
struct Loop {
    router: Router,
    max_packet: usize,
    audio: Option<Arc<AudioLink>>,
    /// Sync points + remap rules; swapped whole when the console changes them.
    sync: Arc<SyncState>,
    /// One timestamped queue per destination address.
    lines: HashMap<SocketAddr, DelayLine>,
    console: Option<ConsoleSide>,
    /// Reused per iteration: encoded messages grouped by destination.
    per_dest: HashMap<SocketAddr, Vec<Vec<u8>>>,
    stats: Stats,
    /// The OSC port the hub listens on — carried in every welcome, since
    /// replies leave from the send socket's ephemeral port.
    hub_port: u16,
    /// Static destination re-resolve rounds ([`spawn_resolver`]).
    resolved: Receiver<Resolved>,
    /// The console's ↻ → the resolver: look every name up now.
    kick: Option<crossbeam_channel::Sender<()>>,
    /// One connected socket per static destination address, so the
    /// kernel's "nobody listening" reaches the status column ([`reach`]).
    conns: HashMap<SocketAddr, Conn>,
    /// Datagram totals at the last status, for the rates: OSC by
    /// destination address, RTP by stream address.
    last_osc: HashMap<SocketAddr, u64>,
    last_rtp: HashMap<SocketAddr, u64>,
    last_status: Instant,
    /// Publish the status on the next round (a lookup answered, ↻).
    status_due: bool,
    /// The take being written, and where to look for a new one.
    recorder: Option<Arc<Recorder>>,
    record: Option<Slot>,
    /// The last message routed per address, with its source — what a
    /// take started mid-run is seeded with. Kept only if recording can start.
    last: HashMap<String, (Option<SocketAddr>, Message)>,
    replay: Option<Receiver<Message>>,
}

/// The router's half of the console: commands in, taps out, the seen
/// table and the watch list.
struct ConsoleSide {
    link: RouterConsole,
    seen: SeenTable,
    watch: Arc<HashSet<String>>,
    last_addrs: Instant,
    /// `osc`/`midi` events held for sync point 1 ([`feed_delay_ms`]).
    line: DelayLine<Event>,
    /// Reused per round: the events whose time has come.
    released: Vec<Event>,
}

impl Loop {
    fn new(config: &Config, router: Router, extras: Extras, takes: Takes) -> Loop {
        Loop {
            recorder: None,
            record: takes.record,
            last: HashMap::new(),
            replay: takes.replay,
            router,
            max_packet: config.osc.max_packet,
            audio: extras.audio,
            sync: Arc::new(SyncState::new(config.sync.clone(), config.remap.clone())),
            lines: HashMap::new(),
            console: extras.console.map(|link| ConsoleSide {
                link,
                seen: SeenTable::new(),
                watch: Arc::new(HashSet::new()),
                last_addrs: Instant::now(),
                line: DelayLine::default(),
                released: Vec::new(),
            }),
            per_dest: HashMap::new(),
            stats: Stats::default(),
            hub_port: config.osc.listen_port,
            resolved: crossbeam_channel::never(),
            kick: None,
            conns: HashMap::new(),
            last_osc: HashMap::new(),
            last_rtp: HashMap::new(),
            last_status: Instant::now(),
            status_due: true,
        }
    }

    /// Every static destination's status, for the console: polls the
    /// sockets for errors the kernel holds, drops the sockets of addresses
    /// no destination uses any more, and — while a page is open — sends
    /// the list. Called about once a second and when something changed.
    fn publish_status(&mut self, now: Instant) {
        self.status_due = false;
        let secs = now.duration_since(self.last_status).as_secs_f32().max(0.05);
        self.last_status = now;
        let live: HashSet<SocketAddr> = self.router.statics.iter().filter(|s| s.resolved).map(|s| s.addr).collect();
        self.conns.retain(|a, _| live.contains(a));
        for c in self.conns.values_mut() {
            c.poll(now);
        }
        let rtp_reach: HashMap<SocketAddr, reach::Reach> =
            self.audio.as_ref().and_then(|a| a.static_reach.lock().ok().map(|m| m.clone())).unwrap_or_default();
        let rate = |last: &mut HashMap<SocketAddr, u64>, addr: SocketAddr, sent: u64| {
            // A socket new since the last status sent all of it in this window.
            let before = last.insert(addr, sent).unwrap_or(0);
            sent.saturating_sub(before) as f32 / secs
        };
        let mut list = Vec::with_capacity(self.router.statics.len());
        for s in &self.router.statics {
            let addr = s.resolved.then_some(s.addr);
            let osc_reach = addr.and_then(|a| self.conns.get(&a)).map(|c| &c.reach);
            let osc = reach::Seen {
                reach: osc_reach,
                rate: match (addr, osc_reach) {
                    (Some(a), Some(r)) => rate(&mut self.last_osc, a, r.sent),
                    _ => 0.0,
                },
            };
            let rtp = s.rtp_port.map(|port| {
                let at = addr.map(|a| SocketAddr::new(a.ip(), port));
                let r = at.and_then(|a| rtp_reach.get(&a));
                let rate = match (at, r) {
                    (Some(a), Some(r)) => rate(&mut self.last_rtp, a, r.sent),
                    _ => 0.0,
                };
                (port, reach::Seen { reach: r, rate })
            });
            let d = reach::Dest { name: &s.name, host: &s.host, port: s.port, addr, lookup: s.lookup };
            list.push(reach::status(d, osc, rtp, now));
        }
        if let Some(c) = &self.console {
            if c.link.tap.active() {
                c.link.tap.send(Event::DestStatus(list));
            }
        }
    }

    /// Apply a re-resolve round: a static destination whose name now
    /// points elsewhere is sent to the new address from here on.
    fn apply_resolved(&mut self, round: Resolved) {
        let mut changed = false;
        for (name, addr) in round {
            let before = self.router.statics.iter().find(|s| s.name == name).map(|s| (s.lookup, s.host.clone()));
            if let Some(a) = self.router.set_static_addr(&name, addr) {
                log::info!("destination {name} now at {a}");
                changed = true;
            }
            match (addr, before) {
                (None, Some((Lookup::Pending | Lookup::Ok, host))) => {
                    log::warn!("destination {name}: {host} doesn't resolve; retrying every {} s", RESOLVE_PERIOD.as_secs())
                }
                (Some(_), Some((Lookup::Failed, _))) if !changed => log::info!("destination {name} resolves again"),
                _ => {}
            }
            self.status_due = true;
        }
        if changed {
            if let Some(audio) = &self.audio {
                if let Ok(mut s) = audio.static_targets.lock() {
                    *s = self.router.static_rtp_targets();
                }
            }
        }
    }

    /// Answer a hello with `/tussen/hub/welcome` (hub OSC port, device
    /// name, TD's IPv4 or "") at the device's registered address: how a device that
    /// broadcast its hello learns where the hub is. Queued with this
    /// round's datagrams for that device.
    fn welcome(&mut self, args: &[simply_osc::Value]) {
        let Some(simply_osc::Value::Str(name)) = args.first() else { return };
        let Some(addr) = self.router.devices().find(|d| d.name == *name).map(|d| d.addr) else { return };
        let m = Message::new("/tussen/hub/welcome")
            .arg(simply_osc::Value::Int32(self.hub_port as i32))
            .arg(simply_osc::Value::Str(name.clone()))
            .arg(simply_osc::Value::Str(self.router.td_ipv4()));
        if let Ok(b) = m.encode() {
            self.per_dest.entry(addr).or_default().push(b);
        }
    }

    /// The earliest due time across every delay line.
    fn next_due(&self) -> Option<Instant> {
        let feed = self.console.as_ref().and_then(|c| c.line.next_due());
        self.lines.values().filter_map(DelayLine::next_due).chain(feed).min()
    }

    fn run(
        &mut self,
        out: &UdpSocket,
        mut net: Receiver<Inbound>,
        mut generated: Receiver<Message>,
        malformed: &AtomicU64,
        stop: &AtomicBool,
        quiet: bool,
    ) {
        let mut cmds = match &self.console {
            Some(c) => c.link.cmds.clone(),
            None => crossbeam_channel::never(),
        };
        let mut resolved = std::mem::replace(&mut self.resolved, crossbeam_channel::never());
        let mut replay = self.replay.take().unwrap_or_else(crossbeam_channel::never);
        let mut last_report = Instant::now();

        while !stop.load(Ordering::Relaxed) {
            // Wait for whatever comes first: a packet, a generator
            // message, a console command, the next due delayed message,
            // or the 20 ms housekeeping tick. A closed channel is
            // replaced by `never` so it can't spin the loop.
            let tick = Duration::from_millis(20);
            let wait = self.next_due().map_or(tick, |d| d.saturating_duration_since(Instant::now()).min(tick));
            let mut inbound = None;
            let mut first_gen = None;
            let mut first_replay = None;
            crossbeam_channel::select! {
                recv(net) -> r => match r {
                    Ok(i) => inbound = Some(i),
                    Err(_) => net = crossbeam_channel::never(),
                },
                recv(generated) -> r => match r {
                    Ok(m) => first_gen = Some(m),
                    Err(_) => generated = crossbeam_channel::never(),
                },
                recv(replay) -> r => match r {
                    Ok(m) => first_replay = Some(m),
                    Err(_) => replay = crossbeam_channel::never(),
                },
                recv(cmds) -> r => match r {
                    Ok(c) => self.command(c),
                    Err(_) => cmds = crossbeam_channel::never(),
                },
                recv(resolved) -> r => match r {
                    Ok(round) => self.apply_resolved(round),
                    Err(_) => resolved = crossbeam_channel::never(),
                },
                default(wait) => {}
            }
            let now = Instant::now();
            self.follow_recorder();

            if let Some(inbound) = inbound {
                self.inbound(&inbound, now);
            }
            for m in first_gen.into_iter().chain(std::iter::from_fn(|| generated.try_recv().ok())) {
                self.stats.msgs_in += 1;
                self.route(&m, Src::Hub, now);
            }
            for m in first_replay.into_iter().chain(std::iter::from_fn(|| replay.try_recv().ok())) {
                self.stats.msgs_in += 1;
                self.route(&m, Src::Replay, now);
            }
            while let Ok(c) = cmds.try_recv() {
                self.command(c);
            }

            // Delayed messages whose time has come join this round.
            let now = Instant::now();
            for (dest, line) in self.lines.iter_mut() {
                if line.next_due().is_some_and(|d| d <= now) {
                    line.pop_due(now, self.per_dest.entry(*dest).or_default());
                }
            }
            self.deliver(out);
            if let Some(c) = &mut self.console {
                c.release(now);
            }

            let pruned = self.router.prune(now);
            if !pruned.is_empty() {
                self.registration_changed();
            }
            for name in pruned {
                log::info!("device {name} timed out");
            }

            if let Some(c) = &mut self.console {
                if now.duration_since(c.last_addrs) >= console::ADDRS_PERIOD {
                    c.last_addrs = now;
                    if c.link.tap.active() {
                        // Reliable: a dropped delta would be lost until that address
                        // changes again (a full list used to heal it 500 ms on).
                        c.link.tap.control(Event::Addrs(c.seen.changes(now)));
                    }
                }
            }

            let elapsed = now.duration_since(last_report);
            if elapsed >= Duration::from_secs(1) {
                self.report(elapsed, malformed.load(Ordering::Relaxed), quiet);
                last_report = now;
            }
            if self.status_due || now.duration_since(self.last_status) >= STATUS_PERIOD {
                self.publish_status(now);
            }
        }
    }

    /// Pick up a take started or stopped since the last round. A new one
    /// gets the sync state and the last value of every address at its t = 0.
    fn follow_recorder(&mut self) {
        let Some(slot) = &self.record else { return };
        let now = record::current(slot);
        if !record::changed(&self.recorder, &now) {
            return;
        }
        if let Some(r) = &now {
            let t0 = r.start_instant();
            r.sync(t0, self.sync.sync.signal_delay_ms, self.sync.sync.td_round_trip_ms);
            for (from, m) in self.last.values() {
                r.osc(t0, *from, m);
            }
        }
        self.recorder = now;
    }

    fn command(&mut self, c: RouterCmd) {
        match c {
            RouterCmd::Sync(state) => {
                self.sync = state;
                if let Some(r) = &self.recorder {
                    r.sync(Instant::now(), self.sync.sync.signal_delay_ms, self.sync.sync.td_round_trip_ms);
                }
                // A device's audio delay follows the round trip and its switch.
                sync_rtp_targets(self.audio.as_deref(), &self.router, &self.sync);
            }
            RouterCmd::Watch(w) => {
                if let Some(c) = &mut self.console {
                    c.watch = w;
                }
            }
            RouterCmd::Resolve => {
                // Look every name up now and forget earlier send errors, so
                // one from before the receiver started stops showing.
                log::info!("console: re-resolving destinations");
                if let Some(k) = &self.kick {
                    let _ = k.try_send(());
                }
                self.router.mark_lookups_pending();
                for c in self.conns.values_mut() {
                    c.forget_errors();
                }
                if let Some(a) = &self.audio {
                    a.reach_epoch.fetch_add(1, Ordering::Relaxed);
                    if let Ok(mut m) = a.static_reach.lock() {
                        for r in m.values_mut() {
                            r.last_err = None;
                        }
                    }
                }
                self.status_due = true;
            }
        }
    }

    fn inbound(&mut self, inbound: &Inbound, now: Instant) {
        for (_, m) in inbound.packet.messages() {
            self.stats.msgs_in += 1;
            match classify(m) {
                HubMessage::Hello(args) => match self.router.hello(inbound.from.ip(), args, now) {
                    Ok(HelloOutcome::Registered) => {
                        if let Some(d) = self.router.devices().find(|d| {
                            matches!(args.first(), Some(simply_osc::Value::Str(n)) if *n == d.name)
                        }) {
                            log::info!("device {} registered at {}", d.name, d.addr);
                        }
                        self.registration_changed();
                    }
                    // A refresh may carry a new RTP port: resync audio
                    // (cheap), but the console's device list is unchanged.
                    Ok(HelloOutcome::Refreshed) => sync_rtp_targets(self.audio.as_deref(), &self.router, &self.sync),
                    Err(e) => log::warn!("bad hello from {}: {e:#}", inbound.from),
                },
                HubMessage::Bye(args) => {
                    if let Some(name) = self.router.bye(args) {
                        log::info!("device {name} said bye");
                        self.registration_changed();
                    }
                }
                HubMessage::Other => {}
            }
            // Every hello gets a welcome back — broadcast or not.
            if let HubMessage::Hello(args) = classify(m) {
                self.welcome(args);
            }
            self.route(m, Src::Net(inbound.from), now);
        }
    }

    fn registration_changed(&self) {
        sync_rtp_targets(self.audio.as_deref(), &self.router, &self.sync);
        if let Some(c) = &self.console {
            c.link.tap.control(Event::Devices(self.router.device_names()));
        }
    }

    /// Route one message: console taps, then per destination remap →
    /// delay → queue. A destination with 0 ms delay and no matching rule
    /// takes exactly the original path: one shared encoding, straight
    /// into this round's datagrams.
    fn route(&mut self, m: &Message, from: Src, now: Instant) {
        let class = sync::class_of(&m.address);
        let from_addr = if let Src::Net(a) = from { Some(a) } else { None };
        if let Some(r) = &self.recorder {
            r.osc(now, from_addr, m);
        }
        if self.record.is_some() && !crate::replay::is_skipped(&m.address) && !crate::replay::is_event(&m.address) {
            match self.last.get_mut(&m.address) {
                Some(v) => *v = (from_addr, m.clone()),
                None => {
                    self.last.insert(m.address.clone(), (from_addr, m.clone()));
                }
            }
        }
        let console = &mut self.console;
        if let Some(c) = console.as_mut() {
            tap(c, m, from, class, now, feed_delay_ms(&self.sync));
        }
        let mut encoded: Option<Vec<u8>> = None;
        let (per_dest, lines) = (&mut self.per_dest, &mut self.lines);
        let mut queue = |dest: SocketAddr, delay_ms: u32, bytes: Vec<u8>| {
            if delay_ms == 0 {
                per_dest.entry(dest).or_default().push(bytes);
            } else {
                lines.entry(dest).or_default().push(sync::due(now, delay_ms), bytes);
            }
        };
        for t in self.router.routes(&m.address) {
            let delay = self.sync.delay_ms(t.name, t.kind);
            let remapped = self.sync.remap(t.name, m);
            let send_original = remapped.as_ref().is_none_or(|(keep, _)| *keep);
            if send_original {
                if encoded.is_none() {
                    match m.encode() {
                        Ok(b) => encoded = Some(b),
                        // A parsed message always re-encodes; a generator
                        // could hand us a bad address. Count-by-log,
                        // never crash.
                        Err(e) => {
                            log::warn!("unencodable message {}: {e}", m.address);
                            return;
                        }
                    }
                }
                if let Some(b) = &encoded {
                    queue(t.addr, delay, b.clone());
                }
            }
            for r in remapped.map(|(_, v)| v).unwrap_or_default() {
                // A remap's output address exists only here — it never
                // appears inbound, so this is where the seen table learns it.
                if let Some(c) = console.as_mut() {
                    let v = r.args.first().and_then(simply_osc::Value::as_float).unwrap_or(0.0);
                    c.seen.note(&r.address, console::us_at(now), v, console::DIR_OUT);
                }
                match r.encode() {
                    Ok(b) => queue(t.addr, delay, b),
                    Err(e) => log::warn!("unencodable remap {} → {}: {e}", m.address, r.address),
                }
            }
        }
    }

    /// Re-bundle per destination, split at max_packet, never block — a
    /// full buffer or an unreachable destination is a count.
    fn deliver(&mut self, out: &UdpSocket) {
        let now = Instant::now();
        let (per_dest, conns, router, stats) = (&mut self.per_dest, &mut self.conns, &self.router, &mut self.stats);
        for (dest, encoded) in per_dest.iter_mut() {
            if encoded.is_empty() {
                continue;
            }
            let n = encoded.len() as u64;
            let mut failed = false;
            // A static destination goes out on its own connected socket
            // (errors are seen); devices share the send socket.
            let is_static = router.statics.iter().any(|s| s.resolved && s.addr == *dest);
            for datagram in bundles::pack(encoded, self.max_packet) {
                let sent = if is_static {
                    conns.entry(*dest).or_insert_with(|| Conn::new(*dest)).send(&datagram, now)
                } else {
                    out.send_to(&datagram, dest).is_ok()
                };
                failed |= !sent;
            }
            encoded.clear();
            if failed {
                stats.send_errors += 1;
            } else {
                stats.msgs_out += n;
            }
        }
        // Devices come and go: don't keep buffers for every one ever seen.
        if self.per_dest.len() > 64 {
            self.per_dest.clear();
        }
        self.lines.retain(|_, l| !l.is_empty());
    }

    /// Items every delay line dropped, the console feed's included.
    fn delay_drops(&self) -> u64 {
        let feed = self.console.as_ref().map_or(0, |c| c.line.dropped);
        self.lines.values().map(|l| l.dropped).sum::<u64>() + feed
    }

    fn report(&mut self, elapsed: Duration, dropped: u64, quiet: bool) {
        let secs = elapsed.as_secs_f64();
        let delay_drops = self.delay_drops();
        let totals = Trouble {
            malformed: dropped,
            send_errors: self.stats.send_errors,
            audio_overflows: self.audio.as_ref().map_or(0, |a| a.overflows.load(Ordering::Relaxed)),
            delay_drops,
            console_drops: self.console.as_ref().map_or(0, |c| c.link.tap.dropped()),
        };
        if let Some(w) = self.stats.trouble.tick(totals, Instant::now()) {
            log::warn!("hub: {w}");
        }
        let stats = &mut self.stats;
        if !quiet {
            let audio_stats = self
                .audio
                .as_ref()
                .map(|a| {
                    let packets = a.packets.load(Ordering::Relaxed);
                    let rate = (packets - stats.audio_at_last_report) as f64 / secs;
                    stats.audio_at_last_report = packets;
                    format!(" audio={rate:.1}pkt/s audio_ovf={}", a.overflows.load(Ordering::Relaxed))
                })
                .unwrap_or_default();
            let feed = self.console.as_ref().map_or(0, |c| c.line.len());
            let queued: usize = self.lines.values().map(DelayLine::len).sum::<usize>() + feed;
            let delayed = if queued > 0 || delay_drops > 0 {
                format!(" delayed={queued} delay_drop={delay_drops}")
            } else {
                String::new()
            };
            let console = self
                .console
                .as_ref()
                .map(|c| format!(" console_drop={}", c.link.tap.dropped()))
                .unwrap_or_default();
            println!(
                "devices={} in={:.0}/s out={:.0}/s{audio_stats}{delayed}{console} malformed={} send_errors={}",
                self.router.device_count(),
                stats.msgs_in as f64 / secs,
                stats.msgs_out as f64 / secs,
                dropped - stats.dropped_at_last_report,
                stats.send_errors,
            );
        }
        stats.msgs_in = 0;
        stats.msgs_out = 0;
        stats.dropped_at_last_report = dropped;
    }
}

impl ConsoleSide {
    /// Publish the feed events whose time has come — in the same round,
    /// from the same `now`, as the destinations' delayed datagrams.
    fn release(&mut self, now: Instant) {
        if self.line.next_due().is_none_or(|d| d > now) {
            return;
        }
        self.line.pop_due(now, &mut self.released);
        let active = self.link.tap.active();
        for ev in self.released.drain(..) {
            if active {
                self.link.tap.send(ev);
            }
        }
    }
}

/// How long the websocket feed holds a signal: sync point 1, what the
/// visuals destination (td) gets. A page sits where td sits — on the
/// hub's machine, with the audio coming back through the same mixer — so
/// it takes td's delay, not a device's round trip (which lines a remote
/// device up with td's NDI picture, and a page draws its own).
fn feed_delay_ms(sync: &SyncState) -> u32 {
    sync.delay_ms("", sync::DestKind::Td)
}

/// Feed the console: every address into the seen table — inbound ones
/// (and replayed ones) as `in`, hub-generated ones (/tussen/clock/…,
/// /tussen/midi/…) as `out`; MIDI-class signals, and watched OSC
/// addresses, as timeline events — only while a page is connected. The
/// seen table is noted at once; the events wait `delay_ms` (sync point 1)
/// in the console's delay line, like td's datagrams. Timestamps are
/// receipt time, before any delay.
fn tap(c: &mut ConsoleSide, m: &Message, from: Src, class: Class, now: Instant, delay_ms: u32) {
    if m.address.starts_with("/tussen/hub/") {
        return;
    }
    let ts = console::us_at(now);
    let v = m.args.first().and_then(simply_osc::Value::as_float).unwrap_or(0.0);
    c.seen.note(&m.address, ts, v, if from == Src::Hub { console::DIR_OUT } else { console::DIR_IN });
    if !c.link.tap.active() {
        return;
    }
    let ev = if class == Class::Midi {
        Event::Midi { ts, addr: m.address.clone(), v }
    } else if c.watch.contains("*") || c.watch.contains(&m.address) {
        let src = match from {
            Src::Net(a) => a.to_string(),
            Src::Hub => "hub".to_owned(),
            Src::Replay => "replay".to_owned(),
        };
        Event::Osc { ts, addr: m.address.clone(), v, src }
    } else {
        return;
    };
    if delay_ms == 0 {
        c.link.tap.send(ev);
    } else {
        c.line.push(sync::due(now, delay_ms), ev);
    }
}

/// Hand the audio sender the current RTP target list (devices whose
/// hello carried a nonzero rtp_port, and the `audio_out` destinations),
/// each with its audio delay
/// — called whenever registration or the sync state changes, so
/// the stream follows hellos, byes, timeouts and the console.
fn sync_rtp_targets(audio: Option<&AudioLink>, router: &Router, sync: &SyncState) {
    if let Some(a) = audio {
        let list: Vec<RtpTarget> = router
            .devices()
            .filter(|d| d.rtp_port != 0)
            .map(|d| RtpTarget {
                addr: SocketAddr::new(d.addr.ip(), d.rtp_port),
                delay_ms: sync.audio_delay_ms(&d.name, sync::DestKind::Device),
            })
            .chain(a.outputs.lock().map(|o| o.clone()).unwrap_or_default().into_iter().map(|o| RtpTarget {
                addr: o.addr,
                delay_ms: sync.audio_delay_ms(&o.name, o.kind),
            }))
            .collect();
        if let Ok(mut targets) = a.targets.lock() {
            *targets = list;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trouble_warns_at_once_then_at_most_once_a_minute() {
        let t0 = Instant::now();
        let mut log = TroubleLog::default();
        let at = |s: u64| t0 + Duration::from_secs(s);
        // All quiet: nothing.
        assert_eq!(log.tick(Trouble::default(), at(1)), None);
        // The first problem: at once.
        let w = log.tick(Trouble { malformed: 3, ..Trouble::default() }, at(2)).unwrap();
        assert!(w.starts_with("3 malformed packet(s) since start"), "{w}");
        // More within the minute: held back, then summed.
        assert_eq!(log.tick(Trouble { malformed: 5, send_errors: 1, ..Trouble::default() }, at(10)), None);
        assert_eq!(log.tick(Trouble { malformed: 6, send_errors: 1, ..Trouble::default() }, at(30)), None);
        let w = log.tick(Trouble { malformed: 6, send_errors: 1, ..Trouble::default() }, at(62)).unwrap();
        assert!(w.starts_with("3 malformed packet(s), 1 failed send round(s) in the last 60 s"), "{w}");
        // Quiet again: nothing, however long.
        assert_eq!(log.tick(Trouble { malformed: 6, send_errors: 1, ..Trouble::default() }, at(500)), None);
    }

    #[test]
    fn a_shrinking_total_counts_from_zero() {
        let last = Trouble { delay_drops: 9, ..Trouble::default() };
        assert_eq!(Trouble { delay_drops: 2, ..Trouble::default() }.since(&last).delay_drops, 2);
        assert_eq!(Trouble::default().describe(), None);
    }
}
