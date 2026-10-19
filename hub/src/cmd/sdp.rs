//! `simply-tussen sdp` — print an SDP describing the hub's RTP/L16 stream,
//! so a stock player can hear it while debugging:
//!
//! ```sh
//! simply-tussen sdp --to 127.0.0.1:5004 > stream.sdp
//! ffplay -nodisp -protocol_whitelist file,rtp,udp stream.sdp
//! ```
//!
//! ffplay then owns :5004, so give it a port some device (or a parallel
//! `listen --audio`) is *not* already bound to.

use tussen_hub::generators::audio::rtp;

pub fn run(to: &str, sample_rate: u32, channels: u16) -> anyhow::Result<()> {
    let addr = super::resolve_hub(to)?;
    print!("{}", rtp::sdp(&addr.ip().to_string(), addr.port(), sample_rate, channels));
    Ok(())
}
