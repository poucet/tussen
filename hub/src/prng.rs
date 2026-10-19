//! The deterministic PRNG the fake generators share (`--fake-midi`,
//! `--fake-pad`): a splitmix64-seeded xorshift64* stream. Same seed,
//! same stream — that is the whole point; nothing here is for secrets.

/// xorshift64*: tiny, deterministic, plenty random for knobs and sticks.
pub struct XorShift(u64);

impl XorShift {
    /// splitmix64 seeding: distinct seeds give distinct, well-spread
    /// states (xorshift needs a nonzero one; 0 maps to a constant
    /// instead).
    pub fn new(seed: u64) -> XorShift {
        let mut z = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^= z >> 31;
        XorShift(if z == 0 { 1 } else { z })
    }

    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_stream_and_zero_seed_works() {
        let stream = |seed: u64| -> Vec<u64> {
            let mut r = XorShift::new(seed);
            (0..32).map(|_| r.next()).collect()
        };
        assert_eq!(stream(42), stream(42));
        assert_ne!(stream(42), stream(43));
        // A 0 seed must not collapse to a constant stream.
        let zeros = stream(0);
        assert_ne!(zeros[0], zeros[1]);
    }
}
