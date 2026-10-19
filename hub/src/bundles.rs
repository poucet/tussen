//! Packing encoded OSC messages into datagrams no larger than
//! `max_packet`.
//!
//! The hub re-bundles per destination: every message a destination should
//! receive from one incoming datagram (or one generator tick) is packed
//! greedily into `#bundle` datagrams that stay at or under the limit, so
//! they never fragment over Wi-Fi. Messages are encoded once by the
//! caller; this module only arranges bytes, which is why it is not part
//! of simply-osc — the split size is hub policy, not protocol.

/// `#bundle\0` (8 bytes) plus the timetag (8 bytes).
pub const BUNDLE_HEADER: usize = 16;
/// Each bundle element is a 4-byte size prefix plus its bytes.
pub const ELEMENT_HEADER: usize = 4;

/// Pack pre-encoded message bytes into datagram payloads of at most
/// `max_packet` bytes, preserving order. A message too large to fit even
/// alone in a bundle is sent bare (the network may fragment it; dropping
/// it would be worse) — with 60 Hz float signals this does not happen.
pub fn pack(messages: &[Vec<u8>], max_packet: usize) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut current: Vec<&[u8]> = Vec::new();
    let mut size = BUNDLE_HEADER;
    for m in messages {
        let element = ELEMENT_HEADER + m.len();
        if !current.is_empty() && size + element > max_packet {
            out.push(bundle_bytes(&current));
            current.clear();
            size = BUNDLE_HEADER;
        }
        if current.is_empty() && BUNDLE_HEADER + element > max_packet {
            out.push(m.clone());
            continue;
        }
        current.push(m);
        size += element;
    }
    if !current.is_empty() {
        out.push(bundle_bytes(&current));
    }
    out
}

/// A `#bundle` with the *immediately* timetag around the given elements.
fn bundle_bytes(elements: &[&[u8]]) -> Vec<u8> {
    let total = BUNDLE_HEADER + elements.iter().map(|e| ELEMENT_HEADER + e.len()).sum::<usize>();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(b"#bundle\0");
    out.extend_from_slice(&1u64.to_be_bytes()); // TimeTag::IMMEDIATE
    for e in elements {
        out.extend_from_slice(&(e.len() as u32).to_be_bytes());
        out.extend_from_slice(e);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use simply_osc::{Message, Packet, Value};

    fn encoded(n: usize) -> Vec<Vec<u8>> {
        (0..n)
            .map(|i| {
                Message::new(format!("/tussen/quest/head/n/sig{i}"))
                    .arg(Value::Float32(i as f32))
                    .encode()
                    .unwrap()
            })
            .collect()
    }

    fn flatten(datagrams: &[Vec<u8>]) -> Vec<Message> {
        let mut out = Vec::new();
        for d in datagrams {
            let packet = simply_osc::wire::parse(d).unwrap();
            for (_, m) in packet.messages() {
                out.push(m.clone());
            }
        }
        out
    }

    #[test]
    fn everything_fits_in_one_bundle_when_small() {
        let msgs = encoded(3);
        let datagrams = pack(&msgs, 1200);
        assert_eq!(datagrams.len(), 1);
        assert!(datagrams[0].len() <= 1200);
        assert!(matches!(simply_osc::wire::parse(&datagrams[0]).unwrap(), Packet::Bundle(_)));
    }

    #[test]
    fn splitting_at_1200_keeps_every_datagram_under_the_limit_and_loses_nothing() {
        // ~44 bytes each encoded; 60 of them cannot fit in one 1200 B bundle.
        let msgs = encoded(60);
        let datagrams = pack(&msgs, 1200);
        assert!(datagrams.len() > 1, "should have split");
        for d in &datagrams {
            assert!(d.len() <= 1200, "datagram of {} bytes", d.len());
        }
        let back = flatten(&datagrams);
        assert_eq!(back.len(), 60);
        for (i, m) in back.iter().enumerate() {
            assert_eq!(m.address, format!("/tussen/quest/head/n/sig{i}"), "order preserved");
        }
    }

    #[test]
    fn packing_is_greedy_up_to_the_boundary() {
        // Same-sized messages: the first bundle holds exactly as many as
        // fit, computed from the real encoded size.
        let msgs = encoded(9); // one digit each, so all the same size
        let element = ELEMENT_HEADER + msgs[0].len();
        let per_bundle = (200 - BUNDLE_HEADER) / element;
        assert!(per_bundle >= 1 && per_bundle < 9);
        let datagrams = pack(&msgs, 200);
        assert_eq!(flatten(&datagrams[..1]).len(), per_bundle);
        assert_eq!(flatten(&datagrams).len(), 9);
    }

    #[test]
    fn an_oversized_message_travels_bare_rather_than_being_dropped() {
        let big = Message::new("/ext/blob").arg(Value::Blob(vec![0; 2000])).encode().unwrap();
        let small = encoded(1);
        let msgs = vec![small[0].clone(), big.clone(), small[0].clone()];
        let datagrams = pack(&msgs, 1200);
        assert_eq!(flatten(&datagrams).len(), 3);
        assert!(datagrams.iter().any(|d| d == &big), "the big one is bare");
    }

    #[test]
    fn no_messages_means_no_datagrams() {
        assert!(pack(&[], 1200).is_empty());
    }
}
