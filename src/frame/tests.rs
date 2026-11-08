//! Frame tests: the descriptor layout, every block-size/mode/checksum combination, the
//! streaming properties (chunking must not change anything), and the error paths.

use super::*;

fn corpus(len: usize) -> Vec<u8> {
    let words = [
        &b"the "[..],
        b"quick ",
        b"brown ",
        b"fox ",
        b"lz4 ",
        b"frame ",
    ];
    let mut out = Vec::with_capacity(len + 8);
    let mut i = 0;
    while out.len() < len {
        out.extend_from_slice(words[(i * 7) % words.len()]);
        i += 1;
    }
    out.truncate(len);
    out
}

fn opts(block_size: usize, block_mode: BlockMode) -> Opts {
    Opts {
        block_size,
        block_mode,
        ..Opts::default()
    }
}

/// Feed `data` to an encoder in `chunk`-sized writes.
fn stream_once(data: &[u8], opts: &Opts, chunk: usize) -> io::Result<Vec<u8>> {
    let mut enc = Encoder::new(Vec::new(), opts, Some(data.len() as u64))?;
    for piece in data.chunks(chunk.max(1)) {
        enc.write_all(piece)?;
    }
    enc.finish()
}

#[test]
fn header_layout_matches_the_specification() {
    let src = corpus(1000);
    let o = Opts {
        block_size: 0x4_0000,
        block_mode: BlockMode::Independent,
        block_checksum: true,
        content_checksum: true,
        content_size: true,
        ..Opts::default()
    };
    let frame = compress(&src, &o).unwrap();
    assert_eq!(&frame[0..4], &MAGIC.to_le_bytes());
    // 0x40 version | 0x20 independent | 0x10 block crc | 0x08 content size | 0x04 content crc
    assert_eq!(frame[4], 0x7C);
    assert_eq!(frame[5], 5 << 4, "block size id 5 = 256 KiB");
    assert_eq!(u64::from_le_bytes(frame[6..14].try_into().unwrap()), 1000);
    assert_eq!(frame[14], header_checksum(&frame[4..14]));
    let size_field = u32::from_le_bytes(frame[15..19].try_into().unwrap());
    assert_eq!(size_field & BLOCK_UNCOMPRESSED, 0);
    assert!(size_field as usize <= 1000, "block {size_field} bytes");

    let d = decompress(&frame).unwrap();
    assert_eq!(d.content, src);
    assert_eq!(d.consumed, frame.len());
    assert_eq!(d.info.block_size, 0x4_0000);
    assert_eq!(d.info.block_mode, BlockMode::Independent);
    assert!(d.info.block_checksum);
    assert!(d.info.content_checksum);
    assert_eq!(d.info.content_size, Some(1000));
}

#[test]
fn roundtrip_every_block_size_and_mode() {
    // Miri interprets, it does not compile: the big sweeps would take hours there
    if cfg!(miri) {
        return;
    }

    for len in [0usize, 1, 100, 0xFFFF, 0x1_0000, 0x1_0001, 0x5_0000] {
        for block_size in [0x1_0000, 0x4_0000, 0x10_0000, 0x40_0000] {
            for mode in [BlockMode::Independent, BlockMode::Linked] {
                for checksums in [(false, false), (true, false), (false, true), (true, true)] {
                    let src = corpus(len);
                    let o = Opts {
                        block_size,
                        block_mode: mode,
                        block_checksum: checksums.0,
                        content_checksum: checksums.1,
                        content_size: len % 2 == 0,
                        ..Opts::default()
                    };
                    let frame = compress(&src, &o).unwrap();
                    let d = decompress(&frame)
                        .unwrap_or_else(|e| panic!("len {len} bs {block_size} {mode:?}: {e}"));
                    assert_eq!(d.content, src, "len {len} bs {block_size} {mode:?}");
                    assert_eq!(d.consumed, frame.len());
                    assert_eq!(d.info.block_mode, mode);
                    assert_eq!(d.info.block_size, block_size);
                }
            }
        }
    }
}

/// A streaming writer must not care how the input is chunked, and must produce exactly what
/// the whole-buffer path produces.
#[test]
fn write_chunking_does_not_change_the_frame() {
    // Miri interprets, it does not compile: the big sweeps would take hours there
    if cfg!(miri) {
        return;
    }

    let src = corpus(300_000);
    for block_size in [0x1_0000, 0x4_0000] {
        for mode in [BlockMode::Independent, BlockMode::Linked] {
            let o = Opts {
                block_size,
                block_mode: mode,
                content_checksum: true,
                ..Opts::default()
            };
            let whole = compress(&src, &o).unwrap();
            for chunk in [1usize, 7, 4096, 65_536, 300_000] {
                let streamed = stream_once(&src, &o, chunk).unwrap();
                assert_eq!(
                    streamed, whole,
                    "chunk {chunk}, block {block_size}, {mode:?}"
                );
            }
        }
    }
}

/// And a streaming reader must not care how the *input* is chunked either: the caller's
/// buffer size is chosen by the caller, not by the format.
#[test]
fn read_chunking_does_not_change_the_content() {
    // Miri interprets, it does not compile: the big sweeps would take hours there
    if cfg!(miri) {
        return;
    }

    let src = corpus(200_000);
    let frame = compress(&src, &opts(0x1_0000, BlockMode::Linked)).unwrap();
    for chunk in [1usize, 3, 1000, 1 << 20] {
        let mut dec = Decoder::new(io::Cursor::new(&frame));
        let mut out = Vec::new();
        let mut buf = vec![0u8; chunk];
        loop {
            let n = dec.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        assert_eq!(out, src, "read chunk {chunk}");
        assert_eq!(dec.produced(), src.len() as u64);
    }
}

/// `Read` and `Write` are what make the tool stream a file; check they compose by
/// transcoding one frame into another without ever holding the content in one buffer.
#[test]
fn io_copy_between_encoder_and_decoder() {
    // Miri interprets, it does not compile: the big sweeps would take hours there
    if cfg!(miri) {
        return;
    }

    let src = corpus(120_000);
    let from = compress(&src, &opts(0x1_0000, BlockMode::Linked)).unwrap();
    let to_opts = opts(0x4_0000, BlockMode::Independent);

    let mut dec = Decoder::new(&from[..]);
    let mut enc = Encoder::new(Vec::new(), &to_opts, Some(src.len() as u64)).unwrap();
    let copied = io::copy(&mut dec, &mut enc).unwrap();
    assert_eq!(copied, src.len() as u64);
    let transcoded = enc.finish().unwrap();

    let d = decompress(&transcoded).unwrap();
    assert_eq!(d.content, src);
    assert_eq!(d.info.block_size, 0x4_0000);
    assert_eq!(d.info.block_mode, BlockMode::Independent);
}

/// The batch has to satisfy the pool's "worth it" gate, else `-T4` silently ran one thread.
#[test]
fn batches_are_sized_for_the_pool() {
    let block = 64 << 10;
    assert_eq!(super::batch_bytes(block, 1), block, "one worker: one block");
    for workers in 2..=8 {
        let bytes = super::batch_bytes(block, workers);
        assert!(
            bytes >= block * crate::par::BLOCKS_PER_WORKER,
            "{workers} workers must batch enough blocks for the pool to use them"
        );
        assert_eq!(bytes % block, 0);
    }
    assert_eq!(super::batch_bytes(block, 16), block * 16);
}

#[test]
fn content_size_must_be_known_and_correct() {
    let src = corpus(5000);
    let o = Opts {
        content_size: true,
        ..opts(0x1_0000, BlockMode::Independent)
    };
    // declared but unknown up front: the descriptor cannot be written
    assert!(Encoder::new(Vec::new(), &o, None).is_err());
    // declared and wrong: caught at finish, and no end mark has been written, so what came
    // out is detectably incomplete rather than a frame that lies
    let mut enc = Encoder::new(Vec::new(), &o, Some(4)).unwrap();
    enc.write_all(&src).unwrap();
    let err = enc.finish().unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    // declared and right: the frame says so
    let frame = stream_once(&src, &o, 777).unwrap();
    assert_eq!(decompress(&frame).unwrap().info.content_size, Some(5000));
}

#[test]
fn incompressible_blocks_are_stored_and_still_roundtrip() {
    // Miri interprets, it does not compile: the big sweeps would take hours there
    if cfg!(miri) {
        return;
    }

    // xorshift noise: no matches for LZ4 to find, so every block must be stored
    let mut x = 0x2545_F491u32;
    let src: Vec<u8> = (0..70_000)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect();
    let frame = compress(&src, &opts(0x1_0000, BlockMode::Independent)).unwrap();
    assert!(
        frame.len() < src.len() + 64,
        "frame {} for {} bytes",
        frame.len(),
        src.len()
    );
    assert_eq!(decompress(&frame).unwrap().content, src);
    let size_field = u32::from_le_bytes(frame[7..11].try_into().unwrap());
    assert_eq!(size_field & BLOCK_UNCOMPRESSED, BLOCK_UNCOMPRESSED);
}

#[test]
fn empty_frame_and_empty_input() {
    let frame = compress(&[], &Opts::default()).unwrap();
    let d = decompress(&frame).unwrap();
    assert!(d.content.is_empty());
    assert_eq!(d.consumed, frame.len());

    // an empty stream has no frame at all: the reader reports the end, not an error
    let mut dec = Decoder::new(&[][..]);
    let mut out = Vec::new();
    assert_eq!(dec.read_to_end(&mut out).unwrap(), 0);
    assert!(dec.info().is_none());
}

#[test]
fn skippable_frames_are_skipped() {
    let src = corpus(5000);
    let frame = compress(&src, &opts(0x1_0000, BlockMode::Independent)).unwrap();
    let mut stream = Vec::new();
    stream.extend_from_slice(&(SKIPPABLE_START | 3).to_le_bytes());
    stream.extend_from_slice(&5u32.to_le_bytes());
    stream.extend_from_slice(b"hello");
    stream.extend_from_slice(&frame);

    let d = decompress(&stream).unwrap();
    assert_eq!(d.content, src);
    assert_eq!(d.consumed, stream.len());

    let mut bad = Vec::new();
    bad.extend_from_slice(&SKIPPABLE_START.to_le_bytes());
    bad.extend_from_slice(&9_999u32.to_le_bytes());
    assert!(decompress(&bad).is_err());
}

#[test]
fn multi_frame_stream_is_walked_by_consumed() {
    let a = corpus(3000);
    let b = corpus(200);
    let mut stream = compress(&a, &opts(0x1_0000, BlockMode::Linked)).unwrap();
    stream.extend_from_slice(&compress(&b, &opts(0x4_0000, BlockMode::Independent)).unwrap());

    let first = decompress(&stream).unwrap();
    assert_eq!(first.content, a);
    let second = decompress(&stream[first.consumed..]).unwrap();
    assert_eq!(second.content, b);
    assert_eq!(first.consumed + second.consumed, stream.len());

    // and the streaming reader walks all of them in one pass
    let mut dec = Decoder::new(&stream[..]);
    let mut all = Vec::new();
    dec.read_to_end(&mut all).unwrap();
    let mut both = a.clone();
    both.extend_from_slice(&b);
    assert_eq!(all, both);
}

#[test]
fn malformed_headers_are_named_errors() {
    let frame = compress(&corpus(2000), &opts(0x1_0000, BlockMode::Independent)).unwrap();

    let mut bad_magic = frame.clone();
    bad_magic[0] ^= 0xFF;
    assert!(matches!(
        decompress(&bad_magic),
        Err(Error::InvalidFrame("magic number"))
    ));

    let mut bad_version = frame.clone();
    bad_version[4] = (bad_version[4] & 0x3F) | 0x80;
    assert!(matches!(
        decompress(&bad_version),
        Err(Error::InvalidFrame(_))
    ));

    let mut reserved = frame.clone();
    reserved[4] |= 0x02;
    assert!(matches!(
        decompress(&reserved),
        Err(Error::InvalidFrame("reserved flag bit is set"))
    ));

    let mut bad_bd = frame.clone();
    bad_bd[5] |= 0x01;
    assert!(matches!(
        decompress(&bad_bd),
        Err(Error::InvalidFrame("reserved block-size bit is set"))
    ));

    let mut bad_id = frame.clone();
    bad_id[5] = 3 << 4; // ids 0..3 are reserved
    assert!(matches!(decompress(&bad_id), Err(Error::InvalidFrame(_))));

    let mut bad_hc = frame.clone();
    bad_hc[6] ^= 0xFF;
    assert!(matches!(
        decompress(&bad_hc),
        Err(Error::ChecksumMismatch { .. })
    ));

    // every truncation is a clean error, never a panic
    for cut in 0..frame.len() {
        assert!(decompress(&frame[..cut]).is_err(), "cut at {cut}");
    }
}

#[test]
fn corrupt_block_data_is_an_error() {
    let src = corpus(5000);
    let o = Opts {
        block_checksum: true,
        ..opts(0x1_0000, BlockMode::Independent)
    };
    let frame = compress(&src, &o).unwrap();
    for bit in 0..8 {
        let mut broken = frame.clone();
        broken[11] ^= 1 << bit; // inside the first block's data
        assert!(decompress(&broken).is_err(), "bit {bit}");
    }
}

#[test]
fn declared_content_size_is_enforced() {
    let src = corpus(4000);
    let mut frame = compress(
        &src,
        &Opts {
            content_size: true,
            ..opts(0x1_0000, BlockMode::Independent)
        },
    )
    .unwrap();
    frame[6..14].copy_from_slice(&3999u64.to_le_bytes());
    frame[14] = header_checksum(&frame[4..14]);
    assert!(matches!(
        decompress(&frame),
        Err(Error::InvalidFrame(
            "content size does not match the descriptor"
        ))
    ));
}

/// A frame compressed with a dictionary (`lz4 -D`, produced by the oracle): decodable when
/// the dictionary is supplied, and a clean error (not garbage) when it is not.
#[test]
fn dictionary_frames_decode_only_with_their_dictionary() {
    let dict = std::fs::read(format!(
        "{}/tests/fixtures/frame/dict.bin",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    let frame = std::fs::read(format!(
        "{}/tests/fixtures/frame/text-dict.lz4",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    let want = std::fs::read(format!(
        "{}/tests/fixtures/text.raw",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();

    let d = decompress_with_dict(&frame, &dict).expect("decode with the dictionary");
    assert_eq!(d.content, want);
    assert_eq!(d.consumed, frame.len());

    // without it, the very first match points before the output: an error, not noise
    assert!(matches!(
        decompress(&frame),
        Err(Error::InvalidOffset(_)) | Err(Error::Truncated)
    ));

    // a dictionary longer than 64 KiB is used from its tail, as the reference does
    let mut long = vec![0u8; 100_000];
    long[100_000 - dict.len()..].copy_from_slice(&dict);
    let d = decompress_with_dict(&frame, &long).expect("the tail is what matters");
    assert_eq!(d.content, want);
}

#[test]
fn block_sizes_off_the_ladder_are_rejected() {
    let src = corpus(100);
    for bad in [0usize, 0x1_0001, 0x8_0000] {
        assert!(matches!(
            compress(&src, &opts(bad, BlockMode::Independent)),
            Err(Error::Unsupported(_))
        ));
    }
}

/// Whatever the input is, decoding returns a value: no panic, no hang, and no allocation
/// request taken from the data.
#[test]
fn arbitrary_bytes_never_panic() {
    // Miri interprets, it does not compile: the big sweeps would take hours there
    if cfg!(miri) {
        return;
    }

    let valid = compress(&corpus(3000), &opts(0x1_0000, BlockMode::Linked)).unwrap();
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    let mut byte = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x as u8
    };
    for len in [0usize, 1, 4, 7, 15, 40, 200, 1000] {
        for _ in 0..64 {
            let candidate: Vec<u8> = (0..len).map(|_| byte()).collect();
            let _ = decompress(&candidate);
        }
    }
    for _ in 0..500 {
        let mut broken = valid.clone();
        let at = (byte() as usize * 7 + byte() as usize) % broken.len();
        broken[at] ^= 1 << (byte() % 8);
        let _ = decompress(&broken);
    }
}
