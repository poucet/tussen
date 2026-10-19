//! tussen-hub — the OSC router at the centre of tussen.
//!
//! Everything that makes a signal sends it to the hub on :9000; the hub
//! delivers each signal to whoever subscribed to its prefix. Static
//! destinations (the music engine, TouchDesigner, the monitor) come from
//! config; devices register themselves with `/tussen/hub/hello` and are
//! forgotten after `device_timeout_ms` of silence. A device never receives
//! its own prefix back — that is what prevents feedback loops.
//!
//! The library half exists so the integration tests can run the whole hub
//! in-process: [`hub::run`] is the router loop, [`fake`] is the fake
//! device's signal state, and the binary in `main.rs` is a thin CLI over
//! both.

pub mod bundles;
pub mod catalog;
pub mod config;
pub mod config_file;
pub mod console;
pub mod fake;
pub mod generators;
pub mod hub;
pub mod link;
pub mod midi;
pub mod pad;
pub mod prng;
pub mod rdns;
pub mod router;
pub mod sync;
pub mod timing;
