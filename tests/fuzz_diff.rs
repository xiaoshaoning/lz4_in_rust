//! Randomized differential fuzzing against the C reference.
//!
//! The committed fixtures pin a handful of shapes; this sweep covers the combinations
//! they cannot: sizes that straddle every boundary the codec cares about (12, 13, 64 KiB,
//! the 64 KiB table switch at 65547), and data whose match structure varies from "one
//! endless run" to "no matches at all".
//!
//! Per case, four claims are checked:
//!   1. our compressed bytes are **identical** to C's (block, acceleration 1);
//!   2. C decodes our block to the original bytes;
//!   3. we decode C's block — both the fast parser and the high-compression one;
//!   4. frames: C decodes ours, we decode C's (2 block sizes, both block modes).
//!
//! Needs `LZ4_C_ORACLE` (see `tools/build_oracle.sh`); without it the test reports how to
//! enable itself and passes, so `cargo test` stays dependency-free.
//!
//! `LZ4_FUZZ_ROUNDS=n` adds n random cases (CI uses 150); `LZ4_FUZZ_SEED=s` changes the
//! seed. A failure prints the seed and the case, so it reproduces exactly.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use lz4_in_rust::frame::{self, BlockMode, Opts as FrameOpts};
use lz4_in_rust::{block, par};

/// Data shapes worth distinguishing: they produce very different match structures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// One byte repeated: the purest run-length case (offset 1).
    Zeros,
    /// A short random pattern repeated: overlapping matches, every offset bucket.
    Cycle,
    /// Repeated words: what LZ4 is built for.
    Text,
    /// Incompressible: exercises the stored-block path.
    Noise,
    /// Text with random bytes sprinkled in: frequent literals between short matches.
    Mixed,
    /// Random-length runs: long matches separated by unpredictable breaks.
    Runs,
}

const KINDS: [Kind; 6] = [
    Kind::Zeros,
    Kind::Cycle,
    Kind::Text,
    Kind::Noise,
    Kind::Mixed,
    Kind::Runs,
];

/// Sizes that straddle the boundaries: the format minimum (13), the table switch (65547),
/// and a size large enough to produce many sequences.
const SIZES: [usize; 12] = [
    0, 1, 4, 12, 13, 64, 4096, 65_535, 65_547, 65_548, 100_000, 300_000,
];

fn main_oracle() -> Option<PathBuf> {
    let path = std::env::var_os("LZ4_C_ORACLE")?;
    let path = PathBuf::from(path);
    if !path.exists() {
        panic!(
            "LZ4_C_ORACLE points at {} which does not exist",
            path.display()
        );
    }
    Some(path)
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn byte(&mut self) -> u8 {
        self.next() as u8
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn data(kind: Kind, len: usize, rng: &mut Rng) -> Vec<u8> {
    match kind {
        Kind::Zeros => vec![0x5A; len],
        Kind::Cycle => {
            let period = 1 + rng.below(64);
            let pattern: Vec<u8> = (0..period).map(|_| rng.byte()).collect();
            (0..len).map(|i| pattern[i % period]).collect()
        }
        Kind::Text => {
            let words: [&[u8]; 7] = [
                b"the ", b"quick ", b"brown ", b"fox ", b"jumps ", b"over ", b"lz4 ",
            ];
            let mut out = Vec::with_capacity(len + 8);
            let mut i = 0;
            while out.len() < len {
                out.extend_from_slice(words[(i * 5 + rng.below(3)) % words.len()]);
                i += 1;
            }
            out.truncate(len);
            out
        }
        Kind::Noise => (0..len).map(|_| rng.byte()).collect(),
        Kind::Mixed => {
            let mut out = data(Kind::Text, len, rng);
            let every = 1 + rng.below(32);
            for i in (0..len).step_by(every) {
                out[i] = rng.byte();
            }
            out
        }
        Kind::Runs => {
            let mut out = Vec::with_capacity(len);
            while out.len() < len {
                let byte = rng.byte();
                let run = 1 + rng.below(300);
                out.extend(std::iter::repeat(byte).take(run.min(len - out.len())));
            }
            out
        }
    }
}

fn case_dir() -> PathBuf {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/fuzz");
    std::fs::create_dir_all(&dir).expect("create target/fuzz");
    dir
}

fn oracle_run(oracle: &Path, args: &[&OsStr]) -> bool {
    let status = Command::new(oracle)
        .args(args)
        .status()
        .unwrap_or_else(|e| panic!("{}: {e}", oracle.display()));
    status.success()
}

struct Fixture<'a> {
    oracle: &'a Path,
    dir: PathBuf,
    raw: PathBuf,
    packed: PathBuf,
    back: PathBuf,
}

impl Fixture<'_> {
    /// Our block -> C decodes it -> the same bytes.
    fn c_decodes_our_block(&self, src: &[u8], acceleration: u32, what: &str) {
        let mut buf = vec![0u8; block::compress_bound(src.len())];
        let n = block::compress_into(src, &mut buf, acceleration).unwrap();
        std::fs::write(&self.packed, &buf[..n]).unwrap();
        let size = src.len().to_string();
        assert!(
            oracle_run(
                self.oracle,
                &[
                    OsStr::new("unblock"),
                    self.packed.as_os_str(),
                    self.back.as_os_str(),
                    OsStr::new(&size),
                ]
            ),
            "{what}: C could not decode our block ({n} bytes, acceleration {acceleration})"
        );
        assert_eq!(
            std::fs::read(&self.back).unwrap(),
            src,
            "{what}: C decoded different bytes"
        );
    }

    /// C compresses -> we decode it -> the same bytes.
    fn we_decode_c_block(&self, src: &[u8], level: &str, what: &str) {
        std::fs::write(&self.raw, src).unwrap();
        assert!(oracle_run(
            self.oracle,
            &[
                OsStr::new("block"),
                OsStr::new(level),
                self.raw.as_os_str(),
                self.packed.as_os_str(),
            ]
        ));
        let packed = std::fs::read(&self.packed).unwrap();
        let mut out = vec![0u8; src.len()];
        let n = block::decompress_into(&packed, &mut out)
            .unwrap_or_else(|e| panic!("{what}: our decoder rejected C's level {level}: {e}"));
        assert_eq!(n, src.len(), "{what}: decoded size, C level {level}");
        assert_eq!(out, src, "{what}: decoded bytes, C level {level}");
    }

    /// Both implementations must emit the same bytes.
    fn block_bytes_match_c(&self, src: &[u8], what: &str) {
        std::fs::write(&self.raw, src).unwrap();
        assert!(oracle_run(
            self.oracle,
            &[
                OsStr::new("block"),
                OsStr::new("0"),
                self.raw.as_os_str(),
                self.packed.as_os_str(),
            ]
        ));
        let theirs = std::fs::read(&self.packed).unwrap();
        let mut ours = vec![0u8; block::compress_bound(src.len())];
        let n = block::compress_into(src, &mut ours, 1).unwrap();
        assert_eq!(
            &ours[..n],
            &theirs[..],
            "{what}: our block ({} bytes) differs from C's ({} bytes)",
            n,
            theirs.len()
        );
    }

    fn c_decodes_our_frame(&self, src: &[u8], opts: &FrameOpts, what: &str) {
        let packed = frame::compress(src, opts).unwrap();
        std::fs::write(&self.packed, &packed).unwrap();
        assert!(
            oracle_run(
                self.oracle,
                &[
                    OsStr::new("unframe"),
                    self.packed.as_os_str(),
                    self.back.as_os_str(),
                ]
            ),
            "{what}: C could not decode our frame"
        );
        assert_eq!(
            std::fs::read(&self.back).unwrap(),
            src,
            "{what}: C frame bytes differ"
        );
    }

    fn we_decode_c_frame(&self, src: &[u8], args: [&str; 6], what: &str) {
        std::fs::write(&self.raw, src).unwrap();
        let mut argv: Vec<&OsStr> = vec![OsStr::new("frame")];
        let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        for a in &owned {
            argv.push(OsStr::new(a));
        }
        argv.push(self.raw.as_os_str());
        argv.push(self.packed.as_os_str());
        assert!(
            oracle_run(self.oracle, &argv),
            "{what}: C failed to build a frame"
        );
        let packed = std::fs::read(&self.packed).unwrap();
        let decoded = frame::decompress(&packed)
            .unwrap_or_else(|e| panic!("{what}: our decoder rejected C's frame ({args:?}): {e}"));
        assert_eq!(decoded.consumed, packed.len(), "{what}: consumed");
        assert_eq!(
            decoded.content, src,
            "{what}: frame bytes differ ({args:?})"
        );
    }
}

// ---------------------------------------------------------------- the rounds

fn run_case(fx: &Fixture, kind: Kind, len: usize, seed: u64) {
    let what = format!("{kind:?}/{len}/seed {seed:#x}");
    let mut rng = Rng(seed);
    let src = data(kind, len, &mut rng);

    fx.block_bytes_match_c(&src, &what);
    fx.c_decodes_our_block(&src, 1, &what);
    // acceleration > 1 has no reference counterpart in the oracle, only decodability
    fx.c_decodes_our_block(&src, 7, &what);
    fx.we_decode_c_block(&src, "0", &what);
    fx.we_decode_c_block(&src, "9", &what);

    // a frame at the smallest block size (many blocks) and the largest (one block)
    let small = FrameOpts {
        block_size: 64 << 10,
        block_mode: BlockMode::Independent,
        ..FrameOpts::default()
    };
    fx.c_decodes_our_frame(&src, &small, &what);
    fx.we_decode_c_frame(&src, ["1", "4", "0", "0", "1", "1"], &what);
    if !src.is_empty() {
        // linked mode needs at least one block to link from
        fx.c_decodes_our_frame(
            &src,
            &FrameOpts {
                block_mode: BlockMode::Linked,
                ..small
            },
            &what,
        );
        fx.we_decode_c_frame(&src, ["1", "4", "1", "1", "1", "1"], &what);
    }
}

#[test]
fn differential_fuzz_against_c() {
    let Some(oracle) = main_oracle() else {
        eprintln!(
            "LZ4_C_ORACLE unset: skipping the differential fuzz ({} cases).\n\
             build it with tools/build_oracle.sh, then: LZ4_C_ORACLE=target/c-oracle cargo test --release",
            SIZES.len() * KINDS.len()
        );
        return;
    };

    let seed: u64 = std::env::var("LZ4_FUZZ_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0x5EED_1234_ABCD_0001);
    let extra: usize = std::env::var("LZ4_FUZZ_ROUNDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let dir = case_dir();
    let fx = Fixture {
        oracle: &oracle,
        raw: dir.join("case.raw"),
        packed: dir.join("case.lz4"),
        back: dir.join("case.back"),
        dir,
    };

    let mut cases = 0usize;
    // every shape at every boundary size
    for kind in KINDS {
        for len in SIZES {
            run_case(&fx, kind, len, seed);
            cases += 1;
        }
    }
    // then random shapes at random sizes
    let mut rng = Rng(seed ^ 0xF00D);
    for i in 0..extra {
        let kind = KINDS[rng.below(KINDS.len())];
        let len = match i % 4 {
            // mix exact boundaries with arbitrary sizes
            0 => SIZES[rng.below(SIZES.len())],
            1 => rng.below(300),
            2 => rng.below(7000),
            _ => rng.below(400_000),
        };
        run_case(&fx, kind, len, rng.next());
        cases += 1;
    }
    let _ = &fx.dir;
    eprintln!("differential fuzz: {cases} cases passed (seed {seed:#x})");
    assert!(cases >= SIZES.len() * KINDS.len());
}

/// The parallel engine must agree with the single-block path and with C, for any worker
/// count — the property the whole `par` design rests on, checked on generated data.
#[test]
fn parallel_blocks_agree_with_c() {
    let Some(oracle) = main_oracle() else {
        eprintln!("LZ4_C_ORACLE unset: skipping");
        return;
    };
    let dir = case_dir();
    let mut rng = Rng(0xB10C_5EED);
    let src = data(Kind::Mixed, 500_000, &mut rng);
    let raw = dir.join("par.raw");
    let packed = dir.join("par.lz4");

    let opts = par::Opts {
        workers: par::Workers::Fixed(4),
        block_size: 64 << 10,
        ..par::Opts::default()
    };
    let blocks = par::compress(&src, &opts).unwrap();
    let refs: Vec<&[u8]> = blocks.iter().map(|b| b.as_slice()).collect();
    let mut out = vec![0u8; src.len()];
    par::decompress_into(&refs, &mut out, &opts).unwrap();
    assert_eq!(out, src, "parallel roundtrip");

    // and each block must be decodable by C on its own (independent blocks)
    let mut offset = 0usize;
    for (i, b) in blocks.iter().enumerate() {
        std::fs::write(&packed, b).unwrap();
        let len = (64 << 10).min(src.len() - offset);
        assert!(
            oracle_run(
                &oracle,
                &[
                    OsStr::new("unblock"),
                    packed.as_os_str(),
                    raw.as_os_str(),
                    OsStr::new(&len.to_string()),
                ]
            ),
            "block {i}: C could not decode it"
        );
        assert_eq!(
            std::fs::read(&raw).unwrap(),
            &src[offset..offset + len],
            "block {i}"
        );
        offset += len;
    }
}
