//! Seeded draws for the plugins that pick a value out of a configured range.
//!
//! A plugin that draws reports the seed it used and the halt message carries
//! it, so the next run can be made to stop in the same place. That is only
//! worth anything if a seed names the same sequence on every host and in every
//! later build, so the generator is written out here as a fixed algorithm with
//! locked test vectors rather than taken from a crate that is free to change
//! its output in a minor release.
//!
//! Callers build a `Prng` from the seed and keep the seed itself for
//! reporting. `Prng` is a `rand` generator, so any `rand` or `rand_distr`
//! sampling code can be pointed at it.

use std::convert::Infallible;

use rand::{SeedableRng, TryRng};

/// SplitMix64.
///
/// The state is a counter advanced by a fixed odd increment and the output
/// is a mix of that counter, which is what gives every seed the full
/// 2^64 period. The mix must not be written back into the state: that
/// turns the counter into an iterated permutation with no guaranteed
/// period.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub(crate) struct Prng(u64);

impl Prng {
    /// The increment SplitMix64 is defined with, the golden ratio scaled to
    /// 64 bits.
    const GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;

    fn draw(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(Self::GAMMA);

        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);

        z ^ (z >> 31)
    }
}

impl SeedableRng for Prng {
    type Seed = [u8; 8];

    /// Little endian, so that a seed names one sequence and not one per
    /// target.
    fn from_seed(seed: Self::Seed) -> Self {
        Self(u64::from_le_bytes(seed))
    }

    /// The seed is the state, unchanged.
    ///
    /// The default implementation runs the value through a mixer first, which
    /// would mean the number printed in a halt message is not the number that
    /// produced the draw. SplitMix64 mixes its counter on the way out, so a
    /// low entropy seed such as 0 or 1 is already fine as a starting state.
    fn seed_from_u64(state: u64) -> Self {
        Self(state)
    }
}

impl TryRng for Prng {
    type Error = Infallible;

    fn try_next_u32(&mut self) -> Result<u32, Self::Error> {
        // The closing xorshift spreads the counter over the whole word, so
        // the low half is as usable as the high half.
        Ok(self.draw() as u32)
    }

    fn try_next_u64(&mut self) -> Result<u64, Self::Error> {
        Ok(self.draw())
    }

    fn try_fill_bytes(&mut self, dst: &mut [u8]) -> Result<(), Self::Error> {
        for chunk in dst.chunks_mut(std::mem::size_of::<u64>()) {
            // Little endian for the same reason `from_seed` is.
            let word = self.draw().to_le_bytes();
            chunk.copy_from_slice(&word[..chunk.len()]);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use rand::Rng;

    use super::*;

    /// The published SplitMix64 vectors. A failure here means every seed
    /// tocat has ever printed now replays somewhere else.
    #[test]
    fn matches_reference_vectors() {
        let mut prng = Prng::seed_from_u64(0);

        assert_eq!(prng.next_u64(), 0xE220_A839_7B1D_CDAF);
        assert_eq!(prng.next_u64(), 0x6E78_9E6A_A1B9_65F4);
        assert_eq!(prng.next_u64(), 0x06C4_5D18_8009_454F);
        assert_eq!(prng.next_u64(), 0xF88B_B8A8_724C_81EC);
        assert_eq!(prng.next_u64(), 0x1B39_896A_51A8_749B);
    }

    /// A seed of 1 is a plausible thing to type, and it has to give a stream
    /// unrelated to a seed of 0.
    #[test]
    fn low_seeds_are_usable() {
        let mut prng = Prng::seed_from_u64(1);

        assert_eq!(prng.next_u64(), 0x910A_2DEC_8902_5CC1);
        assert_eq!(prng.next_u64(), 0xBEEB_8DA1_658E_EC67);
    }

    /// A reported seed has to rebuild the generator that produced the draw.
    #[test]
    fn the_seed_is_the_state() {
        let seed = 0x0123_4567_89AB_CDEF;

        assert_eq!(Prng::seed_from_u64(seed), Prng(seed));
        assert_eq!(Prng::from_seed(seed.to_le_bytes()), Prng(seed));
    }

    /// A trailing partial word takes the low bytes of one more draw.
    #[test]
    fn fills_a_partial_word() {
        let mut prng = Prng::seed_from_u64(0);
        let mut got = [0u8; 12];
        prng.fill_bytes(&mut got);

        let mut want = [0u8; 12];
        want[..8].copy_from_slice(&0xE220_A839_7B1D_CDAF_u64.to_le_bytes());
        want[8..].copy_from_slice(&0x6E78_9E6A_A1B9_65F4_u64.to_le_bytes()[..4]);

        assert_eq!(got, want);
    }
}
