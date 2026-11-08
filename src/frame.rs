//! Reading and writing the LZ4 frame format (`doc/lz4_Frame_format.md`).
//!
//! A frame is: a magic number, a descriptor (flags byte, block-size byte, optional content
//! size and dictionary id, and a one-byte XXH32 check of those bytes), a sequence of blocks,
//! an end mark, and an optional XXH32 of the whole decoded content. A block is a 4-byte size
//! — high bit set means "stored uncompressed" — its data, and an optional XXH32 of *that data
//! as written*. Blocks are independent or linked (a linked block may reference up to 64 KiB
//! decoded before it), in sizes of 64 KiB..4 MiB.
//!
//! # Interface
//!
//! Two streaming types carry the format, so memory stays bounded by one batch of blocks:
//!
//! - [`Encoder`] implements [`Write`]: bytes in, blocks out. It buffers a batch (enough jobs
//!   for [`crate::par`]'s pool) and writes each compressed block as it is ready.
//!   [`Encoder::finish`] writes the end mark and the content checksum — a `Write` has no
//!   end-of-stream call, so this cannot be implicit in `drop`.
//! - [`Decoder`] implements [`Read`]: it parses the descriptor, decodes one block at a time
//!   (keeping the 64 KiB a linked block may reference), verifies both checksums, and hands
//!   out the decoded bytes.
//!
//! [`compress`] and [`decompress`] are the whole-buffer conveniences built on them — the
//! same code, so the byte-identity fixtures cover the streaming path too.
//!
//! This module owns every format detail: nothing outside needs the field layout, and nothing
//! inside needs to know how a block is compressed.
//!
//! Scope: dictionaries are supported in both directions — [`Decoder::with_dict`] reads what
//! `lz4 -D` writes, and [`Encoder::with_dict`] writes frames whose linked blocks reference one
//! (`lz4 -BD -D`). Independent-block dictionary frames reach their dictionary through
//! `usingDictCtx` in the reference, which is not ported. A frame that
//! declares a `dictID` decodes like any other — the id lands in [`FrameInfo`] and, as in the
//! reference, is never compared against the dictionary — so such a frame fails on an invalid
//! offset unless the caller supplied the dictionary it was built with.
//! `content_size` can only be declared when the caller knows it up front (a file): a
//! non-seekable stream cannot go back and patch its own descriptor.

use std::io::{self, Read, Write};

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

/// Whether a block may reference the 64 KiB decoded before it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockMode {
    /// Each block stands alone: what `lz4` writes by default, and what the pool needs.
    Independent,
    /// A block may reference the 64 KiB before it: better ratio, serial decoding.
    ///
    /// Writing one is ported for the fast parser (level 1): blocks really do reference their
    /// history, and the bytes match the reference exactly (fixture `large-b4-linked-plain`).
    /// At levels 2..=12 the frame still says "linked" but its blocks are independent — the
    /// high-compression search would have to walk its hash chain across the history, which is
    /// step B of docs/DESIGN.md §11 — so the ratio there equals [`BlockMode::Independent`].
    /// The CLI says so when it happens. Decoding linked frames, dictionary ones included,
    /// works at every level.
    Linked,
}

/// The descriptor of a frame that was read, exactly as it was written there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameInfo {
    /// Largest decoded block, in bytes (64 KiB..4 MiB).
    pub block_size: usize,
    /// Whether a block may reference the 64 KiB before it.
    pub block_mode: BlockMode,
    /// Whether each block carries its own XXH32.
    pub block_checksum: bool,
    /// Whether the frame ends with an XXH32 of the whole content.
    pub content_checksum: bool,
    /// Total content size, when the writer declared it.
    pub content_size: Option<u64>,
    /// Dictionary id, when the writer declared one (this port cannot decode those).
    pub dict_id: Option<u32>,
}

/// What to write (the read side returns [`FrameInfo`] instead: same fields, different
/// meaning — `content_size` here is "declare it", there it is "it was declared").
///
/// It carries the codec knobs as well (`level`, `workers`, `memory_budget`) because a frame
/// is the composition point of the format and the block codec: callers set both in one
/// place, and `par::Opts` is built from here rather than passed in parallel (where its block
/// size could disagree with the frame's).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Opts {
    /// Block size: 64 KiB, 256 KiB, 1 MiB or 4 MiB.
    pub block_size: usize,
    /// Independent blocks decode in parallel; linked ones compress better.
    pub block_mode: BlockMode,
    /// Add a per-block XXH32 (costs 4 bytes per block, catches damage earlier).
    pub block_checksum: bool,
    /// Add the whole-content XXH32 (the default, and what `lz4` writes).
    pub content_checksum: bool,
    /// Declare the content size in the descriptor. Requires knowing it up front, which is why
    /// it is a separate input of [`Encoder::new`] rather than a field of this struct.
    pub content_size: bool,
    /// Compression level for the blocks.
    pub level: Level,
    /// Who picks the worker count for the block pool.
    pub workers: Workers,
    /// Peak-bytes ceiling for the pool's in-flight buffers.
    pub memory_budget: usize,
}

/// Whether this frame's blocks link, which decides between the serial stateful encoder and the
/// parallel one.
///
/// Linking is ported for the fast parser only: the high-compression encoders search a hash chain
/// across the history, which is separate work (docs/DESIGN.md §11, step B). A linked frame at
/// level 2..=12 is therefore written with independent blocks — valid, `lz4` decodes it, but it
/// does not get the ratio — and the CLI says so.
fn linked_encoder(opts: &Opts) -> Option<crate::block::Encoder> {
    (opts.block_mode == BlockMode::Linked && opts.level.is_fast())
        .then(|| crate::block::Encoder::new(1))
}

/// The stateful encoder a dictionary needs, or the reason this frame cannot have one.
///
/// The two refusals are the two mechanisms that are not ported (docs/DESIGN.md §11), decided here
/// once, so the writer and the message cannot disagree.
fn dictionary_encoder(opts: &Opts, dict: &[u8]) -> Result<crate::block::Encoder, &'static str> {
    if opts.block_mode != BlockMode::Linked {
        return Err("compressing with a dictionary needs linked blocks (-BD) for now");
    }
    if !opts.level.is_fast() {
        return Err("compressing with a dictionary is ported for level 1 only");
    }
    let mut enc = crate::block::Encoder::new(1);
    enc.load_dict(dict);
    Ok(enc)
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

/// The result of decoding a whole buffer: the content, the descriptor, and where the frame ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decoded {
    /// The decoded bytes.
    pub content: Vec<u8>,
    /// The descriptor as it was written in the frame.
    pub info: FrameInfo,
    /// Bytes of the input this frame occupied: a caller looping over frames needs it.
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

fn codec(e: Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e)
}

/// `(descriptor bytes, used length)`: flags, block-size id, optional content size.
fn descriptor(opts: &Opts, declared: Option<u64>) -> ([u8; 14], usize) {
    let mut d = [0u8; 14];
    d[0] = flags(opts, declared.is_some());
    d[1] = block_size_id(opts.block_size).expect("validated by the caller") << 4;
    if let Some(size) = declared {
        d[2..10].copy_from_slice(&size.to_le_bytes());
        (d, 10)
    } else {
        (d, 2)
    }
}

/// An option this port or this configuration cannot honour, as an `io::Error`.
fn unsupported(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, Error::Unsupported(message))
}

/// The flag byte for these preferences.
fn flags(opts: &Opts, declare_size: bool) -> u8 {
    let mut flg = VERSION;
    if opts.block_mode == BlockMode::Independent {
        flg |= 0x20;
    }
    if opts.block_checksum {
        flg |= 0x10;
    }
    if declare_size {
        flg |= 0x08;
    }
    if opts.content_checksum {
        flg |= 0x04;
    }
    flg
}

// ================================================================ writing

/// Streaming frame writer: bytes in, blocks out, memory bounded by one batch.
pub struct Encoder<W: Write> {
    out: W,
    opts: Opts,
    pool: par::Opts,
    /// Input not yet compressed; one pool run flushes it.
    pending: Vec<u8>,
    batch: usize,
    /// The block encoder a *linked* frame needs: it keeps the history the next block may
    /// reference, which makes linked compression a dependency chain and therefore serial.
    /// `Some` only when the frame is linked and the level is the fast parser (see
    /// [`BlockMode::Linked`]).
    linked: Option<crate::block::Encoder>,
    /// Content checksum over the *input*, updated as bytes arrive.
    digest: Xxh32,
    written: u64,
    declared: Option<u64>,
}

impl<W: Write> Encoder<W> {
    /// Write the frame descriptor into `out`.
    ///
    /// `content_len` is the total size the frame will hold, needed only when
    /// [`Opts::content_size`] is set: a stream that cannot seek cannot patch its own
    /// descriptor afterwards, so the size must be known now.
    pub fn new(out: W, opts: &Opts, content_len: Option<u64>) -> io::Result<Self> {
        Self::build(out, opts, content_len, None)
    }

    /// Write a frame whose blocks may reference `dict` — what `lz4 -BD -D` does.
    ///
    /// The dictionary's last 64 KiB precede the first block and every block may reach into that
    /// history, so this is the linked path with a `LZ4_loadDict` before the first block. The
    /// reference writes no dictionary id for this (it only does when a caller asks), so neither
    /// do we.
    ///
    /// [`BlockMode::Linked`] and the fast parser are required: independent blocks reach a
    /// dictionary per block through a separate mechanism (`usingDictCtx`), and the
    /// high-compression encoders would have to walk their hash chain into it. Both are later
    /// steps of docs/DESIGN.md §11, and asking here is better than silently producing a frame
    /// that ignores the dictionary.
    pub fn with_dict(
        out: W,
        opts: &Opts,
        content_len: Option<u64>,
        dict: &[u8],
    ) -> io::Result<Self> {
        Self::build(out, opts, content_len, Some(dict))
    }

    fn build(
        mut out: W,
        opts: &Opts,
        content_len: Option<u64>,
        dict: Option<&[u8]>,
    ) -> io::Result<Self> {
        if block_size_id(opts.block_size).is_none() {
            return Err(unsupported(
                "frame block size: must be 64 KiB, 256 KiB, 1 MiB or 4 MiB",
            ));
        }
        let declared = if opts.content_size {
            Some(
                content_len
                    .ok_or_else(|| unsupported("content_size needs the input length up front"))?,
            )
        } else {
            None
        };

        let pool = par::Opts {
            workers: opts.workers,
            block_size: opts.block_size,
            memory_budget: opts.memory_budget,
            level: opts.level,
        };
        // Enough blocks for the policy's "is multi-threading worth it" gate to answer.
        let workers = par::plan(&pool, opts.block_size * par::BLOCKS_PER_WORKER).workers;

        let linked = match dict {
            Some(dict) => Some(dictionary_encoder(opts, dict).map_err(unsupported)?),
            None => linked_encoder(opts),
        };

        let (desc, len) = descriptor(opts, declared);
        let hc = header_checksum(&desc[..len]);
        out.write_all(&MAGIC.to_le_bytes())?;
        out.write_all(&desc[..len])?;
        out.write_all(&[hc])?;

        Ok(Encoder {
            out,
            opts: *opts,
            pool,
            pending: Vec::with_capacity(batch_bytes(opts.block_size, workers)),
            batch: batch_bytes(opts.block_size, workers),
            linked,
            digest: Xxh32::new(0),
            written: 0,
            declared,
        })
    }

    fn flush_batch(&mut self) -> io::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let pending = std::mem::take(&mut self.pending);
        let pool = self.pool;
        let opts = self.opts;
        let out = &mut self.out;
        if let Some(enc) = self.linked.as_mut() {
            // linked: one block at a time, each allowed to reference the ones before it. No pool:
            // a block cannot start before its predecessor exists.
            let mut packed = vec![0u8; crate::block::compress_bound(opts.block_size)];
            let mut start = 0usize;
            while start < pending.len() {
                let raw = &pending[start..(start + opts.block_size).min(pending.len())];
                let n = enc.compress_into(raw, &mut packed).map_err(codec)?;
                write_block(out, raw, &packed[..n], &opts)?;
                start += raw.len();
            }
            self.written += pending.len() as u64;
            self.pending = Vec::with_capacity(self.batch);
            return Ok(());
        }
        // One pool run for the batch (the figure the memory budget is sized for), then write
        // the blocks in order: the frame layer never learns how they were compressed.
        let blocks = par::compress(&pending, &pool).map_err(codec)?;
        for (i, packed) in blocks.iter().enumerate() {
            let start = i * opts.block_size;
            let raw = &pending[start..(start + opts.block_size).min(pending.len())];
            write_block(out, raw, packed, &opts)?;
        }
        self.written += pending.len() as u64;
        self.pending = Vec::with_capacity(self.batch);
        Ok(())
    }

    /// Write the end mark and the content checksum, and hand the writer back.
    ///
    /// A `Write` has no end-of-stream call, so this cannot happen in `drop`: a frame without
    /// an end mark is truncated, not finished.
    pub fn finish(mut self) -> io::Result<W> {
        self.flush_batch()?;
        // Checked before the end mark: a descriptor that lies about its size must not be
        // dressed up as a complete frame. A mismatch means the input was not the length it
        // declared (a file that grew or shrank while being read).
        if let Some(declared) = self.declared {
            if declared != self.written {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "the input declared {declared} bytes but {written} were compressed;                          did the file change while being read?",
                        written = self.written
                    ),
                ));
            }
        }
        self.out.write_all(&0u32.to_le_bytes())?; // end mark
        if self.opts.content_checksum {
            self.out.write_all(&self.digest.digest().to_le_bytes())?;
        }
        self.out.flush()?;
        Ok(self.out)
    }
}

/// How much input to hold before one pool run.
///
/// [`crate::par`] only uses more than one worker when the input holds at least
/// `BLOCKS_PER_WORKER` blocks (below that the pool's own overhead is not worth it), so a batch
/// of `workers` blocks would quietly compress single-threaded for any `-T#` below that. One
/// worker stays at a single block: there is nothing to batch for.
fn batch_bytes(block_size: usize, workers: usize) -> usize {
    let blocks = if workers <= 1 {
        1
    } else {
        workers.max(par::BLOCKS_PER_WORKER)
    };
    block_size * blocks
}

impl<W: Write> Write for Encoder<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.digest.update(buf);
        let mut rest = buf;
        while !rest.is_empty() {
            let room = self.batch - self.pending.len();
            let take = room.min(rest.len());
            self.pending.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
            if self.pending.len() == self.batch {
                self.flush_batch()?;
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flush_batch()?;
        self.out.flush()
    }
}

/// One block: size field (with the stored flag when compression did not help), data, and the
/// optional block checksum, which covers the data *as written*.
fn write_block<W: Write>(out: &mut W, raw: &[u8], packed: &[u8], opts: &Opts) -> io::Result<()> {
    let stored = packed.len() >= raw.len();
    let size_field = if stored {
        (raw.len() as u32) | BLOCK_UNCOMPRESSED
    } else {
        packed.len() as u32
    };
    let data: &[u8] = if stored { raw } else { packed };
    out.write_all(&size_field.to_le_bytes())?;
    out.write_all(data)?;
    if opts.block_checksum {
        out.write_all(&Xxh32::oneshot(data).to_le_bytes())?;
    }
    Ok(())
}

/// Compress a whole buffer into one frame: the same writer, into a `Vec`.
///
/// Layered on [`Encoder`] on purpose: one block writer means the byte-identity fixtures cover
/// [`compress`] and the streaming API alike, and cannot drift apart.
pub fn compress(src: &[u8], opts: &Opts) -> Result<Vec<u8>, Error> {
    compress_via(src, opts, None)
}

/// [`compress`] for a frame whose blocks may reference `dict` (`lz4 -BD -D`).
pub fn compress_with_dict(src: &[u8], opts: &Opts, dict: &[u8]) -> Result<Vec<u8>, Error> {
    compress_via(src, opts, Some(dict))
}

fn compress_via(src: &[u8], opts: &Opts, dict: Option<&[u8]>) -> Result<Vec<u8>, Error> {
    let declared = if opts.content_size {
        Some(src.len() as u64)
    } else {
        None
    };
    let writer = Vec::new();
    let mut enc = match dict {
        Some(dict) => Encoder::with_dict(writer, opts, declared, dict),
        None => Encoder::new(writer, opts, declared),
    }
    .map_err(from_io)?;
    enc.write_all(src).map_err(from_io)?;
    enc.finish().map_err(from_io)
}

/// Recover a codec error from the `io::Error` the frame layer wrapped it in.
///
/// The fallback is unreachable from here: this writer is a `Vec`, whose `write` cannot fail, so
/// the only errors that can surface are the codec errors (bad options) created below and wrapped
/// as `io::Error`s.
fn from_io(e: io::Error) -> Error {
    match e.get_ref() {
        Some(inner) if inner.is::<Error>() => *inner.downcast_ref::<Error>().expect("just checked"),
        _ => Error::Unsupported("frame write failed"),
    }
}

// ================================================================ reading

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Before the first descriptor (skippable frames are skipped here).
    Start,
    /// A descriptor is parsed and its blocks are still being decoded.
    InFrame,
    /// The frame ended; another may follow (that is how `lz4 -m` writes them).
    Between,
    Done,
}

/// Streaming frame reader: descriptor, block by block, both checksums verified.
pub struct Decoder<R: Read> {
    input: R,
    /// The last up-to-64 KiB of a caller-supplied dictionary: a frame compressed with `-D`
    /// may reference it even from its first block, so the window starts out holding it.
    dict_tail: Vec<u8>,
    /// Last up-to-64 KiB of decoded output (what a linked block may reference) with the block
    /// being decoded appended, laid out contiguously so block offsets work unchanged.
    window: Vec<u8>,
    served_from: usize,
    served_to: usize,
    info: Option<FrameInfo>,
    digest: Xxh32,
    produced: u64,
    state: State,
    /// Stop at the end of the first frame instead of continuing into the next one: what the
    /// whole-buffer [`decompress`] reports, while a `Read` consumer usually wants them all.
    one_frame: bool,
    /// The last codec error, so the whole-buffer wrapper need not parse `io::Error`.
    last: Option<Error>,
}

impl<R: Read> Decoder<R> {
    /// Read every frame in the stream, one after another, as `lz4 -d` does.
    pub fn new(input: R) -> Self {
        Self::with_mode(input, false)
    }

    /// Read only the first frame, then report the end of the stream.
    pub fn one_frame(input: R) -> Self {
        Self::with_mode(input, true)
    }

    /// Read frames that were compressed with a dictionary (`lz4 -D`): the dictionary's last
    /// 64 KiB is made reachable to every block, as the reference expects.
    pub fn with_dict(input: R, dict: &[u8]) -> Self {
        let start = dict.len().saturating_sub(crate::block::MAX_DISTANCE);
        let mut d = Self::with_mode(input, false);
        d.dict_tail = dict[start..].to_vec();
        d.window = d.dict_tail.clone();
        d
    }

    fn with_mode(input: R, one_frame: bool) -> Self {
        Decoder {
            input,
            dict_tail: Vec::new(),
            window: Vec::new(),
            served_from: 0,
            served_to: 0,
            info: None,
            digest: Xxh32::new(0),
            produced: 0,
            state: State::Start,
            one_frame,
            last: None,
        }
    }

    /// The descriptor of the current (or last) frame, once its header has been parsed.
    pub fn info(&self) -> Option<FrameInfo> {
        self.info
    }

    /// Content bytes produced for the current frame.
    pub fn produced(&self) -> u64 {
        self.produced
    }

    /// Take the reader back: a caller looping over frames needs its position.
    pub fn into_inner(self) -> R {
        self.input
    }

    /// The last codec error, if the I/O error the caller saw came from this module.
    pub fn last_error(&self) -> Option<Error> {
        self.last
    }

    fn fail<T>(&mut self, e: Error) -> io::Result<T> {
        self.last = Some(e);
        Err(codec(e))
    }

    fn read_array<const N: usize>(&mut self) -> io::Result<[u8; N]> {
        let mut buf = [0u8; N];
        match self.input.read_exact(&mut buf) {
            Ok(()) => Ok(buf),
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                self.fail(Error::Truncated)?;
                unreachable!("fail returns")
            }
            Err(e) => Err(e),
        }
    }

    fn read_u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.read_array::<4>()?))
    }

    /// Parse the next descriptor, skipping skippable frames. `Ok(None)` at a clean end.
    fn read_header(&mut self) -> io::Result<Option<FrameInfo>> {
        let magic = loop {
            let Some(magic) = self.read_magic()? else {
                return Ok(None);
            };
            if magic & SKIPPABLE_MASK != SKIPPABLE_START {
                break magic;
            }
            let mut left = self.read_u32()? as u64;
            let mut sink = [0u8; 8192];
            while left > 0 {
                let want = left.min(sink.len() as u64) as usize;
                self.input.read_exact(&mut sink[..want])?;
                left -= want as u64;
            }
        };
        if magic != MAGIC {
            return self.fail(Error::InvalidFrame("magic number")).map(Some);
        }
        let flg = self.read_array::<1>()?[0];
        let bd = self.read_array::<1>()?[0];
        let mut descriptor = [0u8; 14];
        descriptor[0] = flg;
        descriptor[1] = bd;
        let mut len = 2usize;
        if flg & 0xC0 != VERSION {
            return self
                .fail(Error::InvalidFrame("version: only '01' is defined"))
                .map(Some);
        }
        if flg & 0x02 != 0 {
            return self
                .fail(Error::InvalidFrame("reserved flag bit is set"))
                .map(Some);
        }
        if bd & 0x8F != 0 {
            return self
                .fail(Error::InvalidFrame("reserved block-size bit is set"))
                .map(Some);
        }
        let block_mode = if flg & 0x20 != 0 {
            BlockMode::Independent
        } else {
            BlockMode::Linked
        };
        let block_checksum = flg & 0x10 != 0;
        let content_checksum = flg & 0x04 != 0;
        let Some(block_size) = block_size_from_id((bd >> 4) & 0x7) else {
            return self.fail(Error::InvalidFrame("block size id")).map(Some);
        };
        let content_size = if flg & 0x08 != 0 {
            let bytes = self.read_array::<8>()?;
            descriptor[len..len + 8].copy_from_slice(&bytes);
            len += 8;
            Some(u64::from_le_bytes(bytes))
        } else {
            None
        };
        let dict_id = if flg & 0x01 != 0 {
            let bytes = self.read_array::<4>()?;
            descriptor[len..len + 4].copy_from_slice(&bytes);
            len += 4;
            Some(u32::from_le_bytes(bytes))
        } else {
            None
        };
        let hc = self.read_array::<1>()?[0];
        if hc != header_checksum(&descriptor[..len]) {
            return self
                .fail(Error::ChecksumMismatch {
                    expected: header_checksum(&descriptor[..len]) as u32,
                    actual: hc as u32,
                })
                .map(Some);
        }
        // every frame starts from the dictionary (or from nothing), not from history
        self.window.clear();
        self.window.extend_from_slice(&self.dict_tail);
        self.served_from = 0;
        self.served_to = 0;
        self.digest = Xxh32::new(0);
        self.produced = 0;
        Ok(Some(FrameInfo {
            block_size,
            block_mode,
            block_checksum,
            content_checksum,
            content_size,
            dict_id,
        }))
    }

    /// Read the 4-byte magic, or `None` when the input ends exactly on a frame boundary.
    fn read_magic(&mut self) -> io::Result<Option<u32>> {
        let mut buf = [0u8; 4];
        let mut got = 0usize;
        while got < 4 {
            match self.input.read(&mut buf[got..])? {
                0 if got == 0 => return Ok(None),
                0 => return self.fail(Error::Truncated).map(Some),
                n => got += n,
            }
        }
        Ok(Some(u32::from_le_bytes(buf)))
    }

    /// Decode the next block into the window. `Ok(None)` after the end mark, once the content
    /// checksum and the declared size have been verified.
    fn decode_block(&mut self) -> io::Result<Option<(usize, usize)>> {
        let info = self.info.expect("header parsed");
        let size_field = self.read_u32()?;
        if size_field == 0 {
            if info.content_checksum {
                let want = self.read_u32()?;
                let got = self.digest.digest();
                if want != got {
                    return self
                        .fail(Error::ChecksumMismatch {
                            expected: want,
                            actual: got,
                        })
                        .map(Some);
                }
            }
            if let Some(declared) = info.content_size {
                if self.produced != declared {
                    return self
                        .fail(Error::InvalidFrame(
                            "content size does not match the descriptor",
                        ))
                        .map(Some);
                }
            }
            return Ok(None);
        }

        let stored = size_field & BLOCK_UNCOMPRESSED != 0;
        let size = (size_field & !BLOCK_UNCOMPRESSED) as usize;
        if size > info.block_size {
            return self
                .fail(Error::InvalidFrame(
                    "block is larger than the frame's block size",
                ))
                .map(Some);
        }
        let mut data = vec![0u8; size];
        self.input.read_exact(&mut data)?;
        if info.block_checksum {
            let want = self.read_u32()?;
            let got = Xxh32::oneshot(&data);
            if want != got {
                return self
                    .fail(Error::ChecksumMismatch {
                        expected: want,
                        actual: got,
                    })
                    .map(Some);
            }
        }

        // keep at most MAX_DISTANCE bytes of history, then append this block
        let keep = crate::block::MAX_DISTANCE.min(self.window.len());
        if self.window.len() > keep {
            self.window.drain(..self.window.len() - keep);
        }
        let start = self.window.len();
        let len = if stored {
            self.window.extend_from_slice(&data);
            data.len()
        } else {
            self.window.resize(start + info.block_size, 0);
            let end = match crate::block::decompress_into_at(&data, &mut self.window, start) {
                Ok(end) => end,
                Err(e) => return self.fail(e).map(Some),
            };
            end - start
        };
        self.window.truncate(start + len);
        self.digest.update(&self.window[start..]);
        self.produced += len as u64;
        Ok(Some((start, start + len)))
    }
}

impl<R: Read> Read for Decoder<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.served_from < self.served_to {
                let n = (self.served_to - self.served_from).min(buf.len());
                buf[..n].copy_from_slice(&self.window[self.served_from..self.served_from + n]);
                self.served_from += n;
                return Ok(n);
            }
            match self.state {
                State::Done => return Ok(0),
                State::Start | State::Between => match self.read_header()? {
                    Some(info) => {
                        self.info = Some(info);
                        self.state = State::InFrame;
                    }
                    None => {
                        self.state = State::Done;
                        return Ok(0);
                    }
                },
                State::InFrame => match self.decode_block()? {
                    Some((from, to)) => {
                        self.served_from = from;
                        self.served_to = to;
                    }
                    None => {
                        self.state = if self.one_frame {
                            State::Done
                        } else {
                            State::Between
                        }
                    }
                },
            }
        }
    }
}

/// [`decompress`] for a frame that was compressed with a dictionary.
pub fn decompress_with_dict(src: &[u8], dict: &[u8]) -> Result<Decoded, Error> {
    decode_one(Decoder::with_dict(io::Cursor::new(src), dict))
}

/// Decode **one** frame from a whole buffer, reporting its descriptor and length.
///
/// Stops at the end of the first frame, so a caller holding several concatenated frames
/// (`lz4 -m`) can loop on `consumed`. Use [`Decoder`] to walk them in one pass.
pub fn decompress(src: &[u8]) -> Result<Decoded, Error> {
    decode_one(Decoder::one_frame(io::Cursor::new(src)))
}

/// Read one frame's worth of output, then report where it ended.
fn decode_one(mut dec: Decoder<io::Cursor<&[u8]>>) -> Result<Decoded, Error> {
    let mut content = Vec::new();
    let mut chunk = [0u8; 64 << 10];
    loop {
        let n = match dec.read(&mut chunk) {
            Ok(n) => n,
            Err(_) => return Err(dec.last_error().unwrap_or(Error::Truncated)),
        };
        if n == 0 {
            break;
        }
        content.extend_from_slice(&chunk[..n]);
    }
    let info = dec.info().ok_or(Error::Truncated)?;
    let consumed = dec.into_inner().position() as usize;
    Ok(Decoded {
        content,
        info,
        consumed,
    })
}

#[cfg(test)]
mod tests;
