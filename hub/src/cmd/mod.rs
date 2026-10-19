//! The CLI commands. Each is a thin shell over the library.

pub mod fake_device;
pub mod list;
pub mod listen;
pub mod monitor;
pub mod run;
pub mod sdp;

use std::net::{SocketAddr, ToSocketAddrs};

/// Resolve `host:port` (or bare `host`, defaulting to the hub port 9000).
pub fn resolve_hub(hub: &str) -> anyhow::Result<SocketAddr> {
    let candidate = if hub.contains(':') { hub.to_owned() } else { format!("{hub}:9000") };
    candidate
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| anyhow::anyhow!("no address for {hub}"))
}
