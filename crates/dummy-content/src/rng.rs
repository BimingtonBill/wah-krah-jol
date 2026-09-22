//! Deterministic pseudo-random numbers shared by every fixture writer.

/// SplitMix64 generator: small, fast, and stable across platforms.
///
/// Fixture bytes are part of the test contract, so the sequence produced by a
/// seed must never change. Extend this type with new methods instead of
/// altering existing ones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rng {
    state: u64,
}

impl Rng {
    /// Creates a generator from a caller-provided seed.
    #[must_use]
    pub const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// Returns the next value in the sequence.
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut value = self.state;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        value ^ (value >> 31)
    }

    /// Fills `bytes` with deterministic random data.
    pub fn fill(&mut self, bytes: &mut [u8]) {
        for chunk in bytes.chunks_mut(8) {
            let value = self.next_u64().to_le_bytes();
            let length = chunk.len();
            chunk.copy_from_slice(&value[..length]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_produces_identical_sequences() {
        let mut first = Rng::new(7);
        let mut second = Rng::new(7);
        for _ in 0..32 {
            assert_eq!(first.next_u64(), second.next_u64());
        }
    }

    #[test]
    fn different_seeds_diverge() {
        assert_ne!(Rng::new(1).next_u64(), Rng::new(2).next_u64());
    }

    #[test]
    fn fill_is_deterministic_for_unaligned_lengths() {
        let mut first = Rng::new(11);
        let mut second = Rng::new(11);
        let mut left = [0u8; 13];
        let mut right = [0u8; 13];
        first.fill(&mut left);
        second.fill(&mut right);
        assert_eq!(left, right);
        assert_ne!(left, [0u8; 13]);
    }
}
