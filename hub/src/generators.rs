//! Generators: each is a plug-in module that publishes under
//! its own prefix into the router; adding one doesn't touch the router.
//!
//! This is the registration point for the generator lanes:
//!
//! * **audio** — the RTP stream of the engine's audio (the
//!   hub computes nothing from it — analysis is device-local);
//! * **midi** — the MIDI controllers under `/tussen/midi/…`;
//! * **pad** — a game controller on the hub laptop under
//!   `/tussen/pad/…`;
//! * **clock** — tempo and beat under `/tussen/clock/…`.
//!
//! A generator owns its own threads and clocking; it publishes by sending
//! [`Message`]s into the channel it is started with. The hub routes them
//! exactly like signals received on :9000 (the ownership rule does not
//! apply — no device is named `audio`, `midi`, `pad` or `clock`), so a new
//! generator is one module plus a config section, and no router change
//! Registration: build the generator from its config section
//! in `cmd::run`, push it into the `Vec<Box<dyn Generator>>` passed to
//! [`start_all`].

pub mod audio;

use crossbeam_channel::Sender;
use simply_osc::Message;

/// A source of hub-published signals. Implementations must not block the
/// router: they get a `Sender` clone and do their timing on their own
/// threads.
pub trait Generator: Send {
    /// The name in the log and the stats line (`"audio"`, `"midi"`, …).
    fn name(&self) -> &'static str;
    /// Start the generator's threads. Publish by sending into `out`;
    /// messages are routed by prefix like any other signal. Errors are
    /// logged and the hub keeps running without this generator.
    fn start(&mut self, out: Sender<Message>) -> anyhow::Result<()>;
}

/// Start every generator, logging rather than failing on the ones that
/// can't (a missing audio device must not stop the router).
pub fn start_all(generators: &mut [Box<dyn Generator>], out: &Sender<Message>) {
    for g in generators {
        match g.start(out.clone()) {
            Ok(()) => log::info!("generator {} started", g.name()),
            Err(e) => log::warn!("generator {} unavailable, continuing without it: {e:#}", g.name()),
        }
    }
}
