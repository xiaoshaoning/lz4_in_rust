//! Compression levels, and the one place that knows what a level *means*.
//!
//! The reference exposes one user-facing scale (`lz4 -1` .. `lz4 -12`) over two parsers:
//! level 1 runs the fast parser (`lib/lz4.c`), levels 2..=12 the high-compression one
//! (`lib/lz4hc.c`). The split point is `LZ4HC_CLEVEL_MIN = 2`, and `lib/lz4frame.c` picks
//! the parser with exactly that comparison.
//!
//! Ported so far: level 1, and levels 3..=9 (the hash-chain strategy). Level 2 (`lz4mid`)
//! and 10..=12 (the optimal parser) report themselves as unsupported rather than
//! compressing at another level and calling it close enough.
//!
//! This mapping lives here on purpose. `par`, the frame layer and the CLI must not each
//! learn "1 is fast, 2+ is HC": that is one design decision, so it gets one home. They
//! pass a [`Level`] and never see an acceleration factor.

use crate::{block, Error};

/// A `lz4 -#` level: 1 (fast) through 12 (slowest, best ratio).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Level(u8);

impl Level {
    pub const MIN: u8 = 1;
    pub const MAX: u8 = 12;
    /// First level that uses the high-compression parser (`LZ4HC_CLEVEL_MIN`).
    pub const FIRST_HIGH: u8 = 2;
    /// The default level, `lz4 -1`.
    pub const FAST: Level = Level(1);

    /// `None` for anything outside 1..=12: the range check belongs to whoever parses the
    /// command line, not to the codec.
    pub fn new(n: u8) -> Option<Level> {
        (Level::MIN..=Level::MAX).contains(&n).then_some(Level(n))
    }

    pub fn as_u8(self) -> u8 {
        self.0
    }

    /// Fast parser vs high-compression parser.
    pub fn is_fast(self) -> bool {
        self.0 < Level::FIRST_HIGH
    }

    /// Acceleration for the fast parser — private, because it is not a second knob for
    /// callers: the reference maps every non-negative level to 1, so `lz4 -1` and `lz4 -2`
    /// differ because the second one is *not* fast, not because it accelerates differently.
    /// Faster-than-default settings come from `--fast=N`, which is the raw knob
    /// [`block::compress_into`] exposes.
    fn acceleration(self) -> u32 {
        1
    }
}

impl Default for Level {
    fn default() -> Self {
        Level::FAST
    }
}

/// Compress `src` into `dst` at `level`, returning the compressed size.
///
/// `dst.len()` must be at least [`block::compress_bound`]`(src.len())`.
pub fn compress_into(src: &[u8], dst: &mut [u8], level: Level) -> Result<usize, Error> {
    if level.is_fast() {
        block::compress_into(src, dst, level.acceleration())
    } else if (crate::hc::LEVEL_MIN..=crate::hc::LEVEL_MAX).contains(&level.as_u8()) {
        crate::hc::compress_into(src, dst, level.as_u8())
    } else {
        Err(Error::Unsupported(
            "high compression level 2 (lz4mid) and 10..=12 (optimal parser) are not ported yet",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_cover_the_cli_range_only() {
        assert_eq!(Level::new(0), None);
        assert_eq!(Level::new(1), Some(Level::FAST));
        assert_eq!(Level::new(13), None);
        for n in Level::MIN..=Level::MAX {
            assert_eq!(Level::new(n).unwrap().as_u8(), n);
        }
    }

    #[test]
    fn level_one_is_fast_and_two_is_not() {
        // this is the whole reason the module exists: LZ4HC_CLEVEL_MIN == 2
        assert!(Level::new(1).unwrap().is_fast());
        assert!(!Level::new(2).unwrap().is_fast());
        assert!(!Level::new(12).unwrap().is_fast());
        assert_eq!(Level::new(1).unwrap(), Level::FAST);
    }

    /// The mapping must be the identity for the fast tier: `lz4 -1` is
    /// `LZ4_compress_default`, which is what the fixture was produced with.
    #[test]
    fn fast_level_matches_the_c_fixture() {
        let raw = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/text.raw"
        ))
        .unwrap();
        let c_block = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/text-l0.lz4"
        ))
        .unwrap();
        let mut ours = vec![0u8; block::compress_bound(raw.len())];
        let n = compress_into(&raw, &mut ours, Level::FAST).unwrap();
        assert_eq!(&ours[..n], &c_block[..], "lz4 -1 must equal the C default");
    }

    /// The levels that are ported must roundtrip; the two that are not must say so rather
    /// than silently compressing at another level.
    #[test]
    fn high_levels_are_either_ported_or_reported() {
        let raw = vec![b'x'; 1000];
        let mut out = vec![0u8; block::compress_bound(raw.len())];
        for n in 3..=9u8 {
            let size = compress_into(&raw, &mut out, Level::new(n).unwrap())
                .unwrap_or_else(|e| panic!("level {n}: {e}"));
            let mut back = vec![0u8; raw.len()];
            assert_eq!(
                block::decompress_into(&out[..size], &mut back).unwrap(),
                raw.len()
            );
            assert_eq!(back, raw);
        }
        for n in [2u8, 10, 11, 12] {
            assert!(
                matches!(
                    compress_into(&raw, &mut out, Level::new(n).unwrap()),
                    Err(Error::Unsupported(_))
                ),
                "level {n} should report the gap"
            );
        }
    }
}
