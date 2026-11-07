//! The LZ4 frame format (`doc/lz4_Frame_format.md`, `lib/lz4frame.c`).
//!
//! A frame is: a magic number, a descriptor (flags byte, block-size byte, optional
//! content size and dictionary id, and a one-byte XXH32 check of those bytes), a sequence
//! of blocks, an end mark, and an optional XXH32 of the whole decoded content.
//! A block is a 4-byte size — high bit set means "stored uncompressed" — its data, and an
//! optional XXH32 of *that data as written*. Blocks are either independent or linked (a
//! linked block may reference up to 64 KiB decoded before it). Block sizes are 64 KiB,
//! 256 KiB, 1 MiB and 4 MiB.
//!
//! This module owns every one of those details: nothing outside it needs the field layout,
//! and nothing inside it needs to know how a block is compressed — that is [`crate::par`],
//! which also supplies the worker pool and writes the blocks in input order.
//!
//! Scope: one frame, wholly in memory. Skippable frames before the data frame are skipped
//! (their payload is not exposed), dictionaries are reported as unsupported rather than
//! decoded wrongly, and a `Read`/`Write` streaming adapter is not here yet.
//!
//! Decoding handles both block modes: a linked block is decoded in place, at its own
//! offset, so its matches can reach back into the 64 KiB before it.
//!
//! Writing a *linked* frame is currently nominal: the block layer has no dictionary
//! support yet, so our blocks never reference the previous one and the ratio is the same
//! as with independent blocks. The frame is valid and any decoder (ours, or `lz4`) reads
//! it correctly; it simply does not cash in the ratio that linking allows.

use crate::level::Level;
use crate::par::{self, Workers, DEFAULT_MEMORY_BUDGET, MAX_BLOCK_SIZE};
use crate::xxhash::Xxh32;
use crate::Error;

/// Frame magic number, little-endian in the file (`LZ4F_MAGICNUMBER`).
pub const MAGIC: u32 = 0x184D_2204;
/// Skippable frames use `0x184D2A50..=0x184D2A5F`.
const SKIPPABLE_MASK: u32 = 0xFFFF_FFF0;
const SKIPPABLE_START: u32 = 0x184D_2A50;
/// High bit of a block's size field: the block is stored, not compressed.
const BLOCK_UNCOMPRESSED: u32 = 0x8000_0000;
/// Version this implementation reads and writes (`01` in the top two flag bits).
const VERSION: u8 = 0x40;
/// Upper bound on the memory reserved up front from the descriptor's content size.
const RESERVE_HINT: u64 = 1 << 20;

/// Whether a block may reference the 64 KiB decoded before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockMode {
    /// The default in `lib/lz4frame.c`: better ratio, serial decoding.
    Linked,
    /// Each block stands alone: what `lz4` writes by default, and what the pool needs.
    Independent,
}

/// The descriptor of a frame that was read, exactly as it was written there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameInfo {
    pub block_size: usize,
    pub block_mode: BlockMode,
    pub block_checksum: bool,
    pub content_checksum: bool,
    pub content_size: Option<u64>,
    pub dict_id: Option<u32>,
}

/// What to write (the read side returns [`FrameInfo`] instead: same fields, different
/// meaning — `content_size` here is "declare it", there it is "it was declared").
///
/// It carries the codec knobs as well (`level`, `workers`, `memory_budget`) because a
/// frame *is* the composition point of the format and the block codec: this way callers
/// set both in one place, and `par::Opts` is built from here instead of being passed in
/// parallel (where its block size could disagree with the frame's).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Opts {
    pub block_size: usize,
    pub block_mode: BlockMode,
    pub block_checksum: bool,
    pub content_checksum: bool,
    /// Write the real content size into the descriptor (worth it for files, not for pipes).
    pub content_size: bool,
    pub level: Level,
    pub workers: Workers,
    pub memory_budget: usize,
}

impl Default for Opts {
    fn default() -> Self {
        Opts {
            block_size: MAX_BLOCK_SIZE,
            block_mode: BlockMode::Independent,
            block_checksum: false,
            content_checksum: true,
            content_size: false,
            level: Level::FAST,
            workers: Workers::Single,
            memory_budget: DEFAULT_MEMORY_BUDGET,
        }
    }
}

/// The result of decoding one frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decoded {
    pub content: Vec<u8>,
    pub info: FrameInfo,
    /// Bytes of the input this frame occupied: a caller decoding a file loops on it.
    pub consumed: usize,
}

fn block_size_id(block_size: usize) -> Option<u8> {
    match block_size {
        0x1_0000 => Some(4),
        0x4_0000 => Some(5),
        0x10_0000 => Some(6),
        0x40_0000 => Some(7),
        _ => None,
    }
}

fn block_size_from_id(id: u8) -> Option<usize> {
    match id {
        4 => Some(0x1_0000),
        5 => Some(0x4_0000),
        6 => Some(0x10_0000),
        7 => Some(0x40_0000),
        _ => None,
    }
}

/// `LZ4F_headerChecksum`: one byte of the XXH32 of the descriptor bytes.
fn header_checksum(descriptor: &[u8]) -> u8 {
    (Xxh32::oneshot(descriptor) >> 8) as u8
}

/// Compress `src` into one complete frame.
///
/// Blocks are cut at `opts.block_size`, compressed by [`crate::par`] (so the worker policy
/// applies), and written in input order. A block whose compressed form is not smaller than
/// the block itself is stored, as the reference does.
pub fn compress(src: &[u8], opts: &Opts) -> Result<Vec<u8>, Error> {
    let id = block_size_id(opts.block_size).ok_or(Error::Unsupported(
        "frame block size: must be 64 KiB, 256 KiB, 1 MiB or 4 MiB",
    ))?;

    let mut flg = VERSION;
    if opts.block_mode == BlockMode::Independent {
        flg |= 0x20;
    }
    if opts.block_checksum {
        flg |= 0x10;
    }
    if opts.content_size {
        flg |= 0x08;
    }
    if opts.content_checksum {
        flg |= 0x04;
    }

    let mut out = Vec::with_capacity(src.len() / 2 + 64);
    out.extend_from_slice(&MAGIC.to_le_bytes());
    let descriptor_start = out.len();
    out.push(flg);
    out.push(id << 4);
    if opts.content_size {
        out.extend_from_slice(&(src.len() as u64).to_le_bytes());
    }
    let hc = header_checksum(&out[descriptor_start..]);
    out.push(hc);

    let pool = par::Opts {
        workers: opts.workers,
        block_size: opts.block_size,
        memory_budget: opts.memory_budget,
        level: opts.level,
    };
    par::compress_each(src, &pool, |i, packed| {
        let start = i * opts.block_size;
        let raw = &src[start..(start + opts.block_size).min(src.len())];
        // same decision as LZ4F_makeBlock: store when compression did not save space
        let stored = packed.len() >= raw.len();
        let size_field = if stored {
            (raw.len() as u32) | BLOCK_UNCOMPRESSED
        } else {
            packed.len() as u32
        };
        out.extend_from_slice(&size_field.to_le_bytes());
        let data: &[u8] = if stored { raw } else { &packed };
        out.extend_from_slice(data);
        if opts.block_checksum {
            out.extend_from_slice(&Xxh32::oneshot(data).to_le_bytes());
        }
        Ok::<(), Error>(())
    })?;

    out.extend_from_slice(&0u32.to_le_bytes()); // end mark
    if opts.content_checksum {
        out.extend_from_slice(&Xxh32::oneshot(src).to_le_bytes());
    }
    Ok(out)
}

/// Decode the first frame in `src`, skipping any skippable frames in front of it.
///
/// `Decoded::consumed` locates the end of the frame, so a loop over a multi-frame file is
/// `let d = decompress(&src[pos..])?; pos += d.consumed;`.
pub fn decompress(src: &[u8]) -> Result<Decoded, Error> {
    let mut ip = 0usize;

    // skippable frames may precede the data frame; their payload is not exposed
    loop {
        let magic = peek_u32(src, ip)?;
        if magic & SKIPPABLE_MASK == SKIPPABLE_START {
            let size = peek_u32(src, ip + 4)? as usize;
            ip = ip
                .checked_add(8)
                .and_then(|v| v.checked_add(size))
                .filter(|end| *end <= src.len())
                .ok_or(Error::Truncated)?;
            continue;
        }
        if magic != MAGIC {
            return Err(Error::InvalidFrame("magic number"));
        }
        break;
    }

    let frame_start = ip;
    ip += 4;
    let flg = read_u8(src, &mut ip)?;
    let bd = read_u8(src, &mut ip)?;

    if flg & 0xC0 != VERSION {
        return Err(Error::InvalidFrame("version: only '01' is defined"));
    }
    if flg & 0x02 != 0 {
        return Err(Error::InvalidFrame("reserved flag bit is set"));
    }
    if bd & 0x8F != 0 {
        return Err(Error::InvalidFrame("reserved block-size bit is set"));
    }
    let block_mode = if flg & 0x20 != 0 {
        BlockMode::Independent
    } else {
        BlockMode::Linked
    };
    let block_checksum = flg & 0x10 != 0;
    let content_checksum = flg & 0x04 != 0;
    let block_size =
        block_size_from_id((bd >> 4) & 0x7).ok_or(Error::InvalidFrame("block size id"))?;
    let content_size = if flg & 0x08 != 0 {
        Some(read_u64(src, &mut ip)?)
    } else {
        None
    };
    let dict_id = if flg & 0x01 != 0 {
        Some(read_u32(src, &mut ip)?)
    } else {
        None
    };

    let hc = read_u8(src, &mut ip)?;
    let expected = header_checksum(&src[frame_start + 4..ip - 1]);
    if hc != expected {
        return Err(Error::ChecksumMismatch {
            expected: expected as u32,
            actual: hc as u32,
        });
    }
    if dict_id.is_some() {
        // decoding it would silently produce the wrong bytes without the dictionary
        return Err(Error::Unsupported(
            "frame declares a dictionary id: dictionaries are not ported",
        ));
    }

    // A declared content size is a claim about the data, not an allocation request: a
    // 15-byte frame saying 2^64-1 must not make us reserve 2^64-1 (that panics with
    // "capacity overflow"). Reserve a sane amount and let growth do the rest.
    let mut content = Vec::with_capacity(content_size.unwrap_or(0).min(RESERVE_HINT) as usize);
    let mut digest = Xxh32::new(0);

    loop {
        let size_field = read_u32(src, &mut ip)?;
        if size_field == 0 {
            break; // end mark
        }
        let stored = size_field & BLOCK_UNCOMPRESSED != 0;
        let size = (size_field & !BLOCK_UNCOMPRESSED) as usize;
        if size > block_size {
            return Err(Error::InvalidFrame(
                "block is larger than the frame's block size",
            ));
        }
        let data = src.get(ip..ip + size).ok_or(Error::Truncated)?;
        ip += size;
        if block_checksum {
            let want = read_u32(src, &mut ip)?;
            let got = Xxh32::oneshot(data);
            if want != got {
                return Err(Error::ChecksumMismatch {
                    expected: want,
                    actual: got,
                });
            }
        }

        let start = content.len();
        if stored {
            content.extend_from_slice(data);
        } else {
            // decode in place: a linked block may reference what is already there
            content.resize(start + block_size, 0);
            let end = crate::block::decompress_into_at(data, &mut content, start)?;
            content.truncate(end);
        }
        digest.update(&content[start..]);

        if let Some(declared) = content_size {
            if content.len() as u64 > declared {
                return Err(Error::InvalidFrame(
                    "content is larger than the descriptor says",
                ));
            }
        }
    }

    if content_checksum {
        let want = read_u32(src, &mut ip)?;
        let got = digest.digest();
        if want != got {
            return Err(Error::ChecksumMismatch {
                expected: want,
                actual: got,
            });
        }
    }
    if let Some(declared) = content_size {
        if content.len() as u64 != declared {
            return Err(Error::InvalidFrame(
                "content size does not match the descriptor",
            ));
        }
    }

    Ok(Decoded {
        content,
        info: FrameInfo {
            block_size,
            block_mode,
            block_checksum,
            content_checksum,
            content_size,
            dict_id,
        },
        consumed: ip,
    })
}

// ---------------------------------------------------------------- little-endian reads

fn peek_u32(src: &[u8], at: usize) -> Result<u32, Error> {
    let b = src.get(at..at + 4).ok_or(Error::Truncated)?;
    Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn read_u8(src: &[u8], ip: &mut usize) -> Result<u8, Error> {
    let b = *src.get(*ip).ok_or(Error::Truncated)?;
    *ip += 1;
    Ok(b)
}

fn read_u32(src: &[u8], ip: &mut usize) -> Result<u32, Error> {
    let v = peek_u32(src, *ip)?;
    *ip += 4;
    Ok(v)
}

fn read_u64(src: &[u8], ip: &mut usize) -> Result<u64, Error> {
    let b = src.get(*ip..*ip + 8).ok_or(Error::Truncated)?;
    *ip += 8;
    Ok(u64::from_le_bytes([
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
    ]))
}

#[cfg(test)]
mod tests {
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
        // one block; this corpus compresses, so the size field is the compressed size and
        // the stored flag is clear
        let size_field = u32::from_le_bytes(frame[15..19].try_into().unwrap());
        assert_eq!(size_field & BLOCK_UNCOMPRESSED, 0);
        assert!(size_field as usize <= 1000, "block {size_field} bytes");

        // decoded back by us, and every field reported as written
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

    #[test]
    fn incompressible_blocks_are_stored_and_still_roundtrip() {
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
        // 70 000 incompressible bytes cannot be smaller than 70 000 stored bytes
        assert!(
            frame.len() < src.len() + 64,
            "frame {} for {} bytes",
            frame.len(),
            src.len()
        );
        assert_eq!(decompress(&frame).unwrap().content, src);
        // the first block's size field must have the stored flag
        let size_field = u32::from_le_bytes(frame[7..11].try_into().unwrap());
        assert_eq!(size_field & BLOCK_UNCOMPRESSED, BLOCK_UNCOMPRESSED);
    }

    #[test]
    fn empty_frame_and_empty_input() {
        let frame = compress(&[], &Opts::default()).unwrap();
        let d = decompress(&frame).unwrap();
        assert!(d.content.is_empty());
        assert_eq!(d.consumed, frame.len());
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

        // a skippable frame whose declared size runs past the input is truncation
        let mut bad = Vec::new();
        bad.extend_from_slice(&SKIPPABLE_START.to_le_bytes());
        bad.extend_from_slice(&9_999u32.to_le_bytes());
        assert_eq!(decompress(&bad), Err(Error::Truncated));
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
    }

    #[test]
    fn malformed_headers_are_named_errors() {
        let frame = compress(&corpus(2000), &opts(0x1_0000, BlockMode::Independent)).unwrap();

        let mut bad_magic = frame.clone();
        bad_magic[0] ^= 0xFF;
        assert_eq!(
            decompress(&bad_magic),
            Err(Error::InvalidFrame("magic number"))
        );

        let mut bad_version = frame.clone();
        bad_version[4] = (bad_version[4] & 0x3F) | 0x80;
        assert!(matches!(
            decompress(&bad_version),
            Err(Error::InvalidFrame(_))
        ));

        let mut reserved = frame.clone();
        reserved[4] |= 0x02;
        assert_eq!(
            decompress(&reserved),
            Err(Error::InvalidFrame("reserved flag bit is set"))
        );

        let mut bad_bd = frame.clone();
        bad_bd[5] |= 0x01;
        assert_eq!(
            decompress(&bad_bd),
            Err(Error::InvalidFrame("reserved block-size bit is set"))
        );

        let mut bad_id = frame.clone();
        bad_id[5] = 3 << 4; // ids 0..3 are reserved
        assert_eq!(
            decompress(&bad_id),
            Err(Error::InvalidFrame("block size id"))
        );

        let mut bad_hc = frame.clone();
        bad_hc[6] ^= 0xFF;
        assert!(matches!(
            decompress(&bad_hc),
            Err(Error::ChecksumMismatch { .. })
        ));

        // and every truncation is a clean error, never a panic
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
            let block_data = 7 + 4; // after magic+descriptor+HC, then the size field
            broken[block_data] ^= 1 << bit;
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
        // claim one byte less than the blocks decode to
        frame[6..14].copy_from_slice(&3999u64.to_le_bytes());
        let hc = header_checksum(&frame[4..14]);
        frame[14] = hc;
        assert!(matches!(
            decompress(&frame),
            Err(Error::InvalidFrame(
                "content is larger than the descriptor says"
            ))
        ));
    }

    /// Regression: a declared content size used to be handed straight to `with_capacity`,
    /// so a tiny hostile frame could abort the process with "capacity overflow".
    #[test]
    fn huge_declared_content_size_is_not_an_allocation_request() {
        let mut frame = compress(
            &corpus(100),
            &Opts {
                content_size: true,
                ..opts(0x1_0000, BlockMode::Independent)
            },
        )
        .unwrap();
        frame[6..14].copy_from_slice(&u64::MAX.to_le_bytes());
        frame[14] = header_checksum(&frame[4..14]);
        assert!(matches!(
            decompress(&frame),
            Err(Error::InvalidFrame(
                "content size does not match the descriptor"
            ))
        ));
    }

    /// Whatever the input is, decoding returns a value: no panic, no hang, no allocation
    /// request taken from the data.
    #[test]
    fn arbitrary_bytes_never_panic() {
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
        // and mutated copies of a valid frame, which get further into the parser
        for _ in 0..500 {
            let mut broken = valid.clone();
            let at = (byte() as usize * 7 + byte() as usize) % broken.len();
            broken[at] ^= 1 << (byte() % 8);
            let _ = decompress(&broken);
        }
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
}
