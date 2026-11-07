//! Frame-format interop against the C reference.
//!
//! `tests/fixtures/frame/` holds frames produced by `lib/lz4frame.c` itself
//! (`tools/build_oracle.sh`), covering the descriptor combinations: linked vs independent
//! blocks, 64 KiB and 4 MiB blocks, block and content checksums, a declared content size,
//! high-compression blocks, and an empty frame. Decoding them must reproduce the `.raw`
//! fixtures byte for byte, and our own frames must be decodable by the C library.

use lz4_in_rust::frame::{self, BlockMode, Opts};
use lz4_in_rust::level::Level;
use lz4_in_rust::Error;

fn read(path: &str) -> Vec<u8> {
    let full = format!("{}/tests/fixtures/{path}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&full).unwrap_or_else(|e| panic!("{full}: {e}"))
}

/// (frame fixture, raw input it must decode to)
const FRAMES: &[(&str, &str)] = &[
    ("frame/text-1.lz4", "text.raw"),
    ("frame/text-linked.lz4", "text.raw"),
    ("frame/text-b4.lz4", "text.raw"),
    ("frame/text-crc.lz4", "text.raw"),
    ("frame/text-size.lz4", "text.raw"),
    ("frame/text-plain.lz4", "text.raw"),
    ("frame/text-hc9.lz4", "text.raw"),
    // 150 KiB in 64 KiB blocks: three blocks, and the linked one really does reference
    // across block boundaries (it is 5 KiB smaller than the independent version)
    ("frame/large-b4.lz4", "frame/large.raw"),
    ("frame/large-b4-linked.lz4", "frame/large.raw"),
    ("frame/large-b4-crc-size.lz4", "frame/large.raw"),
    ("frame/numbers-1.lz4", "numbers.raw"),
    ("frame/empty-1.lz4", "empty.raw"),
    ("frame/one-1.lz4", "one.raw"),
];

#[test]
fn decodes_c_frames() {
    for (frame_name, raw_name) in FRAMES {
        let packed = read(frame_name);
        let raw = read(raw_name);
        let decoded = frame::decompress(&packed).unwrap_or_else(|e| panic!("{frame_name}: {e}"));
        assert_eq!(decoded.content, raw, "{frame_name}: content");
        assert_eq!(decoded.consumed, packed.len(), "{frame_name}: consumed");
    }
}

#[test]
fn reports_the_descriptor_it_read() {
    let linked = frame::decompress(&read("frame/text-linked.lz4"))
        .unwrap()
        .info;
    assert_eq!(linked.block_mode, BlockMode::Linked);
    assert_eq!(linked.block_size, 4 << 20);
    assert!(linked.content_checksum);
    assert!(!linked.block_checksum);
    assert_eq!(linked.content_size, None);

    // the multi-block frame: linked blocks decode against the history before them
    let multi = frame::decompress(&read("frame/large-b4-linked.lz4"))
        .unwrap()
        .info;
    assert_eq!(multi.block_mode, BlockMode::Linked);
    assert_eq!(multi.block_size, 64 << 10);

    let independent = frame::decompress(&read("frame/text-b4.lz4")).unwrap().info;
    assert_eq!(independent.block_mode, BlockMode::Independent);
    assert_eq!(independent.block_size, 64 << 10);

    let crc = frame::decompress(&read("frame/text-crc.lz4")).unwrap().info;
    assert!(crc.block_checksum && crc.content_checksum);

    let sized = frame::decompress(&read("frame/text-size.lz4"))
        .unwrap()
        .info;
    assert_eq!(sized.content_size, Some(read("text.raw").len() as u64));

    let plain = frame::decompress(&read("frame/text-plain.lz4"))
        .unwrap()
        .info;
    assert!(!plain.block_checksum && !plain.content_checksum);

    let hc = frame::decompress(&read("frame/text-hc9.lz4")).unwrap().info;
    assert_eq!(hc.block_mode, BlockMode::Independent);
}

/// Same input, same preferences, same bytes: the frame layer adds no decisions of its own
/// on top of the block codec (which is already byte-identical).
#[test]
fn our_frame_is_byte_identical_to_c() {
    let raw = read("text.raw");
    let ours = frame::compress(&raw, &Opts::default()).unwrap();
    let c_frame = read("frame/text-1.lz4");
    assert_eq!(
        ours,
        c_frame,
        "our default frame ({} bytes) differs from C's ({} bytes)",
        ours.len(),
        c_frame.len()
    );

    // and with every descriptor option on
    let opts = Opts {
        block_size: 64 << 10,
        block_mode: BlockMode::Independent,
        block_checksum: true,
        content_checksum: true,
        content_size: true,
        workers: lz4_in_rust::par::Workers::Fixed(2),
        level: Level::FAST,
        ..Opts::default()
    };
    let ours = frame::compress(&raw, &opts).unwrap();
    // C writes the same descriptor; compare through C's decoder instead of a fixture,
    // because the reference refuses content_size when streaming (autoFlush) inputs
    let decoded = frame::decompress(&ours).unwrap();
    assert_eq!(decoded.content, raw);
    assert_eq!(decoded.info.content_size, Some(raw.len() as u64));
}

#[test]
fn our_frames_decode_with_the_c_library() {
    let Ok(oracle) = std::env::var("LZ4_C_ORACLE") else {
        eprintln!("LZ4_C_ORACLE unset: skipping");
        return;
    };
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
    let packed_path = dir.join("frame-interop.lz4");
    let back_path = dir.join("frame-interop.raw");

    let cases = [
        ("text.raw", Opts::default()),
        (
            "text.raw",
            Opts {
                block_mode: BlockMode::Linked,
                ..Opts::default()
            },
        ),
        (
            "text.raw",
            Opts {
                block_size: 64 << 10,
                block_checksum: true,
                ..Opts::default()
            },
        ),
        (
            "numbers.raw",
            Opts {
                content_size: true,
                workers: lz4_in_rust::par::Workers::Fixed(4),
                ..Opts::default()
            },
        ),
        ("empty.raw", Opts::default()),
    ];

    for (raw_name, opts) in cases {
        let raw = read(raw_name);
        let packed = frame::compress(&raw, &opts).unwrap();
        std::fs::write(&packed_path, &packed).unwrap();
        let status = std::process::Command::new(&oracle)
            .args([
                "unframe",
                packed_path.to_str().unwrap(),
                back_path.to_str().unwrap(),
            ])
            .status()
            .unwrap_or_else(|e| panic!("{oracle}: {e}"));
        assert!(
            status.success(),
            "{raw_name} ({} blocks, {:?}): C could not decode our frame",
            opts.block_size,
            opts.block_mode
        );
        assert_eq!(
            std::fs::read(&back_path).unwrap(),
            raw,
            "{raw_name}: C decoded different bytes"
        );
    }
}

#[test]
fn corrupt_frames_are_errors() {
    let packed = read("frame/text-crc.lz4");
    // every truncation
    for cut in 0..packed.len() {
        assert!(
            matches!(
                frame::decompress(&packed[..cut]),
                Err(Error::Truncated
                    | Error::InvalidFrame(_)
                    | Error::ChecksumMismatch { .. }
                    | Error::OutputTooSmall { .. })
            ),
            "cut at {cut}"
        );
    }
    // single-bit flips in the descriptor and in the body
    for at in [4usize, 5, 6, 20, 40, 100, packed.len() - 1] {
        if at >= packed.len() {
            continue;
        }
        let mut broken = packed.clone();
        broken[at] ^= 0x01;
        assert!(
            frame::decompress(&broken).is_err(),
            "flip at {at} was accepted"
        );
    }
}
