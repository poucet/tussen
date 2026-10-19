//! Reverse DNS for the console's Link peer list: an address's name, looked
//! up off-thread and cached, so asking never blocks the caller.
//!
//! A reverse lookup can take seconds (an mDNS miss, a router that does not
//! answer PTR queries), and the asker is the Link thread, which must keep
//! its beat. So [`Names::get`] answers from the cache at once — `None`
//! while a lookup is in flight — and starts one on a thread of its own when
//! the address is new or its answer has aged: a name is kept for
//! [`FOUND_FOR`], a miss is retried after [`MISSED_FOR`].

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long a found name is trusted before it is looked up again.
pub const FOUND_FOR: Duration = Duration::from_secs(600);
/// How long before an address with no name is tried again.
pub const MISSED_FOR: Duration = Duration::from_secs(60);

#[derive(Clone, Debug)]
struct Entry {
    /// The last answer, kept while a fresh lookup runs.
    name: Option<String>,
    /// When it was answered; `None` while a lookup is in flight.
    at: Option<Instant>,
}

type Lookup = dyn Fn(IpAddr) -> Option<String> + Send + Sync;

/// A cache of reverse lookups. Cheap to clone; clones share the cache.
#[derive(Clone)]
pub struct Names {
    lookup: Arc<Lookup>,
    cache: Arc<Mutex<HashMap<IpAddr, Entry>>>,
}

impl Names {
    /// Names from the system resolver ([`system`]).
    pub fn system() -> Names {
        Names::with(system)
    }

    /// Names from `lookup`, which may block: it only ever runs off-thread.
    pub fn with(lookup: impl Fn(IpAddr) -> Option<String> + Send + Sync + 'static) -> Names {
        Names { lookup: Arc::new(lookup), cache: Arc::default() }
    }

    /// `ip`'s name as far as is known right now; never waits.
    pub fn get(&self, ip: IpAddr) -> Option<String> {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        let due = match cache.get(&ip) {
            None => true,
            Some(Entry { at: None, .. }) => false,
            Some(Entry { name, at: Some(at) }) => {
                at.elapsed() >= if name.is_some() { FOUND_FOR } else { MISSED_FOR }
            }
        };
        let entry = cache.entry(ip).or_insert(Entry { name: None, at: None });
        if due {
            entry.at = None;
            let (lookup, cache) = (Arc::clone(&self.lookup), Arc::clone(&self.cache));
            let spawned = std::thread::Builder::new().name("simply-tussen rdns".into()).spawn(move || {
                let name = lookup(ip);
                let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
                cache.insert(ip, Entry { name, at: Some(Instant::now()) });
            });
            if spawned.is_err() {
                entry.at = Some(Instant::now());
            }
        }
        entry.name.clone()
    }
}

/// The system resolver's name for `ip` (`getnameinfo`), without a trailing
/// dot; `None` if it has none or only echoes the address back.
pub fn system(ip: IpAddr) -> Option<String> {
    let name = dns_lookup::lookup_addr(&ip).ok()?;
    let name = name.trim_end_matches('.');
    (!name.is_empty() && name.parse::<IpAddr>().is_err()).then(|| name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    /// Wait (briefly) for an off-thread lookup to land.
    fn settled(names: &Names, at: IpAddr) -> Option<String> {
        for _ in 0..200 {
            if let Some(n) = names.get(at) {
                return Some(n);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        None
    }

    #[test]
    fn a_name_is_looked_up_once_off_thread_and_then_cached() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (gate_tx, gate_rx) = crossbeam_channel::bounded::<()>(0);
        let names = {
            let calls = Arc::clone(&calls);
            Names::with(move |a| {
                calls.fetch_add(1, Ordering::SeqCst);
                let _ = gate_rx.recv(); // a resolver that takes as long as the test likes
                (a == ip("192.0.2.21")).then(|| "laptop.local".to_owned())
            })
        };
        let laptop = ip("192.0.2.21");
        // The lookup is stuck, and asking does not wait for it.
        assert_eq!(names.get(laptop), None);
        assert_eq!(names.get(laptop), None, "still in flight; no second lookup");
        gate_tx.send(()).unwrap();
        assert_eq!(settled(&names, laptop).as_deref(), Some("laptop.local"));
        assert_eq!(names.get(laptop).as_deref(), Some("laptop.local"));
        assert_eq!(calls.load(Ordering::SeqCst), 1, "answered from the cache");
        // A miss is cached too (until MISSED_FOR).
        let other = ip("192.0.2.33");
        assert_eq!(names.get(other), None);
        gate_tx.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(names.get(other), None);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn the_system_resolver_never_echoes_an_address_as_a_name() {
        // Whatever the machine's resolver says about loopback, it is a
        // name or nothing — never "127.0.0.1" passed off as one.
        if let Some(n) = system(ip("127.0.0.1")) {
            assert!(n.parse::<IpAddr>().is_err(), "{n}");
            assert!(!n.ends_with('.'));
        }
    }
}
