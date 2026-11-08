//! Interop against the C reference implementation.
//!
//! The fixtures in `tests/fixtures/` are produced by the C library itself
//! (`tools/build_oracle.sh`), so these tests pin compatibility in both directions:
//! C-compressed blocks must decode here, and `xxh32` must match C byte for byte.
//! Nothing here needs a C toolchain at test time.

use lz4_in_rust::block::{compress_bound, decompress_into};
use lz4_in_rust::xxhash::Xxh32;

mod common;
use lz4_in_rust::Error;

fn read(path: &str) -> Vec<u8> {
    let full = format!("{}/tests/fixtures/{}", env!("CARGO_MANIFEST_DIR"), path);
    std::fs::read(&full).unwrap_or_else(|e| panic!("{full}: {e}"))
}

/// Reference blocks: (raw input, C-compressed block, level used).
const BLOCKS: &[(&str, &str)] = &[
    ("empty.raw", "empty-l0.lz4"),
    ("one.raw", "one-l0.lz4"),
    ("short_text.raw", "short_text-l0.lz4"),
    ("zeros.raw", "zeros-l0.lz4"),
    ("text.raw", "text-l0.lz4"),
    ("text.raw", "text-l9.lz4"),
    ("text.raw", "text-l12.lz4"),
    ("numbers.raw", "numbers-l0.lz4"),
    ("numbers.raw", "numbers-l9.lz4"),
    ("random.raw", "random-l0.lz4"),
];

#[test]
fn decodes_c_compressed_blocks() {
    for (raw_name, block_name) in BLOCKS {
        let raw = read(raw_name);
        let packed = read(block_name);

        let mut out = vec![0u8; raw.len()];
        let size =
            decompress_into(&packed, &mut out).unwrap_or_else(|e| panic!("{block_name}: {e}"));
        assert_eq!(size, raw.len(), "{block_name}: decoded size");
        assert_eq!(out, raw, "{block_name}: decoded content");

        // one byte short must be a clean error, not a panic or a silent truncation
        if !raw.is_empty() {
            let mut small = vec![0u8; raw.len() - 1];
            assert!(
                matches!(
                    decompress_into(&packed, &mut small),
                    Err(Error::OutputTooSmall { .. })
                ),
                "{block_name}: undersized output"
            );
        }
    }
}

#[test]
fn our_blocks_decode_with_the_same_decoder() {
    // compress -> decompress on a fixture, at several accelerations
    let raw = read("text.raw");
    for acceleration in [1, 2, 8, 64] {
        let mut packed = vec![0u8; compress_bound(raw.len())];
        let csize = lz4_in_rust::block::compress_into(&raw, &mut packed, acceleration).unwrap();
        let mut out = vec![0u8; raw.len()];
        let size = decompress_into(&packed[..csize], &mut out).unwrap();
        assert_eq!(size, raw.len());
        assert_eq!(out, raw, "acceleration {acceleration}");
    }
}

/// Ratio parity: a faithful port of the same greedy parser should land within a few
/// percent of the reference on the same input. This catches accidental loss of matches
/// (the risk when porting the hash-table/skip logic by hand).
#[test]
fn compressed_size_is_close_to_c() {
    // (input, C block fixture)
    let cases = [
        ("short_text.raw", "short_text-l0.lz4"),
        ("zeros.raw", "zeros-l0.lz4"),
        ("text.raw", "text-l0.lz4"),
        ("numbers.raw", "numbers-l0.lz4"),
        ("random.raw", "random-l0.lz4"),
    ];
    for (raw_name, c_block) in cases {
        let raw = read(raw_name);
        let c_size = read(c_block).len();
        let mut packed = vec![0u8; compress_bound(raw.len())];
        let ours = lz4_in_rust::block::compress_into(&raw, &mut packed, 1).unwrap();
        eprintln!("{raw_name}: ours {ours} vs C {c_size}");
        assert!(
            ours <= c_size + c_size / 20 + 16,
            "{raw_name}: ours {ours} vs C {c_size} (>5% worse)"
        );
        // and it must actually decode
        let mut out = vec![0u8; raw.len()];
        assert_eq!(
            decompress_into(&packed[..ours], &mut out).unwrap(),
            raw.len()
        );
        assert_eq!(out, raw);
    }
}

/// Byte-for-byte identical output, not merely a compatible stream: the port keeps the
/// reference's parser quirks (5-byte hash for `byU32`, `byU16` table below 64 KiB, the
/// `ip-1` candidate probe), so a diff here means a porting bug, not a tuning difference.
#[test]
fn level_0_output_is_byte_identical_to_c() {
    for (raw_name, block_name) in BLOCKS {
        if !block_name.ends_with("-l0.lz4") {
            continue;
        }
        let raw = read(raw_name);
        let c_block = read(block_name);
        let mut ours = vec![0u8; compress_bound(raw.len())];
        let size = lz4_in_rust::block::compress_into(&raw, &mut ours, 1).unwrap();
        assert_eq!(
            &ours[..size],
            &c_block[..],
            "{block_name}: output differs from the reference (ours {size} vs C {})",
            c_block.len()
        );
    }
}

/// The other direction: C must decode what we produce.
///
/// Needs the oracle binary (`tools/build_oracle.sh`); set `LZ4_C_ORACLE` to its path.
/// Skips silently when unset so the test suite stays dependency-free.
#[test]
fn c_decodes_our_blocks() {
    let Some(oracle) = common::oracle_or_skip("the differential block tests") else {
        return;
    };
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
    let tmp = dir.join("interop-roundtrip.bin");
    let packed_path = dir.join("interop-roundtrip.lz4");

    for name in [
        "short_text.raw",
        "zeros.raw",
        "text.raw",
        "numbers.raw",
        "random.raw",
    ] {
        let raw = read(name);
        for acceleration in [1, 3, 16] {
            let mut packed = vec![0u8; compress_bound(raw.len())];
            let csize = lz4_in_rust::block::compress_into(&raw, &mut packed, acceleration).unwrap();
            std::fs::write(&packed_path, &packed[..csize]).unwrap();

            let ok = common::oracle_run(
                &oracle,
                [
                    "unblock",
                    packed_path.to_str().unwrap(),
                    tmp.to_str().unwrap(),
                    &raw.len().to_string(),
                ],
            );
            assert!(ok, "{name} accel {acceleration}: C failed to decode");
            assert_eq!(
                std::fs::read(&tmp).unwrap(),
                raw,
                "{name} accel {acceleration}"
            );
        }
    }
}

/// XXH32 digests produced by the C library (`target/c-oracle xxh32 <file>`).
#[test]
fn xxh32_matches_c() {
    let expected: &[(&str, u32)] = &[
        ("empty.raw", 0x02cc_5d05),
        ("one.raw", 0x550d_7456),
        ("short_text.raw", 0xe8fd_894f),
        ("zeros.raw", 0x4755_46f5),
        ("text.raw", 0x5663_fe8f),
        ("random.raw", 0xb8f4_fb7b),
        ("numbers.raw", 0xda97_40a5),
    ];
    for (name, want) in expected {
        let data = read(name);
        assert_eq!(Xxh32::oneshot(&data), *want, "{name}");

        // and the streaming form must agree no matter how the data is split
        for chunk in [1usize, 7, 16, 255, 4096] {
            let mut h = Xxh32::new(0);
            for piece in data.chunks(chunk) {
                h.update(piece);
            }
            assert_eq!(h.digest(), *want, "{name} in chunks of {chunk}");
        }
    }
}
