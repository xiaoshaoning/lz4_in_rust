//! The LZ4 block format (`doc/lz4_Block_format.md`, `lib/lz4.c`).
//!
//! A block is a sequence of runs:
//!
//! ```text
//! token  1B  : hi nibble = literal length, lo nibble = match length - MIN_MATCH
//!             a nibble of 15 means "add the following 0..n 255-bytes, then one byte"
//! literals nB
//! offset 2B  : little-endian distance back into the already-decoded output, 1..=65535
//! match     : MIN_MATCH + the low nibble length; the last run has literals only
//! ```
//!
//! Both directions are `&[u8]`-in / `&mut [u8]`-out. There is no `unsafe` here, and
//! decompression never trusts the input: every read and write is bounds checked, so a
//! corrupt block yields `Err` and never a panic, an overrun, or a hang.

use crate::Error;

/// Shortest encodable match; also the number of bytes the offset can reach back.
pub const MIN_MATCH: usize = 4;
/// A match may not start later than this many bytes before the end of the block.
pub const MFLIMIT: usize = 12;
/// The last run is literals only, and holds at least this many bytes.
pub const LAST_LITERALS: usize = 5;
/// Largest encodable match distance (the format's 16-bit offset).
pub const MAX_DISTANCE: usize = 65535;
/// Largest input block the reference implementation accepts (2 GB - 16 MB).
pub const MAX_INPUT_SIZE: usize = 0x7E00_0000;

/// Shortest input worth looking for matches in.
const MIN_LENGTH: usize = MFLIMIT + 1;
const RUN_MASK: usize = 15;
const ML_MASK: usize = 15;
const ML_BITS: u32 = 4;
/// Hash bits for the `byU32` table (inputs >= 64 KiB), as `LZ4_HASHLOG` in the reference.
const HASH_LOG: u32 = 12;
/// Inputs below this use the reference's `byU16` table with one more hash bit
/// (`LZ4_64Klimit`); the table shape is part of the output, so the port keeps both.
const SMALL_INPUT_LIMIT: usize = 64 * 1024 + MFLIMIT - 1;
/// Sized for the wider of the two tables: `byU16` has one more hash bit than `byU32`,
/// and both shapes share this one array (32 KiB, per call, on the stack).
const TABLE_SIZE: usize = 1 << (HASH_LOG + 1);
/// Number of hash probes before the skip step grows (`LZ4_skipTrigger`).
const SKIP_TRIGGER: u32 = 6;
/// Where the stateful encoder starts its offset space: two windows in. One window is the room the
/// reference's `LZ4_loadDict` reserves for a dictionary (`currentOffset += 64 KB`, so a full-size
/// dictionary still leaves the indices above zero); the second is what keeps index 0 free as the
/// "empty table slot" marker — a dropped candidate saturates to 0, and with a zero-based space it
/// would alias the oldest byte still held.
const HISTORY_START: u32 = 2 * (MAX_DISTANCE as u32 + 1);
/// The reference renumbers its offsets once they pass `2^31` (`LZ4_renormDictT`), keeping them in
/// a `u32` that a long stream would otherwise overflow.
const RENORM_AT: u32 = 0x8000_0000;

/// Smallest destination size that [`compress_into`] accepts for a `src_len`-byte block.
///
/// Identical to `LZ4_COMPRESSBOUND`, and part of the compatibility contract: a buffer
/// this large is always enough, which is why the encoder needs no per-write capacity
/// checks (the only error is the one check on entry).
pub fn compress_bound(src_len: usize) -> usize {
    src_len + src_len / 255 + 16
}

/// Compress `src` into `dst`, returning the compressed size.
///
/// `acceleration` is the reference's raw "step faster" knob: 1 is the default effort,
/// larger values skip more input bytes and are proportionally faster and coarser. Most
/// callers want [`crate::level`] instead, which is the `lz4 -1..-12` scale this maps onto.
/// `dst.len()` must be at least [`compress_bound`]`(src.len())`.
pub fn compress_into(src: &[u8], dst: &mut [u8], acceleration: u32) -> Result<usize, Error> {
    let src_len = src.len();
    if src_len > MAX_INPUT_SIZE {
        return Err(Error::InputTooLarge(src_len));
    }
    let bound = compress_bound(src_len);
    if dst.len() < bound {
        return Err(Error::OutputTooSmall {
            need: bound,
            have: dst.len(),
        });
    }
    let mut out = Out { buf: dst, at: 0 };

    if src_len < MIN_LENGTH {
        put_last_literals(&mut out, src, 0);
        return Ok(out.at);
    }

    let mut table = [0u32; TABLE_SIZE];

    // the reference also picks the table shape by input size; keep that choice
    if src_len < SMALL_INPUT_LIMIT {
        run::<true>(src, &mut out, acceleration, &mut table)
    } else {
        run::<false>(src, &mut out, acceleration, &mut table)
    }
}

/// The greedy parser, generic over the reference's two table shapes (`byU16` for small
/// inputs, `byU32` otherwise). Same code, different hash: that is exactly how the
/// reference keeps its output stable across the 64 KiB switch.
///
/// `src` must hold at least [`MIN_LENGTH`] bytes; the two limits below are its consequences,
/// so they are derived here rather than passed in.
fn run<const BY_U16: bool>(
    src: &[u8],
    out: &mut Out,
    acceleration: u32,
    table: &mut [u32; TABLE_SIZE],
) -> Result<usize, Error> {
    // a match may start at most `MFLIMIT` bytes before the end, so nothing at or beyond
    // `mflimit` is worth hashing
    let mflimit = src.len() - MFLIMIT + 1;
    let matchlimit = src.len() - LAST_LITERALS;
    let mut anchor = 0usize;
    let mut ip = 1usize;
    table[hash::<BY_U16>(src, 0) as usize] = 0;
    let mut forward_h = hash::<BY_U16>(src, ip);

    'outer: loop {
        // ---- find the next match, skipping faster as `acceleration` grows
        let mut match_at;
        let mut forward_ip = ip;
        let mut step = 1usize;
        let mut search_match_nb = acceleration << SKIP_TRIGGER;
        loop {
            let h = forward_h;
            let current = forward_ip as u32;
            let candidate = table[h as usize] as usize;
            ip = forward_ip;
            forward_ip += step;
            step = (search_match_nb >> SKIP_TRIGGER) as usize;
            search_match_nb += 1;

            if forward_ip > mflimit {
                break 'outer;
            }
            forward_h = hash::<BY_U16>(src, forward_ip);
            table[h as usize] = current;

            if candidate < ip
                && candidate + MAX_DISTANCE >= ip
                && read4(src, candidate) == read4(src, ip)
            {
                match_at = candidate;
                break;
            }
        }

        // ---- emit one match after another, until the byte at `ip` starts no match
        loop {
            // pull the match start backwards over bytes already counted as literals
            while ip > anchor && match_at > 0 && src[ip - 1] == src[match_at - 1] {
                ip -= 1;
                match_at -= 1;
            }

            let match_len = count_match(src, ip + MIN_MATCH, match_at + MIN_MATCH, matchlimit);
            put_sequence(out, &src[anchor..ip], ip - match_at, match_len + MIN_MATCH);

            ip += match_len + MIN_MATCH;
            anchor = ip;

            if ip >= mflimit {
                break 'outer;
            }

            match next_match_at::<BY_U16>(src, ip, table) {
                Some(at) => match_at = at,
                None => {
                    ip += 1;
                    forward_h = hash::<BY_U16>(src, ip);
                    continue 'outer;
                }
            }
        }
    }

    put_last_literals(out, src, anchor);
    Ok(out.at)
}

/// Fill the table around `ip` and report a match starting exactly at `ip`, if any.
///
/// The positions tried are `ip-1 + 1` (the byte after the previous occurrence of
/// `ip-1`, requiring 8 matching bytes) before the plain hash candidate: on periodic
/// data this breaks chains of 4-byte matches that the hash alone would produce
/// (`lib/lz4.c`, "Fill table, and test next position").
fn next_match_at<const BY_U16: bool>(
    src: &[u8],
    ip: usize,
    table: &mut [u32; TABLE_SIZE],
) -> Option<usize> {
    let h = hash::<BY_U16>(src, ip);
    let h_prev = hash::<BY_U16>(src, ip - 1);
    let prev = table[h_prev as usize] as usize;
    table[h_prev as usize] = (ip - 1) as u32;

    let ctx = prev + 1;
    let candidate = if ctx < ip && ctx + MAX_DISTANCE >= ip && read8(src, ctx) == read8(src, ip) {
        ctx
    } else {
        table[h as usize] as usize
    };
    table[h as usize] = ip as u32;

    (candidate < ip && candidate + MAX_DISTANCE >= ip && read4(src, candidate) == read4(src, ip))
        .then_some(candidate)
}

/// Decompress a block into `dst`, returning the decoded size.
///
/// `dst.len()` is the capacity; a block that decodes to more than that is
/// `Error::OutputTooSmall`. Trailing input bytes are an error, not a silently
/// accepted tail (the reference returns a short count instead; nobody relies on it).
///
// Performance: on real text 96% of matches are <= 32 bytes, 95% start more than 32 bytes
// back and 89% of literal runs are <= 4 bytes, so almost every run becomes one wide move
// (`[u8; 32]` lowers to whatever the target supports — SSE2 by default, AVX2 with
// `-C target-cpu=x86-64-v3`); the exact and overlapping paths cover the rest. Writing up
// to 31 bytes past a run's logical end is safe: output is produced front to back and
// matches only read behind `op`. Measured and rejected alternatives: docs/DESIGN.md §2.
pub fn decompress_into(src: &[u8], dst: &mut [u8]) -> Result<usize, Error> {
    decompress_impl(src, dst, 0)
}

/// [`decompress_into`] writing at `op_start`, with the bytes before it kept as history.
///
/// This is what a *linked* frame needs: block N may reference up to 64 KiB that was
/// decoded before it, so the caller decodes every block into one buffer at its own offset.
/// `offset > op` is still rejected, so a match can reach back into the history but never
/// before the start of the buffer. Returns the end position (`op_start + decoded length`).
pub fn decompress_into_at(src: &[u8], dst: &mut [u8], op_start: usize) -> Result<usize, Error> {
    if op_start > dst.len() {
        return Err(Error::OutputTooSmall {
            need: op_start,
            have: dst.len(),
        });
    }
    decompress_impl(src, dst, op_start)
}

fn decompress_impl(src: &[u8], dst: &mut [u8], op_start: usize) -> Result<usize, Error> {
    let src_len = src.len();
    let dst_len = dst.len();
    let mut ip = 0usize;
    let mut op = op_start;

    loop {
        // ---- fast loop ------------------------------------------------
        // One wide move per run, no per-run error or length checks. The entry
        // condition bounds every read (48 bytes: token, one length byte, literals,
        // offset) and every write (64 bytes: literals + match at `op`, 32 each), so
        // the body needs no checks of its own. Anything unusual (long match, long
        // literal run, 255-continued length, less than 48/64 bytes of slack) falls
        // through to the checked path, which re-decodes that run from `ip`/`op`.
        // Nothing is written before the last check, so re-decoding is idempotent.
        while ip + 48 <= src_len && op + 64 <= dst_len {
            let token = src[ip];
            let match_nibble = token & ML_MASK as u8;
            let mut lit_len = (token >> ML_BITS) as usize;
            let mut literals_at = ip + 1;

            if match_nibble == ML_MASK as u8 {
                break; // match longer than 18 bytes: checked path
            }
            if lit_len == RUN_MASK {
                let ext = src[literals_at];
                if ext == 255 {
                    break; // continued length: checked path
                }
                lit_len += ext as usize;
                literals_at += 1;
                if lit_len > WIDE {
                    break;
                }
            }

            let match_at = op + lit_len; // the match starts where the literals end
            let offset_at = literals_at + lit_len;
            if offset_at + 2 > src_len {
                break; // block ends here: checked path returns
            }
            let offset = u16::from_le_bytes([src[offset_at], src[offset_at + 1]]) as usize;
            if offset == 0 || offset > match_at {
                return Err(Error::InvalidOffset(offset));
            }

            // Copy widths follow the length ladder instead of always moving 32 bytes:
            // 89% of literal runs are <= 8 bytes and 59% of matches are too, and store
            // traffic is a large share of the cost at these speeds.
            copy_wide_from(src, dst, op, literals_at);

            if offset >= WIDE {
                copy_wide(dst, match_at, match_at - offset);
            } else if offset >= 16 {
                copy16(dst, match_at, match_at - offset);
                copy16(dst, match_at + 16, match_at - offset + 16);
            } else if offset >= 8 {
                copy8(dst, match_at, match_at - offset);
                copy8(dst, match_at + 8, match_at - offset + 8);
                copy8(dst, match_at + 16, match_at - offset + 16);
                copy8(dst, match_at + 24, match_at - offset + 24);
            } else {
                copy_repeating(dst, match_at, offset, match_nibble as usize + MIN_MATCH);
            }

            op = match_at + match_nibble as usize + MIN_MATCH;
            ip = offset_at + 2;
        }

        // ---- checked path: one run, every read and write validated ----
        if ip == src_len {
            return Ok(op); // last run: literals only
        }
        let token = *src.get(ip).ok_or(Error::Truncated)?;
        ip += 1;
        let mut lit_len = (token >> ML_BITS) as usize;
        if lit_len == RUN_MASK {
            lit_len += read_extension(src, &mut ip)?;
        }
        let lit_end = op.checked_add(lit_len).ok_or(Error::OutputTooSmall {
            need: usize::MAX,
            have: dst_len,
        })?;
        if lit_end > dst_len {
            return Err(Error::OutputTooSmall {
                need: lit_end,
                have: dst_len,
            });
        }
        let lit_src_end = ip.checked_add(lit_len).ok_or(Error::Truncated)?;
        if lit_src_end > src_len {
            return Err(Error::Truncated);
        }
        if lit_len <= WIDE && lit_src_end + WIDE <= src_len && lit_end + WIDE <= dst_len {
            copy_wide_from(src, dst, op, ip);
        } else {
            copy_exact_from(src, dst, op, ip, lit_len);
        }
        ip = lit_src_end;
        op = lit_end;

        if ip == src_len {
            return Ok(op); // last run: literals only
        }

        let raw = src.get(ip..ip + 2).ok_or(Error::Truncated)?;
        let offset = u16::from_le_bytes([raw[0], raw[1]]) as usize;
        ip += 2;
        if offset == 0 || offset > op {
            return Err(Error::InvalidOffset(offset));
        }

        let mut match_len = (token & ML_MASK as u8) as usize;
        if match_len == ML_MASK {
            match_len += read_extension(src, &mut ip)?;
        }
        match_len += MIN_MATCH;
        let end = op.checked_add(match_len).ok_or(Error::OutputTooSmall {
            need: usize::MAX,
            have: dst_len,
        })?;
        if end > dst_len {
            return Err(Error::OutputTooSmall {
                need: end,
                have: dst_len,
            });
        }

        // The same distance ladder as the fast loop above, with one difference that explains why
        // it is written twice: this path may not write past the end of the run, because here the
        // destination has not been proven to have `WIDE` bytes of slack.
        if match_len <= WIDE && end + WIDE <= dst_len {
            if offset >= WIDE {
                copy_wide(dst, op, op - offset);
            } else if offset >= 16 {
                copy16(dst, op, op - offset);
                copy16(dst, op + 16, op - offset + 16);
            } else if offset >= 8 {
                copy8(dst, op, op - offset);
                copy8(dst, op + 8, op - offset + 8);
                copy8(dst, op + 16, op - offset + 16);
                copy8(dst, op + 24, op - offset + 24);
            } else {
                copy_repeating(dst, op, offset, match_len);
            }
        } else if offset >= match_len {
            dst.copy_within(op - offset..op - offset + match_len, op); // long, disjoint
        } else {
            copy_repeating(dst, op, offset, match_len);
        }
        op = end;
    }
}

/// Width of the wide moves. 32 bytes is one vector register on every target Rust
/// supports (2x SSE2, 1x AVX2, 1x NEON...), and covers 96% of real matches.
const WIDE: usize = 32;

#[inline(always)]
fn copy_exact_from(src: &[u8], dst: &mut [u8], at: usize, from: usize, len: usize) {
    dst[at..at + len].copy_from_slice(&src[from..from + len]);
}

// The four moves below copy between slices of the same constant length, so there is no
// length to convert or check: `copy_from_slice` lowers to one vector load + store (or two,
// for WIDE on SSE2) exactly like the array conversion it replaces, without an unreachable
// `expect` sitting in a decoder that must never panic.
#[inline(always)]
fn copy_wide_from(src: &[u8], dst: &mut [u8], at: usize, from: usize) {
    dst[at..at + WIDE].copy_from_slice(&src[from..from + WIDE]);
}

#[inline(always)]
fn copy_wide(dst: &mut [u8], at: usize, from: usize) {
    dst.copy_within(from..from + WIDE, at);
}

#[inline(always)]
fn copy16(dst: &mut [u8], at: usize, from: usize) {
    dst.copy_within(from..from + 16, at);
}

#[inline(always)]
fn copy8(dst: &mut [u8], at: usize, from: usize) {
    dst.copy_within(from..from + 8, at);
}

/// Distances below 8 cannot use a wide move (the source would still be unwritten),
/// so grow the copied chunk instead: `offset`, `2*offset`, `4*offset`... Each chunk's
/// source is complete before it is read, which reproduces the format's repeat semantics.
#[inline]
fn copy_repeating(dst: &mut [u8], op: usize, offset: usize, len: usize) {
    let mut written = offset.min(len);
    dst.copy_within(op - offset..op - offset + written, op);
    while written < len {
        let chunk = written.min(len - written);
        dst.copy_within(op..op + chunk, op + written);
        written += chunk;
    }
}

/// Reads a run of `255` bytes plus a terminator, as used for both lengths.
#[inline(always)]
fn read_extension(src: &[u8], ip: &mut usize) -> Result<usize, Error> {
    let mut len = 0usize;
    loop {
        let b = *src.get(*ip).ok_or(Error::Truncated)?;
        *ip += 1;
        len = len.checked_add(b as usize).ok_or(Error::Truncated)?;
        if b != 255 {
            return Ok(len);
        }
    }
}

// ---------------------------------------------------------------- stateful encoder (linked blocks)

/// A block encoder that remembers what it compressed, so the next block can reference it.
///
/// This is the reference's `LZ4_stream_t` driven by `LZ4_compress_fast_continue` in **prefix**
/// mode — the mode a *linked* frame uses, where the history is logically the bytes immediately
/// before the block. (The reference's other mode, an external dictionary buffer, is what a raw
/// `LZ4_compress_fast_continue` with `LZ4_loadDict` does; it is not ported, see
/// docs/DESIGN.md §11.)
///
/// Indices are the reference's: an absolute offset that grows with every byte compressed, with
/// the history occupying `[current - history.len(), current)`. Only the last 64 KiB of history
/// is kept; dropping older bytes renumbers every index by the same delta, which preserves every
/// distance and therefore every decision. `dict_size` is deliberately *not* renumbered — it is
/// the reference's `dictSize`, which only the `dictSmall` rule reads (and that rule can only
/// fire before anything was dropped).
pub(crate) struct Encoder {
    /// The `byU32` table, which is what the reference uses for streams.
    table: [u32; TABLE_SIZE],
    /// History bytes, oldest first: index `i` is `history[i - base]`.
    history: Vec<u8>,
    /// Index of `history[0]`.
    base: u32,
    /// Index just past the history: where the next block starts.
    current: u32,
    /// The reference's `dictSize`: bytes compressed since the stream began (never renumbered).
    dict_size: u64,
    acceleration: u32,
}

impl Encoder {
    /// A fresh encoder with no history. `acceleration` below 1 counts as 1, as the reference does.
    pub(crate) fn new(acceleration: u32) -> Encoder {
        Encoder {
            table: [0; TABLE_SIZE],
            history: Vec::new(),
            base: HISTORY_START,
            current: HISTORY_START,
            dict_size: 0,
            acceleration: acceleration.max(1),
        }
    }

    /// `LZ4_loadDict`: forget everything and make the tail of `dict` the history.
    ///
    /// The reference resets the stream, advances its offset by a whole window, keeps the last
    /// 64 KiB of the dictionary, ignores a dictionary shorter than a hash unit (8 bytes), and
    /// fills the table every three bytes — all of which decides the output, so all of it is here.
    pub(crate) fn load_dict(&mut self, dict: &[u8]) {
        *self = Encoder::new(self.acceleration);
        if dict.len() < 8 {
            return;
        }
        let tail = &dict[dict.len().saturating_sub(MAX_DISTANCE + 1)..]; // 64 KiB, as the reference caps it
        self.history.extend_from_slice(tail);
        self.base = HISTORY_START - self.history.len() as u32;
        self.current = HISTORY_START;
        self.dict_size = self.history.len() as u64;
        let mut i = 0usize;
        while i + 8 <= self.history.len() {
            let h = hash::<false>(&self.history, i) as usize;
            self.table[h] = self.base + i as u32;
            i += 3;
        }
    }

    /// `LZ4_compress_fast_continue` in prefix mode: compress one block, referencing the history.
    ///
    /// Returns the number of bytes written to `dst`, which must hold
    /// [`compress_bound`]`(src.len())`. The block becomes part of the history afterwards, so the
    /// next call may reference it — which is exactly what a linked frame needs.
    pub(crate) fn compress_into(&mut self, src: &[u8], dst: &mut [u8]) -> Result<usize, Error> {
        let src_len = src.len();
        if src_len > MAX_INPUT_SIZE {
            return Err(Error::InputTooLarge(src_len));
        }
        let bound = compress_bound(src_len);
        if dst.len() < bound {
            return Err(Error::OutputTooSmall {
                need: bound,
                have: dst.len(),
            });
        }
        let mut out = Out { buf: dst, at: 0 };
        if src_len < MIN_LENGTH {
            put_last_literals(&mut out, src, 0);
            self.advance(src);
            return Ok(out.at);
        }

        // Where this block starts in the offset space, and the oldest index a match may reach:
        // the reference's `lowLimit` for prefix mode, which allows every distance up to
        // `MAX_DISTANCE` and nothing further.
        //
        // The reference's other guard here (`LZ4_isDictSmall`, which rejects stale table entries
        // when the history is shorter than the window) cannot fire on this path: it needs a
        // partial *loaded* dictionary, and `load_dict` is not ported (docs/DESIGN.md §11).
        let start = self.current;
        let low = self.base.saturating_sub(1);

        let mflimit = src_len - MFLIMIT + 1;
        let matchlimit = src_len - LAST_LITERALS;
        let mut anchor = 0usize;
        let mut ip = 1usize;
        self.table[hash::<false>(src, 0) as usize] = start;
        let mut forward_h = hash::<false>(src, ip);

        'outer: loop {
            // ---- find a candidate, in this block or in the history
            let mut match_at;
            let mut forward_ip = ip;
            let mut step = 1usize;
            let mut search_match_nb = self.acceleration << SKIP_TRIGGER;
            loop {
                let h = forward_h;
                let current = start + forward_ip as u32;
                let candidate = self.table[h as usize];
                ip = forward_ip;
                forward_ip += step;
                step = (search_match_nb >> SKIP_TRIGGER) as usize;
                search_match_nb += 1;
                if forward_ip > mflimit {
                    break 'outer;
                }
                forward_h = hash::<false>(src, forward_ip);
                self.table[h as usize] = current;

                if candidate + (MAX_DISTANCE as u32) < current {
                    continue; // too far to encode
                }
                if self.read4_at(src, candidate) == Some(read4(src, ip)) {
                    match_at = candidate;
                    break;
                }
            }

            // pull the match start back over bytes already counted as literals. Only for a match
            // the *search* found: the reference jumps past its catch-up when the `ip - 1` probe
            // matched, and there `ip == anchor`, so walking back would eat the previous match.
            let mut back = match_at > low && self.byte_at(src, match_at - 1) == Some(src[ip - 1]);
            while back {
                ip -= 1;
                match_at -= 1;
                back = ip > anchor
                    && match_at > low
                    && self.byte_at(src, match_at - 1) == Some(src[ip - 1]);
            }

            // ---- emit matches until the byte at `ip` starts none
            loop {
                let match_len = self.count_at(src, match_at, ip, matchlimit);
                put_sequence(
                    &mut out,
                    &src[anchor..ip],
                    (start + ip as u32 - match_at) as usize,
                    match_len + MIN_MATCH,
                );

                ip += match_len + MIN_MATCH;
                anchor = ip;
                if ip >= mflimit {
                    break 'outer;
                }

                // the reference's `ip - 1` probe: the position after the previous occurrence of
                // `ip - 1`, if it matches 8 bytes, beats the plain hash candidate on periodic data
                match self.probe(src, ip, start) {
                    Some(at) => match_at = at,
                    None => {
                        ip += 1;
                        forward_h = hash::<false>(src, ip);
                        continue 'outer;
                    }
                }
            }
        }

        put_last_literals(&mut out, src, anchor);
        self.advance(src);
        Ok(out.at)
    }

    /// An index as `(bytes, offset into them)`, or `None` when it is outside what we still hold.
    ///
    /// This is *the* mapping from the offset space to memory, and the only place that knows a
    /// candidate below `current` lives in the history. Every other routine asks here — keeping it
    /// in one function is what makes the offset arithmetic readable and testable.
    fn seg<'a>(&'a self, src: &'a [u8], index: u32) -> Option<(&'a [u8], usize)> {
        if index < self.current {
            if index < self.base {
                return None; // dropped from the window
            }
            Some((&self.history, (index - self.base) as usize))
        } else {
            Some((src, (index - self.current) as usize))
        }
    }

    fn byte_at(&self, src: &[u8], index: u32) -> Option<u8> {
        let (buf, at) = self.seg(src, index)?;
        buf.get(at).copied()
    }

    /// The 4-byte test the search loop makes. A candidate within three bytes of the end of the
    /// history cannot be read as four bytes here (the reference reads into whatever follows its
    /// dictionary buffer); it is treated as no match — see docs/DESIGN.md §11, R48.
    fn read4_at(&self, src: &[u8], index: u32) -> Option<u32> {
        let (buf, at) = self.seg(src, index)?;
        (at + 4 <= buf.len()).then(|| read4(buf, at))
    }

    /// The reference's `ip - 1` probe, then the plain hash candidate; `Some` when either matches.
    fn probe(&mut self, src: &[u8], ip: usize, start: u32) -> Option<u32> {
        let h = hash::<false>(src, ip);
        let current = start + ip as u32;
        let h_prev = hash::<false>(src, ip - 1);
        let prev = self.table[h_prev as usize];
        self.table[h_prev as usize] = current - 1;

        let ctx = prev.wrapping_add(1);
        let in_history = ctx < start;
        let ctx_ok = ctx + (MAX_DISTANCE as u32) >= current
            // the eight compared bytes must be inside the history, not straddle the boundary
            && (!in_history || ctx + 8 <= start)
            && self.match8_at(src, ctx, ip);
        let candidate = if ctx_ok { ctx } else { self.table[h as usize] };
        self.table[h as usize] = current;

        let ok = candidate + (MAX_DISTANCE as u32) >= current
            && self.read4_at(src, candidate) == Some(read4(src, ip));
        ok.then_some(candidate)
    }

    fn match8_at(&self, src: &[u8], index: u32, ip: usize) -> bool {
        match self.seg(src, index) {
            Some((buf, at)) if at + 8 <= buf.len() => read8(buf, at) == read8(src, ip),
            _ => false,
        }
    }

    /// The length beyond `MIN_MATCH` of the match at `match_at`/`ip`, as `LZ4_count` reports it
    /// over the logical stream: the history and the block are contiguous for matching purposes,
    /// so a run that reaches the end of the history continues at the start of the block.
    fn count_at(&self, src: &[u8], match_at: u32, ip: usize, matchlimit: usize) -> usize {
        let Some((hist, at)) = self.seg(src, match_at) else {
            return 0;
        };
        if match_at >= self.current {
            return count_match(src, ip + MIN_MATCH, at + MIN_MATCH, matchlimit);
        }
        let limit = (ip + (hist.len() - at)).min(matchlimit);
        let mut len = 0usize;
        while ip + MIN_MATCH + len < limit
            && hist[at + MIN_MATCH + len] == src[ip + MIN_MATCH + len]
        {
            len += 1;
        }
        if ip + MIN_MATCH + len == limit {
            len += count_match(src, ip + MIN_MATCH + len, 0, matchlimit);
        }
        len
    }

    /// Append the block to the history and drop the oldest bytes, shifting every index by the
    /// same delta — which preserves all distances, so no decision changes.
    ///
    /// Two reasons to drop bytes, one operation: the window is full (the reference's
    /// `LZ4_renormDictT`), or the offset space is nearly used up (the reference renumbers for that
    /// at 2^31). `dict_size` keeps counting regardless, because the `dictSmall` rule reads it.
    fn advance(&mut self, src: &[u8]) {
        self.current += src.len() as u32;
        self.dict_size += src.len() as u64;
        self.history.extend_from_slice(src);

        let delta = self
            .history
            .len()
            .saturating_sub(MAX_DISTANCE)
            .max(self.current.saturating_sub(RENORM_AT) as usize);
        if delta == 0 {
            return;
        }
        // Renumbering keeps `base` where it is and moves `current` down: the oldest byte we
        // still hold stays at `base`, a dropped index saturates below it and reads as "empty".
        self.history.drain(..delta);
        self.current -= delta as u32;
        for e in self.table.iter_mut() {
            *e = (*e).saturating_sub(delta as u32);
        }
        debug_assert_eq!(self.current, self.base + self.history.len() as u32);
    }
}

// ---------------------------------------------------------------- encoder helpers

/// Write cursor into the destination block. `at` stays within `compress_bound`,
/// which the encoder checks once on entry — see the module docs. Shared with
/// [`crate::hc`], which writes the same format and has the same guarantee.
pub(crate) struct Out<'a> {
    pub(crate) buf: &'a mut [u8],
    pub(crate) at: usize,
}

impl Out<'_> {
    #[inline]
    pub(crate) fn put(&mut self, b: u8) {
        self.buf[self.at] = b;
        self.at += 1;
    }

    #[inline]
    pub(crate) fn set(&mut self, at: usize, b: u8) {
        self.buf[at] = b;
    }

    #[inline]
    pub(crate) fn put_slice(&mut self, s: &[u8]) {
        self.buf[self.at..self.at + s.len()].copy_from_slice(s);
        self.at += s.len();
    }

    #[inline]
    pub(crate) fn put_u16_le(&mut self, v: u16) {
        self.put_slice(&v.to_le_bytes());
    }

    /// A length nibble is saturated at 15; emit what exceeds it as 255-runs.
    pub(crate) fn put_length(&mut self, mut len: usize) {
        if len < RUN_MASK {
            return;
        }
        len -= RUN_MASK;
        while len >= 255 {
            self.put(255);
            len -= 255;
        }
        self.put(len as u8);
    }
}

/// Emit one sequence: literals, token, 16-bit offset, then the match length.
///
/// The block format's inner layout, and the only place that knows it: the fast parser here and
/// the high-compression encoders in `hc` both write their sequences through this function, so a
/// change to the layout cannot reach one of them and miss the other.
pub(crate) fn put_sequence(out: &mut Out, literals: &[u8], offset: usize, match_len: usize) {
    debug_assert!(match_len >= MIN_MATCH, "a sequence needs a match");
    let token_at = out.at;
    out.put(0); // patched below, once the extended lengths are written
    out.put_length(literals.len());
    out.put_slice(literals);
    out.put_u16_le(offset as u16);
    let mut code = match_len - MIN_MATCH;
    if code >= ML_MASK {
        out.put_length(code);
        code = ML_MASK;
    }
    out.set(
        token_at,
        ((literals.len().min(RUN_MASK) << ML_BITS) as u8) | code as u8,
    );
}

/// Emit the closing run: token, optional length bytes, literals.
pub(crate) fn put_last_literals(out: &mut Out, src: &[u8], anchor: usize) {
    let rest = &src[anchor..];
    out.put((rest.len().min(RUN_MASK) << ML_BITS) as u8);
    out.put_length(rest.len());
    out.put_slice(rest);
}

/// One bounds check, one unaligned load: indexing the four bytes separately would cost four
/// checks per call, and this is the hottest helper in both directions. Shared with `hc`, whose
/// search loads the same way.
#[inline]
pub(crate) fn read4(src: &[u8], at: usize) -> u32 {
    let mut b = [0u8; 4];
    b.copy_from_slice(&src[at..at + 4]);
    u32::from_le_bytes(b)
}

/// One unaligned 8-byte load, shared with `hc` for the same reason as [`read4`].
#[inline]
pub(crate) fn read8(src: &[u8], at: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&src[at..at + 8]);
    u64::from_le_bytes(b)
}

/// `LZ4_hashPosition`: 64-bit builds hash 5 bytes into the `byU32` table (that is what
/// makes its output differ from a plain 4-byte hash) and 4 bytes into `byU16`.
#[inline]
fn hash<const BY_U16: bool>(src: &[u8], at: usize) -> u32 {
    if BY_U16 {
        let hash_log = HASH_LOG + 1;
        read4(src, at).wrapping_mul(2_654_435_761) >> (32 - hash_log)
    } else {
        const PRIME5: u64 = 889_523_592_379;
        ((read8(src, at) << 24).wrapping_mul(PRIME5) >> (64 - HASH_LOG)) as u32
    }
}

/// Length of the match at `ip`/`match_at`, stopping at `limit` (`lib/lz4.c: LZ4_count`).
#[inline]
pub(crate) fn count_match(src: &[u8], mut at: usize, mut other: usize, limit: usize) -> usize {
    debug_assert!(other <= at, "match must start before the current position");
    let start = at;
    while at + 8 <= limit {
        let diff = read8(src, at) ^ read8(src, other);
        if diff == 0 {
            at += 8;
            other += 8;
            continue;
        }
        return at - start + (diff.trailing_zeros() as usize) / 8;
    }
    while at < limit && src[at] == src[other] {
        at += 1;
        other += 1;
    }
    at - start
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(data: &[u8], acceleration: u32) {
        let mut packed = vec![0u8; compress_bound(data.len())];
        let csize = compress_into(data, &mut packed, acceleration).expect("compress");
        assert!(csize <= packed.len());
        // fill with a pattern, not zeros: anything the decoder reads before writing it
        // (the wide copies deliberately write past a run's end) must not be relied upon
        let mut out = vec![0xAAu8; data.len()];
        let dsize = decompress_into(&packed[..csize], &mut out).expect("decompress");
        assert_eq!(dsize, data.len(), "size mismatch for {} bytes", data.len());
        assert_eq!(out, data, "content mismatch for {} bytes", data.len());
    }

    /// Deterministic pseudo-random bytes; no dependency needed.
    fn noise(len: usize, seed: u32) -> Vec<u8> {
        let mut x = seed | 1;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect()
    }

    #[test]
    fn roundtrip_all_sizes() {
        // Miri interprets, it does not compile: the big sweeps would take hours there
        if cfg!(miri) {
            return;
        }

        for len in 0..600 {
            roundtrip(&noise(len, len as u32), 1);
            let repeated = vec![b'x'; len];
            roundtrip(&repeated, 1);
            let text: Vec<u8> = (0..len).map(|i| b"the quick brown fox "[i % 20]).collect();
            roundtrip(&text, 1);
        }
    }

    #[test]
    fn roundtrip_large() {
        // Miri interprets, it does not compile: the big sweeps would take hours there
        if cfg!(miri) {
            return;
        }

        let mut data = noise(100_000, 7);
        // mix in a long repeat so matches reach far back and run long
        let chunk: Vec<u8> = data[0..3000].to_vec();
        for (i, b) in data[50_000..].iter_mut().enumerate() {
            *b = chunk[i % chunk.len()];
        }
        roundtrip(&data, 1);
        roundtrip(&data, 8);
    }

    /// Walks the boundary between the wide fast loop (needs 48 bytes of input and 64
    /// bytes of output slack) and the checked tail: every size around those thresholds,
    /// compressed at several accelerations, with repeats that hit all offset buckets.
    #[test]
    fn roundtrip_across_fast_loop_boundary() {
        // Miri interprets, it does not compile: the big sweeps would take hours there
        if cfg!(miri) {
            return;
        }

        for len in 0..200 {
            let mut data = noise(len, 99 ^ len as u32);
            if len > 40 {
                // force short offsets (1..7) and mid offsets (8..32) in the same block
                for i in 40..len {
                    data[i] = data[i - 3];
                }
            }
            for acceleration in [1, 4, 64] {
                roundtrip(&data, acceleration);
            }
        }
        // sizes right around 48/64/96 bytes of output slack
        for len in [46, 47, 48, 49, 62, 63, 64, 65, 95, 96, 97, 1000] {
            let data = noise(len, 5);
            roundtrip(&data, 1);
            let repeated: Vec<u8> = b"ab".iter().copied().cycle().take(len).collect();
            roundtrip(&repeated, 1);
        }
    }

    #[test]
    fn decompress_rejects_corrupt_input_without_panicking() {
        let data = b"hello hello hello hello hello hello hello".to_vec();
        let mut packed = vec![0u8; compress_bound(data.len())];
        let csize = compress_into(&data, &mut packed, 1).unwrap();

        // every truncation is a clean error
        for cut in 0..csize {
            let mut out = vec![0u8; 1024];
            let _ = decompress_into(&packed[..cut], &mut out);
        }
        // every single-byte corruption is a clean error or a wrong-but-bounded result
        for i in 0..csize {
            for bit in 0..8 {
                let mut broken = packed[..csize].to_vec();
                broken[i] ^= 1 << bit;
                let mut out = vec![0u8; 1024];
                let _ = decompress_into(&broken, &mut out);
            }
        }
    }

    #[test]
    fn decompress_reports_small_output() {
        let data = vec![b'z'; 1000];
        let mut packed = vec![0u8; compress_bound(data.len())];
        let csize = compress_into(&data, &mut packed, 1).unwrap();
        let mut out = vec![0u8; 10];
        assert!(matches!(
            decompress_into(&packed[..csize], &mut out),
            Err(Error::OutputTooSmall { have: 10, .. })
        ));
    }

    #[test]
    fn decompress_rejects_bad_offset() {
        // token = 0 literals + a 4-byte match, offset 0
        let block = [0x00, 0x00, 0x00];
        let mut out = [0u8; 16];
        assert_eq!(
            decompress_into(&block, &mut out),
            Err(Error::InvalidOffset(0))
        );
    }
}
