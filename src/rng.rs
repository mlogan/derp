//! xoshiro256** seeded through `SplitMix64`, copied from the VM so the
//! stream for a given seed is fixed forever. Shared by the rewriter (memory
//! hook selection) and the supervisor (quanta and thread choice).

#[derive(Clone, Debug)]
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

    /// First `SplitMix64` output for seed 0 is a well-known reference value
    #[test]
    fn known_seed_expansion() {
        let r = Rng::seed_from_u64(0);
        assert_eq!(r.s[0], 0xE220_A839_7B1D_CDAF);
    }
}
