//! xoshiro256** seeded through `SplitMix64`, copied from the VM so the
//! stream for a given seed is fixed forever. Shared by the rewriter (memory
//! hook selection) and the supervisor (quanta and thread choice).

// `repr(C)`: lives in the scheduler state shared between processes.
#[derive(Clone, Debug)]
#[repr(C)]
pub struct Rng {
    s: [u64; 4],
}

impl Rng {
    pub fn seed_from_u64(seed: u64) -> Self {
        let mut x = seed;
        let mut next = || {
            x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = x;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        };
        Self {
            s: [next(), next(), next(), next()],
        }
    }

    pub fn next_u64(&mut self) -> u64 {
        let s = &mut self.s;
        let result = s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = s[1] << 17;
        s[2] ^= s[0];
        s[3] ^= s[1];
        s[1] ^= s[2];
        s[0] ^= s[3];
        s[2] ^= t;
        s[3] = s[3].rotate_left(45);
        result
    }

    /// Uniform value in `[0, n)`; `n` must be non-zero. Always consumes one
    /// output so the stream advances the same way regardless of `n`.
    pub fn below(&mut self, n: u64) -> u64 {
        ((u128::from(self.next_u64()) * u128::from(n)) >> 64) as u64
    }

    /// Uniform value in `[lo, hi]`
    pub fn range_inclusive(&mut self, lo: u32, hi: u32) -> u32 {
        lo + self.below(u64::from(hi - lo) + 1) as u32
    }

    /// Value in `[lo, hi]` (`0 < lo <= hi`) whose every octave above `lo` is
    /// about equally likely, so short and long values are both common.
    /// Integer arithmetic only: floating-point `ln`/`exp` may differ between
    /// library versions, and a draw must not.
    pub fn log_uniform(&mut self, lo: u64, hi: u64) -> u64 {
        let octaves = u64::from((hi / lo).ilog2());
        let k = self.below(octaves + 1);
        let from = lo << k;
        let to = hi.min((lo << (k + 1)) - 1);
        from + self.below(to - from + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_stream() {
        let mut a = Rng::seed_from_u64(42);
        let mut b = Rng::seed_from_u64(42);
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn different_seeds_differ() {
        let mut a = Rng::seed_from_u64(1);
        let mut b = Rng::seed_from_u64(2);
        assert_ne!(a.next_u64(), b.next_u64());
    }

    #[test]
    fn range_inclusive_stays_in_range() {
        let mut r = Rng::seed_from_u64(7);
        for _ in 0..10_000 {
            let v = r.range_inclusive(100, 200);
            assert!((100..=200).contains(&v));
        }
        assert_eq!(r.range_inclusive(5, 5), 5);
    }

    #[test]
    fn log_uniform_spreads_over_octaves() {
        let mut r = Rng::seed_from_u64(3);
        let (lo, hi) = (10_000, 100_000_000);
        let mut below_100us = 0;
        let mut above_10ms = 0;
        for _ in 0..10_000 {
            let v = r.log_uniform(lo, hi);
            assert!((lo..=hi).contains(&v));
            below_100us += u32::from(v < 100_000);
            above_10ms += u32::from(v > 10_000_000);
        }
        // About a quarter each; a uniform draw would give 0.1% and 90%
        assert!((1500..4000).contains(&below_100us), "{below_100us}");
        assert!((1000..4000).contains(&above_10ms), "{above_10ms}");
        assert_eq!(r.log_uniform(5, 5), 5);
        assert!((5..=9).contains(&r.log_uniform(5, 9)));
    }

    /// First `SplitMix64` output for seed 0 is a well-known reference value
    #[test]
    fn known_seed_expansion() {
        let r = Rng::seed_from_u64(0);
        assert_eq!(r.s[0], 0xE220_A839_7B1D_CDAF);
    }
}
