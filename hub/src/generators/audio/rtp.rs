//! Minimal RTP (RFC 3550) for the one thing the hub sends: L16 (RFC 3551)
//! 16-bit big-endian PCM. L16's static payload types only cover 44.1 kHz,
//! so 48 kHz rides on dynamic payload type 96, declared by the SDP that
//! `simply-tussen sdp` prints. The parser half exists for `listen --audio`
//! and the tests.

/// Dynamic payload type for L16/48000/2 — matched by [`sdp`].
pub const PAYLOAD_TYPE: u8 = 96;
/// RTP header size: no CSRCs, no extension.
pub const HEADER_LEN: usize = 12;

/// Builds successive RTP packets for one L16 stream. The sequence number
/// counts packets (mod 2^16, loss detection); the timestamp counts
/// frames/samples-per-channel (mod 2^32, the RFC 3551 L16 clock).
pub struct Packetizer {
    pub ssrc: u32,
    pub seq: u16,
    pub timestamp: u32,
}

impl Packetizer {
    /// RFC 3550 wants random initial seq/timestamp/SSRC (defence against
    /// cross-stream confusion, not cryptography) — derived from the clock.
    pub fn new() -> Packetizer {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x5eed);
        // splitmix64: cheap, well-mixed, no dependency.
        let mut z = seed.wrapping_add(0x9e3779b97f4a7c15);
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^= z >> 31;
        Packetizer { ssrc: z as u32, seq: (z >> 32) as u16, timestamp: (z >> 16) as u32 }
    }

    /// One packet: 12-byte header + every sample as i16 big-endian.
    /// `samples` is interleaved, `frames` = samples.len() / channels.
    pub fn packet(&mut self, samples: &[f32], channels: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + samples.len() * 2);
        self.packet_into(samples, channels, &mut out);
        out
    }

    /// [`Packetizer::packet`] into `out` (cleared first): no allocation
    /// once `out` has held a packet this size — the sender's delay ring
    /// reuses its slots this way.
    pub fn packet_into(&mut self, samples: &[f32], channels: usize, out: &mut Vec<u8>) {
        out.clear();
        out.push(0x80); // V=2, no padding, no extension, no CSRCs
        out.push(PAYLOAD_TYPE); // marker 0
        out.extend_from_slice(&self.seq.to_be_bytes());
        out.extend_from_slice(&self.timestamp.to_be_bytes());
        out.extend_from_slice(&self.ssrc.to_be_bytes());
        for &s in samples {
            let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
            out.extend_from_slice(&v.to_be_bytes());
        }
        self.seq = self.seq.wrapping_add(1);
        self.timestamp = self.timestamp.wrapping_add((samples.len() / channels) as u32);
    }
}

impl Default for Packetizer {
    fn default() -> Self {
        Packetizer::new()
    }
}

/// A parsed RTP packet (header fields + L16 payload decoded to f32).
pub struct Packet {
    pub payload_type: u8,
    pub seq: u16,
    pub timestamp: u32,
    pub ssrc: u32,
    pub samples: Vec<f32>,
}

/// Parse an RTP datagram; `None` for anything that isn't plain V=2 RTP.
pub fn parse(datagram: &[u8]) -> Option<Packet> {
    if datagram.len() < HEADER_LEN || datagram[0] >> 6 != 2 {
        return None;
    }
    // Reject padding/extension/CSRCs — the hub never sends them, and
    // handling them here would be untested code.
    if datagram[0] & 0x3f != 0 {
        return None;
    }
    let payload = &datagram[HEADER_LEN..];
    if payload.len() % 2 != 0 {
        return None;
    }
    Some(Packet {
        payload_type: datagram[1] & 0x7f,
        seq: u16::from_be_bytes([datagram[2], datagram[3]]),
        timestamp: u32::from_be_bytes([datagram[4], datagram[5], datagram[6], datagram[7]]),
        ssrc: u32::from_be_bytes([datagram[8], datagram[9], datagram[10], datagram[11]]),
        samples: payload
            .chunks_exact(2)
            .map(|b| i16::from_be_bytes([b[0], b[1]]) as f32 / 32767.0)
            .collect(),
    })
}

/// The SDP (RFC 4566/8866) that lets ffplay or VLC play the stream:
/// `ffplay -protocol_whitelist file,rtp,udp stream.sdp`.
pub fn sdp(dest_ip: &str, dest_port: u16, sample_rate: u32, channels: u16) -> String {
    format!(
        "v=0\r\n\
         o=- 0 0 IN IP4 127.0.0.1\r\n\
         s=simply-tussen engine audio\r\n\
         c=IN IP4 {dest_ip}\r\n\
         t=0 0\r\n\
         m=audio {dest_port} RTP/AVP {PAYLOAD_TYPE}\r\n\
         a=rtpmap:{PAYLOAD_TYPE} L16/{sample_rate}/{channels}\r\n\
         a=recvonly\r\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// seq and timestamp advance monotonically, and the L16 BE
    /// payload round-trips through the parser.
    #[test]
    fn packets_round_trip_and_count_monotonically() {
        let mut p = Packetizer { ssrc: 7, seq: u16::MAX, timestamp: 100 };
        let frame: Vec<f32> = (0..512).map(|i| ((i as f32) / 256.0 - 1.0) * 0.9).collect();

        let first = parse(&p.packet(&frame, 2)).unwrap();
        let second = parse(&p.packet(&frame, 2)).unwrap();

        assert_eq!(first.payload_type, PAYLOAD_TYPE);
        assert_eq!(first.ssrc, 7);
        assert_eq!((first.seq, second.seq), (u16::MAX, 0)); // wraps, still +1
        assert_eq!(first.timestamp, 100);
        assert_eq!(second.timestamp, 100 + 256); // frames, not samples
        assert_eq!(first.samples.len(), 512);
        for (a, b) in frame.iter().zip(&first.samples) {
            assert!((a - b).abs() < 1.0 / 32000.0, "{a} vs {b}");
        }
    }

    /// The payload really is big-endian i16 — checked byte by byte, since
    /// a little-endian slip would still round-trip through our own parser.
    #[test]
    fn the_payload_is_big_endian_l16() {
        let mut p = Packetizer { ssrc: 0, seq: 0, timestamp: 0 };
        let bytes = p.packet(&[0.5, -1.0], 2);
        assert_eq!(bytes.len(), HEADER_LEN + 4);
        let half = (0.5f32 * 32767.0) as i16;
        assert_eq!(&bytes[HEADER_LEN..HEADER_LEN + 2], &half.to_be_bytes());
        assert_eq!(&bytes[HEADER_LEN + 2..], &(-32767i16).to_be_bytes());
    }

    #[test]
    fn junk_does_not_parse() {
        assert!(parse(&[]).is_none());
        assert!(parse(&[0x80; 5]).is_none()); // too short
        assert!(parse(&[0x00; 16]).is_none()); // version 0
        let mut odd = vec![0x80, 96, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        odd.push(1); // odd payload length
        assert!(parse(&odd).is_none());
    }

    #[test]
    fn sdp_names_the_l16_stream() {
        let s = sdp("127.0.0.1", 5004, 48000, 2);
        assert!(s.starts_with("v=0\r\n"));
        assert!(s.contains("m=audio 5004 RTP/AVP 96\r\n"));
        assert!(s.contains("a=rtpmap:96 L16/48000/2\r\n"));
    }
}
