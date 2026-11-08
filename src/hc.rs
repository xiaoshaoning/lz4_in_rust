//! High-compression levels (`lib/lz4hc.c`): where `-2..-12` live.
//!
//! Three strategies, one file, because upstream keeps them together and they share the
//! encoder (`encode_sequence`) and the match counter:
//!
//! | levels | strategy | what decides a match |
//! |---|---|---|
//! | 2 | `lz4mid` | two flat hash tables (4 and 7 bytes), first candidate wins, one look at `ip+1` |
//! | 3..=9 | hash chain | a delta chain walked for `nbSearches` probes, then a two-step lookahead (`_Search2`/`_Search3`) |
//! | 10..=12 | optimal parser | a quarter-byte price table over the next 4096 positions, then the cheapest path |
//!
//! All three are **byte-identical** to the C library, so the parts that decide *which* match
//! wins are ported literally: the chain walk order and its `MAX_DISTANCE` clamp, the
//! level-9-only pattern analysis, the lookahead's truncation rules (`OPTIMAL_ML`, the `+3`
//! and `< 3` conditions), the price model, and `encodeSequence`'s token/length layout.
//!
//! Four C branches are absent because the features they serve are not ported: external
//! dictionaries (`dictCtx`/`usingExtDict`), the `fillOutput` directive (`destSize`),
//! `favorDecSpeed` (a frame-layer option the reference tool leaves off), and the
//! interrupted-compression bookkeeping (`suspend`). Their absence is deliberate: the
//! drivers pass the same "no dictionary, not limited" arguments the C does, which makes
//! those branches unreachable.

use crate::block::{self, count_match, Out};
use crate::Error;

/// Hash table size (`LZ4HC_HASH_LOG`).
const HASH_LOG: u32 = 15;
const HASH_SIZE: usize = 1 << HASH_LOG;
/// Chain table size (`LZ4HC_MAXD`): one 16-bit delta per position, indexed modulo 64 KiB.
const CHAIN_SIZE: usize = 1 << 16;
/// Initial `dictLimit`/`lowLimit` for a single buffer (`LZ4HC_init_internal` adds 64 KiB).
const PREFIX_IDX: u32 = 1 << 16;
/// Longest match expressible without an extra length byte (`OPTIMAL_ML`).
const OPTIMAL_ML: usize = (ML_MASK - 1) + MIN_MATCH;

const MIN_MATCH: usize = block::MIN_MATCH;
const MAX_DISTANCE: usize = block::MAX_DISTANCE;
const MIN_LENGTH: usize = block::MFLIMIT + 1;
const ML_MASK: usize = 15;
const ML_BITS: u32 = 4;
const RUN_MASK: usize = 15;
/// First level handled here (`lz4mid`); level 1 is [`crate::block`].
pub const LEVEL_MIN: u8 = 2;
/// Last level handled here (the optimal parser at 12); the CLI's `-1..-12` ends here.
pub const LEVEL_MAX: u8 = 12;
/// `lz4mid`: one hash table over 4 bytes and one over 7, `2^14` buckets each.
const MID_HASHLOG: u32 = HASH_LOG - 1;
const MID_TABLE: usize = 1 << MID_HASHLOG;
/// How many bytes `LZ4MID` hashes (a 7-byte motif hides behind an 8-byte read).
const MID_HASH_BYTES: usize = 8;
/// Probes per position, for levels 3..=9 (`k_clTable`; its first four entries are the
/// `lz4mid` levels this module does not handle, so the table starts at level 3).
const NB_SEARCHES: [u32; 7] = [4, 8, 16, 32, 64, 128, 256];
/// Level 9 and above analyse repeated patterns (`maxNbAttempts > 128` in the C).
const PATTERN_ANALYSIS_FROM: u32 = 128;

/// A match as the search reports it: length, distance back, and how far the start was pulled
/// back over already-counted literals (negative, as in C, so the driver's arithmetic matches).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Match {
    len: isize,
    off: isize,
    back: isize,
}

const NO_MATCH: Match = Match {
    len: 0,
    off: 0,
    back: 0,
};

/// The hash chain and its bookkeeping: indices are "prefix-relative + 64 KiB", as in C, so
/// the tables can be indexed modulo their size and the distance arithmetic stays 16-bit.
struct Chain {
    hash: Vec<u32>,
    chain: Vec<u16>,
    next_to_update: u32,
}

impl Chain {
    fn new() -> Chain {
        Chain {
            hash: vec![0u32; HASH_SIZE],
            // the C fills the chain with 0xFF: an empty slot reads as "65535 behind"
            chain: vec![u16::MAX; CHAIN_SIZE],
            next_to_update: PREFIX_IDX,
        }
    }

    /// `LZ4HC_Insert`: record every position before `ip` in the tables.
    fn insert(&mut self, src: &[u8], ip: usize) {
        let target = ip as u32 + PREFIX_IDX;
        let mut idx = self.next_to_update;
        while idx < target {
            let at = (idx - PREFIX_IDX) as usize;
            let h = hash(read4(src, at)) as usize;
            let mut delta = idx.wrapping_sub(self.hash[h]);
            if delta > MAX_DISTANCE as u32 {
                delta = MAX_DISTANCE as u32;
            }
            self.chain[(idx & 0xFFFF) as usize] = delta as u16;
            self.hash[h] = idx;
            idx += 1;
        }
        self.next_to_update = target;
    }

    #[inline]
    fn delta(&self, idx: u32) -> u32 {
        self.chain[(idx & 0xFFFF) as usize] as u32
    }
}

// ---------------------------------------------------------------- small helpers

#[inline]
fn read4(src: &[u8], at: usize) -> u32 {
    let mut b = [0u8; 4];
    b.copy_from_slice(&src[at..at + 4]);
    u32::from_le_bytes(b)
}

#[inline]
fn read2(src: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([src[at], src[at + 1]])
}

#[inline]
fn can_read2(src: &[u8], at: usize) -> bool {
    at + 2 <= src.len()
}

#[inline]
fn hash(v: u32) -> u32 {
    v.wrapping_mul(2_654_435_761) >> (MIN_MATCH as u32 * 8 - HASH_LOG)
}

#[inline]
fn read8(src: &[u8], at: usize) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&src[at..at + 8]);
    u64::from_le_bytes(b)
}

/// `LZ4HC_countBack`: how many bytes match before `ip`/`m`, bounded by the two floors.
/// Returns a non-positive value, like the C, because call sites add it to positions.
fn count_back(src: &[u8], ip: usize, m: usize, i_min: usize, m_min: usize) -> isize {
    let floor = (ip - i_min).min(m - m_min) as isize; // how far back the walk may go
    let mut back = 0isize;
    // `(back - min) > 3` in the C, where its `min` is our -floor: four bytes at a time
    while back + floor > 3 {
        let at = (ip as isize + back - 4) as usize;
        let other = (m as isize + back - 4) as usize;
        let v = read4(src, at) ^ read4(src, other);
        if v != 0 {
            // `back - LZ4HC_NbCommonBytes32(v)`, which on a little-endian host is
            // `clz(v) >> 3`: the matching bytes *adjacent to the current position* are the
            // high-address ones (an `ip - 4 .. ip` window), so it is leading zeros here,
            // not trailing.
            return back - (v.leading_zeros() as isize / 8);
        }
        back -= 4;
    }
    while -back < floor.abs()
        && src[(ip as isize + back - 1) as usize] == src[(m as isize + back - 1) as usize]
    {
        back -= 1;
    }
    back
}

/// `LZ4HC_countPattern`: how many bytes of `pattern` (a repeated 1/2/4-byte motif) follow `at`.
fn count_pattern(src: &[u8], at: usize, limit: usize, pattern: u32) -> usize {
    let start = at;
    let mut ip = at;
    let motif = u32::from_le(pattern);
    let wide = motif as u64 | ((motif as u64) << 32);
    while ip + 8 <= limit {
        if read8(src, ip) != wide {
            let diff = read8(src, ip) ^ wide;
            return ip - start + (diff.trailing_zeros() as usize) / 8;
        }
        ip += 8;
    }
    let bytes = pattern.to_le_bytes();
    let mut k = 0usize;
    while ip < limit {
        if src[ip] != bytes[k] {
            break;
        }
        ip += 1;
        k = (k + 1) % 4;
    }
    ip - start
}

/// `LZ4HC_reverseCountPattern`: how many bytes of `pattern` precede `at`, not below `low`.
fn reverse_count_pattern(src: &[u8], at: usize, low: usize, pattern: u32) -> usize {
    let start = at;
    let mut ip = at;
    while ip >= low + 4 && read4(src, ip - 4) == pattern {
        ip -= 4;
    }
    let bytes = pattern.to_le_bytes();
    let mut k = 3usize;
    while ip > low && src[ip - 1] == bytes[k] {
        ip -= 1;
        k = (k + 3) % 4; // 3, 2, 1, 0, 3, ... : the motif repeats backwards
    }
    start - ip
}

/// `LZ4HC_protectDictEnd`: a candidate this close to the prefix start cannot be safely probed
/// with a 4-byte read.
#[inline]
fn protect_dict_end(prefix_idx: u32, match_index: u32) -> bool {
    (prefix_idx.wrapping_sub(1)).wrapping_sub(match_index) >= 3
}

/// `LZ4HC_sparsestChain`: a longer match also matches at every position inside the current
/// one; among those, find the chain link that jumps farthest back. Returns the distance to
/// it and where inside the match it sits (the optimal parser uses it to skip candidates
/// that cannot be longer).
fn sparsest_chain(chain: &Chain, match_index: u32, longest: isize) -> (u32, u32) {
    const TRIGGER: i32 = 4;
    let end = longest - MIN_MATCH as isize + 1;
    let mut distance = 1u32;
    let mut chain_pos = 0u32;
    let mut accel = 1i32 << TRIGGER;
    let mut pos = 0i32;
    while (pos as isize) < end {
        let candidate = chain.delta(match_index + pos as u32);
        let step = accel >> TRIGGER;
        accel += 1;
        if candidate > distance {
            distance = candidate;
            chain_pos = pos as u32;
            accel = 1 << TRIGGER;
        }
        pos += step;
    }
    (distance, chain_pos)
}

/// `LZ4HC_patternSegment`, prefix-only (no external dictionary): the run of `pattern` around
/// `candidate`, measured forward and backward, never below `lowest`.
fn pattern_segment(
    src: &[u8],
    candidate: u32,
    pattern: u32,
    high: usize,
    lowest: u32,
) -> Option<(usize, usize)> {
    let at = (candidate - PREFIX_IDX) as usize;
    if read4(src, at) != pattern {
        return None;
    }
    let forward = count_pattern(src, at + 4, high, pattern) + 4;
    let mut back = reverse_count_pattern(src, at, 0, pattern);
    back = (candidate - (candidate - back as u32).max(lowest)) as usize;
    Some((forward, back))
}

// ---------------------------------------------------------------- level 2: lz4mid

#[inline]
fn mid_hash4(v: u32) -> u32 {
    v.wrapping_mul(2_654_435_761) >> (32 - MID_HASHLOG)
}

/// `LZ4MID_hash8Ptr`: seven bytes (`v << 8`) times a 56-bit prime, into `2^14` buckets.
#[inline]
fn mid_hash8(src: &[u8], at: usize) -> u32 {
    const PRIME: u64 = 58_295_818_150_454_627;
    ((read8(src, at) << 8).wrapping_mul(PRIME) >> (64 - MID_HASHLOG)) as u32
}

/// `LZ4MID_countMatch` for a candidate inside the prefix: 0 when out of reach, else the
/// length of the match between `ip` and the position `pos` encoded in the tables.
fn mid_count_match(src: &[u8], ip: usize, ip_index: u32, pos: u32, matchlimit: usize) -> usize {
    if ip_index - pos > MAX_DISTANCE as u32 {
        return 0;
    }
    count_match(src, ip, (pos - PREFIX_IDX) as usize, matchlimit)
}

/// `LZ4MID_compress`: two hash lookups per position, no lookahead, skip faster over
/// incompressible runs.
fn compress_mid(src: &[u8], out: &mut Out) -> Result<usize, Error> {
    let len = src.len();
    if len < MIN_LENGTH {
        encode_last_literals(out, src, 0);
        return Ok(out.at);
    }
    let mut hash4 = vec![0u32; MID_TABLE];
    let mut hash8 = vec![0u32; MID_TABLE];

    let mflimit = len - block::MFLIMIT;
    let matchlimit = len - block::LAST_LITERALS;
    let ilimit = len - MID_HASH_BYTES;
    let mut ip = 0usize;
    let mut anchor = 0usize;

    while ip <= mflimit {
        let ip_index = ip as u32 + PREFIX_IDX;

        // the long (7-byte) hash first: it finds the good matches, the 4-byte one the rest
        let h8 = mid_hash8(src, ip) as usize;
        let pos8 = hash8[h8];
        debug_assert!(pos8 < ip_index, "the tables only hold past positions");
        hash8[h8] = ip_index;
        let mut match_len = mid_count_match(src, ip, ip_index, pos8, matchlimit);
        let mut match_dist = ip_index - pos8;

        if match_len < MIN_MATCH {
            let h4 = mid_hash4(read4(src, ip)) as usize;
            let pos4 = hash4[h4];
            debug_assert!(pos4 < ip_index);
            hash4[h4] = ip_index;
            match_len = mid_count_match(src, ip, ip_index, pos4, matchlimit);
            match_dist = ip_index - pos4;
        }

        if match_len >= MIN_MATCH {
            if match_len < 8 && ip < mflimit {
                // a short match: the next position may start a longer one
                let h8b = mid_hash8(src, ip + 1) as usize;
                let pos8b = hash8[h8b];
                let longer = mid_count_match(src, ip + 1, ip_index + 1, pos8b, matchlimit);
                if longer > match_len && pos8b != ip_index {
                    hash8[h8b] = ip_index + 1;
                    ip += 1;
                    match_len = longer;
                    match_dist = ip_index + 1 - pos8b;
                }
            }

            // pull the match back over bytes already counted as literals
            while ip > anchor
                && (ip as u32) > match_dist
                && src[ip - 1] == src[ip - match_dist as usize - 1]
            {
                ip -= 1;
                match_len += 1;
            }

            // the start of the match is worth hashing (the tables skipped it)
            let start_index = ip as u32 + PREFIX_IDX;
            let at8 = mid_hash8(src, ip + 1) as usize;
            hash8[at8] = start_index + 1;
            let at8 = mid_hash8(src, ip + 2) as usize;
            hash8[at8] = start_index + 2;
            let at4 = mid_hash4(read4(src, ip + 1)) as usize;
            hash4[at4] = start_index + 1;

            encode_sequence(
                out,
                src,
                &mut ip,
                &mut anchor,
                Match {
                    len: match_len as isize,
                    off: match_dist as isize,
                    back: 0,
                },
            );

            // and the end of it
            let end_index = ip as u32 + PREFIX_IDX;
            if end_index - 2 < ilimit as u32 + PREFIX_IDX {
                if ip > 5 {
                    let at = mid_hash8(src, ip - 5) as usize;
                    hash8[at] = end_index - 5;
                }
                let at = mid_hash8(src, ip - 3) as usize;
                hash8[at] = end_index - 3;
                let at = mid_hash8(src, ip - 2) as usize;
                hash8[at] = end_index - 2;
                let at = mid_hash4(read4(src, ip - 2)) as usize;
                hash4[at] = end_index - 2;
                let at = mid_hash4(read4(src, ip - 1)) as usize;
                hash4[at] = end_index - 1;
            }
            continue;
        }

        // no match: skip ahead, faster the longer the literal run gets
        ip += 1 + ((ip - anchor) >> 9);
    }

    encode_last_literals(out, src, anchor);
    Ok(out.at)
}

// ---------------------------------------------------------------- levels 10..=12: optimal

/// Parser window (`LZ4_OPT_NUM`): the price table covers this many positions ahead.
const OPT_NUM: usize = 1 << 12;
/// Positions right after a match also get priced as literals (`TRAILING_LITERALS`).
const TRAILING_LITERALS: usize = 3;
/// Prices are in 1/4 byte, so a sequence and a literal run compare exactly.
const PRICE_SCALE: i32 = 4;

/// (probes, sufficient match length) for levels 10, 11, 12 (`k_clTable`).
const OPT_PARAMS: [(u32, usize); 3] = [(96, 64), (512, 128), (16384, OPT_NUM)];

/// One cell of the price table: `mlen == 1` means "this step is a literal".
#[derive(Clone, Copy, Default)]
struct Cell {
    price: i32,
    off: i32,
    mlen: i32,
    litlen: i32,
}

fn literals_price(litlen: i32) -> i32 {
    debug_assert!(litlen >= 0);
    let mut price = litlen;
    if litlen >= RUN_MASK as i32 {
        price += 1 + (litlen - RUN_MASK as i32) / 255;
    }
    price * PRICE_SCALE
}

fn sequence_price(litlen: i32, mlen: i32) -> i32 {
    debug_assert!(mlen >= MIN_MATCH as i32);
    let mut price = 1 + 2; // token + 16-bit offset
    if mlen >= (ML_MASK + MIN_MATCH) as i32 {
        price += 1 + (mlen - (ML_MASK + MIN_MATCH) as i32) / 255;
    }
    price * PRICE_SCALE + literals_price(litlen)
}

/// Long literal runs only matter through their position inside the 255-byte steps, and the
/// scaled price of a megabyte-long run would overflow an i32.
fn priced_litlen(llen: i32) -> i32 {
    let reduce_from = RUN_MASK as i32 + 255;
    if llen < reduce_from {
        return llen;
    }
    reduce_from + (llen - RUN_MASK as i32) % 255
}

fn run_start_price(opt: &[Cell], pos: usize) -> i32 {
    let ll = opt[pos].litlen as usize;
    if ll <= pos {
        opt[pos - ll].price
    } else {
        0
    }
}

/// `LZ4HC_addMatch`: price every match length reachable from `cur`, then fill the trailing
/// positions with literals. Returns the furthest position priced so far.
fn add_match(opt: &mut [Cell], cur: usize, m: Match, last_pos: usize) -> usize {
    let ll = opt[cur].litlen;
    let base_price = run_start_price(opt, cur);
    let mut last_pos = last_pos;
    let mut ml = MIN_MATCH as i32;
    while ml <= m.len as i32 {
        let pos = cur + ml as usize;
        let price = base_price + sequence_price(ll, ml);
        if pos as i32 > last_pos as i32 + TRAILING_LITERALS as i32 || price <= opt[pos].price {
            opt[pos] = Cell {
                price,
                off: m.off as i32,
                mlen: ml,
                litlen: 0,
            };
            if ml == m.len as i32 && last_pos < pos {
                last_pos = pos;
            }
        }
        ml += 1;
    }
    for add_lit in 1..=TRAILING_LITERALS {
        let pos = last_pos + add_lit;
        opt[pos] = Cell {
            price: opt[last_pos].price + literals_price(add_lit as i32),
            off: 0,
            mlen: 1,
            litlen: add_lit as i32,
        };
    }
    last_pos
}

/// `LZ4HC_FindLongestMatch`: the optimal parser searches with a chain swap and always with
/// pattern analysis, whatever the level.
fn find_longest(
    chain: &mut Chain,
    src: &[u8],
    ip: usize,
    i_high_limit: usize,
    attempts: u32,
) -> Match {
    let m = wider_match(
        chain,
        src,
        ip,
        ip,
        i_high_limit,
        MIN_MATCH as isize - 1,
        attempts,
        true,
        true,
    );
    if m.len < MIN_MATCH as isize {
        NO_MATCH
    } else {
        m
    }
}

/// `LZ4HC_compress_optimal`: dynamic programming over a window of `OPT_NUM` positions, then
/// a backward walk to recover the cheapest path of literals and matches.
///
/// `favorDecSpeed` is not implemented (the reference tool leaves it off, and it is a
/// frame-level preference): the branches and prices it would add are absent, not guessed.
fn compress_optimal(src: &[u8], out: &mut Out, level: u8) -> Result<usize, Error> {
    let (attempts, target_length) = OPT_PARAMS[(level - 10) as usize];
    let sufficient_len = target_length.min(OPT_NUM - 1) as isize;
    let len = src.len();
    if len < MIN_LENGTH {
        encode_last_literals(out, src, 0);
        return Ok(out.at);
    }

    let mut chain = Chain::new();
    let mut opt = vec![Cell::default(); OPT_NUM + TRAILING_LITERALS + 1];
    let mflimit = len - block::MFLIMIT;
    let matchlimit = len - block::LAST_LITERALS;
    let mut ip = 0usize;
    let mut anchor = 0usize;

    while ip <= mflimit {
        let llen = priced_litlen((ip - anchor) as i32);
        let first = find_longest(&mut chain, src, ip, matchlimit, attempts);
        if first.len == 0 {
            ip += 1;
            continue;
        }

        let (last_seq_pos, last_seq) = if first.len > sufficient_len {
            // already good enough: no point pricing anything
            (0usize, first)
        } else {
            // the first MINMATCH positions can only be literals
            for (r, cell) in opt.iter_mut().take(MIN_MATCH).enumerate() {
                *cell = Cell {
                    price: literals_price(llen + r as i32),
                    off: 0,
                    mlen: 1,
                    litlen: llen + r as i32,
                };
            }
            let mut last_pos = add_match(&mut opt, 0, first, 0);

            let mut cur = 1usize;
            let mut immediate: Option<(usize, Match)> = None;
            while cur < last_pos {
                if ip + cur > mflimit {
                    break;
                }
                // the next position is no cheaper: searching here cannot improve the path
                if opt[cur + 1].price <= opt[cur].price {
                    cur += 1;
                    continue;
                }
                let new = find_longest(&mut chain, src, ip + cur, matchlimit, attempts);
                if new.len == 0 {
                    cur += 1;
                    continue;
                }
                if new.len > sufficient_len || new.len + cur as isize >= OPT_NUM as isize {
                    immediate = Some((cur, new));
                    break;
                }
                last_pos = add_match(&mut opt, cur, new, last_pos);
                cur += 1;
            }

            match immediate {
                Some((pos, m)) => (pos, m),
                None => {
                    let m = Match {
                        len: opt[last_pos].mlen as isize,
                        off: opt[last_pos].off as isize,
                        back: 0,
                    };
                    (last_pos - m.len as usize, m)
                }
            }
        };

        // `opt[pos]` describes the step *ending* at pos: walk back, moving each step to its
        // starting position, so the path can then be encoded front to back
        let mut pos = last_seq_pos as isize;
        let mut mlen = last_seq.len as i32;
        let mut off = last_seq.off as i32;
        while pos > 0 {
            let prev_mlen = opt[pos as usize].mlen;
            let prev_off = opt[pos as usize].off;
            opt[pos as usize].mlen = mlen;
            opt[pos as usize].off = off;
            mlen = prev_mlen;
            off = prev_off;
            debug_assert!(mlen > 0 && mlen as isize <= pos);
            pos -= mlen as isize;
        }
        opt[0].mlen = mlen;
        opt[0].off = off;

        let mut r_pos = 0usize;
        while r_pos <= last_seq_pos {
            let ml = opt[r_pos].mlen;
            let offset = opt[r_pos].off;
            if ml == 1 {
                ip += 1;
                r_pos += 1;
                continue;
            }
            r_pos += ml as usize;
            encode_sequence(
                out,
                src,
                &mut ip,
                &mut anchor,
                Match {
                    len: ml as isize,
                    off: offset as isize,
                    back: 0,
                },
            );
        }
    }

    encode_last_literals(out, src, anchor);
    Ok(out.at)
}

// ---------------------------------------------------------------- the search

/// `LZ4HC_InsertAndGetWiderMatch`, prefix-only: walk the chain from `ip`, keep the longest
/// match that also reaches back before `ip` if it can, and report it.
///
/// The C keeps an external-dictionary branch here; this port is single-buffer, so a
/// candidate is always inside the prefix (the tables only ever hold prefix indices).
#[allow(clippy::too_many_arguments)]
fn wider_match(
    chain: &mut Chain,
    src: &[u8],
    ip: usize,
    i_low_limit: usize,
    i_high_limit: usize,
    longest: isize,
    max_attempts: u32,
    pattern_analysis: bool,
    chain_swap: bool,
) -> Match {
    let ip_index = ip as u32 + PREFIX_IDX;
    let within_start = PREFIX_IDX + MAX_DISTANCE as u32 + 1 > ip_index;
    let lowest = if within_start {
        PREFIX_IDX
    } else {
        ip_index - MAX_DISTANCE as u32
    };
    let look_back = ip - i_low_limit;
    let max_length = (i_high_limit - i_low_limit) as isize;
    let mut best = Match {
        len: longest,
        off: 0,
        back: 0,
    };
    let pattern = read4(src, ip);
    let mut attempts = max_attempts;

    chain.insert(src, ip);
    let mut match_index = chain.hash[hash(pattern) as usize];

    // repeated-motif detection is only paid for when a candidate needs it
    let mut match_chain_pos = 0u32;
    let mut repeat_tested = false;
    let mut repeat_confirmed = false;
    let mut src_pattern_length = 0usize;

    // Candidates only replace the current match if strictly longer, so stopping once
    // `longest` reaches `maxLength` cannot change the result.
    while match_index >= lowest && attempts > 0 && best.len < max_length {
        attempts -= 1;
        debug_assert!(match_index < ip_index);
        let match_at = (match_index - PREFIX_IDX) as usize;
        // `LZ4_read16(iLowLimit + longest - 1) == LZ4_read16(matchPtr - lookBack + longest - 1)`:
        // a cheap filter that rejects most candidates with two bytes instead of a match
        // count. The reference reads before its buffer start when lookBack exceeds the
        // candidate position; there is no such slack here, so an unavailable read means
        // "inconclusive" and the 4-byte test below decides.
        debug_assert!(best.len >= 1);
        let probe = best.len as usize;
        let near = match_at + probe - 1;
        let pre = if can_read2(src, i_low_limit + probe - 1)
            && near >= look_back
            && can_read2(src, near - look_back)
        {
            read2(src, i_low_limit + probe - 1) == read2(src, near - look_back)
        } else {
            true
        };
        let mut match_length = 0isize;
        if pre && read4(src, match_at) == pattern {
            let back = if look_back > 0 {
                count_back(src, ip, match_at, i_low_limit, 0)
            } else {
                0
            };
            let found =
                MIN_MATCH + count_match(src, ip + MIN_MATCH, match_at + MIN_MATCH, i_high_limit);
            match_length = found as isize - back;
            if match_length > best.len {
                best.len = match_length;
                best.off = (ip_index - match_index) as isize;
                best.back = back;
            }
        }

        // A longer match must also match at each position inside the current one: if this
        // iteration found one, jump straight to the chain that reaches furthest back
        // (only the optimal parser asks for this: chainSwap).
        if chain_swap && match_length == best.len && match_index + best.len as u32 <= ip_index {
            let (distance, pos) = sparsest_chain(chain, match_index, best.len);
            debug_assert_eq!(look_back, 0, "chainSwap searches forward only");
            if distance > 1 {
                if distance > match_index {
                    break;
                }
                match_index -= distance;
                match_chain_pos = pos;
                continue;
            }
        }

        // pattern analysis (level 9 only): a run of a repeated motif lets the search jump
        // straight to the best position inside that run instead of walking every link
        let next_delta = chain.delta(match_index);
        if pattern_analysis && next_delta == 1 && match_chain_pos == 0 {
            let candidate = match_index - 1;
            if !repeat_tested {
                repeat_tested = true;
                let pattern_is_cyclic =
                    (pattern & 0xFFFF) == (pattern >> 16) && (pattern & 0xFF) == (pattern >> 24);
                repeat_confirmed = pattern_is_cyclic;
                if repeat_confirmed {
                    src_pattern_length = count_pattern(src, ip + 4, i_high_limit, pattern) + 4;
                }
            }
            if repeat_confirmed && candidate >= lowest && protect_dict_end(PREFIX_IDX, candidate) {
                let Some((forward, back)) =
                    pattern_segment(src, candidate, pattern, i_high_limit, lowest)
                else {
                    // not a run of this motif after all: fall through to the chain walk
                    let step = chain.delta(match_index);
                    if step > match_index {
                        break;
                    }
                    match_index -= step;
                    continue;
                };
                let segment = back + forward;
                let full_pattern_fits =
                    segment >= src_pattern_length && forward <= src_pattern_length;
                let new_index = if full_pattern_fits {
                    candidate + forward as u32 - src_pattern_length as u32
                } else {
                    candidate - back as u32
                };
                if !protect_dict_end(PREFIX_IDX, new_index) {
                    match_index = PREFIX_IDX;
                    continue;
                }
                match_index = new_index;
                if !full_pattern_fits && look_back == 0 {
                    let max_ml = segment.min(src_pattern_length) as isize;
                    if best.len < max_ml {
                        if (ip_index - match_index) as usize > MAX_DISTANCE {
                            break;
                        }
                        best.len = max_ml;
                        best.off = (ip_index - match_index) as isize;
                        best.back = 0;
                    }
                    let next = chain.delta(match_index);
                    if next > match_index {
                        break;
                    }
                    match_index -= next;
                }
                continue;
            }
        }

        let step = chain.delta(match_index + match_chain_pos);
        if step > match_index {
            break;
        }
        match_index -= step;
    }

    best
}

fn find_best_match(
    chain: &mut Chain,
    src: &[u8],
    ip: usize,
    i_limit: usize,
    attempts: u32,
    pattern_analysis: bool,
) -> Match {
    // iLowLimit == ip: the search may not pull the match backwards
    wider_match(
        chain,
        src,
        ip,
        ip,
        i_limit,
        MIN_MATCH as isize - 1,
        attempts,
        pattern_analysis,
        false,
    )
}

// ---------------------------------------------------------------- encoding

/// `LZ4HC_encodeSequence`: the same layout as the fast parser, with the match found by the
/// chain search. Returns the new match length after writing (the caller advances `ip`).
fn encode_sequence(out: &mut Out, src: &[u8], ip: &mut usize, anchor: &mut usize, m: Match) {
    let literals = &src[*anchor..*ip];
    let token_at = out.at;
    out.put(0);
    out.put_length(literals.len());
    out.put_slice(literals);
    out.put_u16_le(m.off as u16);

    debug_assert!(m.len >= MIN_MATCH as isize, "encode_sequence needs a match");
    let mut code = m.len as usize - MIN_MATCH;
    if code >= ML_MASK {
        out.put_length(code);
        code = ML_MASK;
    }
    out.set(
        token_at,
        ((literals.len().min(RUN_MASK) << ML_BITS) as u8) | code as u8,
    );
    // the C's encodeSequence ends with `ip += matchLength; anchor = ip;`
    *ip += m.len as usize;
    *anchor = *ip;
}

/// `LZ4HC_encodeLastLiterals`.
fn encode_last_literals(out: &mut Out, src: &[u8], anchor: usize) {
    let rest = &src[anchor..];
    out.put((rest.len().min(RUN_MASK) << ML_BITS) as u8);
    out.put_length(rest.len());
    out.put_slice(rest);
}

// ---------------------------------------------------------------- the driver

/// `LZ4HC_compress_hashChain`: greedy, with a two-step lookahead.
pub(crate) fn compress_into(src: &[u8], dst: &mut [u8], level: u8) -> Result<usize, Error> {
    // positions are u32 (the format's indices), so the input limit is the format's, not this
    // machine's: a larger slice would wrap the arithmetic instead of failing
    if src.len() > block::MAX_INPUT_SIZE {
        return Err(Error::InputTooLarge(src.len()));
    }
    let bound = block::compress_bound(src.len());
    if dst.len() < bound {
        return Err(Error::OutputTooSmall {
            need: bound,
            have: dst.len(),
        });
    }
    if !(LEVEL_MIN..=LEVEL_MAX).contains(&level) {
        return Err(Error::Unsupported(
            "hc::compress_into handles levels 2..=9; the caller must route the others",
        ));
    }
    let mut out = Out { buf: dst, at: 0 };
    if level == 2 {
        return compress_mid(src, &mut out);
    }
    if level >= 10 {
        return compress_optimal(src, &mut out, level);
    }
    let attempts = NB_SEARCHES[(level - LEVEL_MIN - 1) as usize];
    let pattern_analysis = attempts > PATTERN_ANALYSIS_FROM;

    let len = src.len();
    if len < MIN_LENGTH {
        encode_last_literals(&mut out, src, 0);
        return Ok(out.at);
    }

    let mut chain = Chain::new();
    let mflimit = len - block::MFLIMIT;
    let matchlimit = len - block::LAST_LITERALS;

    let mut ip = 0usize;
    let mut anchor = 0usize;

    'outer: while ip <= mflimit {
        let m1 = find_best_match(&mut chain, src, ip, matchlimit, attempts, pattern_analysis);
        if m1.len < MIN_MATCH as isize {
            ip += 1;
            continue;
        }

        // m1 (at ip) is the next match to encode; m0 is the fallback if we skip too far
        let mut start0 = ip;
        let mut m0 = m1;
        let mut m1 = m1;

        'search2: loop {
            let ip_s = ip as isize;

            // search a longer match m2 starting before the end of m1
            let (mut start2, mut m2) = if ip + m1.len as usize <= mflimit {
                let at = ip + m1.len as usize - 2;
                let m2 = wider_match(
                    &mut chain,
                    src,
                    at,
                    ip,
                    matchlimit,
                    m1.len,
                    attempts,
                    pattern_analysis,
                    false,
                );
                (at as isize + m2.back, m2)
            } else {
                (0, NO_MATCH)
            };

            if m2.len <= m1.len {
                // no better match: encode m1 now
                encode_sequence(&mut out, src, &mut ip, &mut anchor, m1);
                continue 'outer;
            }

            if start0 < ip && start2 < ip_s + m0.len {
                // m1 was skipped at least once and m2 starts early enough: restore m0
                ip = start0;
                m1 = m0;
            }

            if start2 - (ip as isize) < 3 {
                // m1 too short before m2: drop it
                ip = start2 as usize;
                m1 = m2;
                continue 'search2;
            }

            // m2 is the candidate after m1; look for m3 before the end of m2
            loop {
                let ip_s = ip as isize;
                // m1 is cut where m2 starts but keeps up to OPTIMAL_ML bytes (no extra
                // length byte); m2 shifts after them while it keeps MINMATCH
                let gap = start2 - ip_s;
                if gap < OPTIMAL_ML as isize {
                    let mut new_ml = m1.len.min(OPTIMAL_ML as isize);
                    let ceiling = gap + m2.len - MIN_MATCH as isize;
                    if new_ml > ceiling {
                        new_ml = ceiling;
                    }
                    if new_ml > gap {
                        let shift = new_ml - gap;
                        start2 += shift;
                        m2.len -= shift;
                    }
                }

                let (start3, m3) = if start2 + m2.len <= mflimit as isize {
                    let at = (start2 + m2.len - 3) as usize;
                    let m3 = wider_match(
                        &mut chain,
                        src,
                        at,
                        start2 as usize,
                        matchlimit,
                        m2.len,
                        attempts,
                        pattern_analysis,
                        false,
                    );
                    (at as isize + m3.back, m3)
                } else {
                    (0, NO_MATCH)
                };

                if m3.len > m2.len && start3 < ip_s + m1.len + 3 {
                    if start3 >= ip_s + m1.len {
                        // m3 starts after m1: encode m1, then m3 becomes m1 and the
                        // (possibly shifted, possibly replaced) m2 becomes the fallback
                        if start2 < ip_s + m1.len {
                            let correction = ip_s + m1.len - start2;
                            start2 += correction;
                            m2.len -= correction;
                            if m2.len < MIN_MATCH as isize {
                                start2 = start3;
                                m2 = m3;
                            }
                        }
                        encode_sequence(&mut out, src, &mut ip, &mut anchor, m1);
                        // `anchor` stays where the encode left it: the one or two bytes
                        // before start3 are literals of the next sequence, not a gap
                        ip = start3 as usize;
                        m1 = m3;
                        start0 = start2 as usize;
                        m0 = m2;
                        continue 'search2;
                    }
                    // m3 starts inside m1: it replaces m2
                    start2 = start3;
                    m2 = m3;
                    continue;
                }

                // encode m1, cut where m2 starts
                if start2 < ip_s + m1.len {
                    m1.len = start2 - ip_s;
                }
                encode_sequence(&mut out, src, &mut ip, &mut anchor, m1);
                ip = start2 as usize;
                m1 = m2;

                if m3.len <= m1.len {
                    // no better match: encode m1 (the former m2) and go on
                    encode_sequence(&mut out, src, &mut ip, &mut anchor, m1);
                    continue 'outer;
                }
                // m3 becomes m2
                start2 = start3;
                m2 = m3;
            }
        }
    }

    encode_last_literals(&mut out, src, anchor);
    Ok(out.at)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::level::Level;

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!(
            "{}/tests/fixtures/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap_or_else(|e| panic!("{name}: {e}"))
    }

    /// The committed fixtures were produced by `LZ4_compress_HC` itself, so this pins the
    /// port's output to C's without needing a C toolchain.
    #[test]
    fn levels_match_the_c_fixtures() {
        // Miri interprets, it does not compile: the big sweeps would take hours there
        if cfg!(miri) {
            return;
        }

        for (raw_name, frame_name, level) in [
            ("text.raw", "text-l9.lz4", 9u8),
            ("numbers.raw", "numbers-l9.lz4", 9),
            ("text.raw", "text-l12.lz4", 12),
        ] {
            let raw = fixture(raw_name);
            let c_block = fixture(frame_name);
            let mut ours = vec![0u8; block::compress_bound(raw.len())];
            let n =
                crate::level::compress_into(&raw, &mut ours, Level::new(level).unwrap()).unwrap();
            assert_eq!(
                &ours[..n],
                &c_block[..],
                "{frame_name}: level {level} output differs from C (ours {n}, C {})",
                c_block.len()
            );
        }
    }

    /// Positions are `u32` here (the format's indices, offset by the 64 KiB prefix), so the
    /// input limit has to leave room for that offset — that is what makes the casts sound.
    #[test]
    fn the_u32_position_arithmetic_cannot_overflow() {
        let max_position = block::MAX_INPUT_SIZE + super::PREFIX_IDX as usize;
        assert!(
            max_position < u32::MAX as usize,
            "MAX_INPUT_SIZE + prefix offset must fit in u32"
        );
    }

    #[test]
    fn every_level_roundtrips() {
        // Miri interprets, it does not compile: the big sweeps would take hours there
        if cfg!(miri) {
            return;
        }

        let mut x = 0x2545_F491u32;
        let raw: Vec<u8> = (0..200_000)
            .map(|i| {
                if i % 7 == 0 {
                    x ^= x << 13;
                    x ^= x >> 17;
                    x ^= x << 5;
                    x as u8
                } else {
                    b"the quick brown fox "[i % 20]
                }
            })
            .collect();
        for level in LEVEL_MIN..=LEVEL_MAX {
            let mut packed = vec![0u8; block::compress_bound(raw.len())];
            let n =
                crate::level::compress_into(&raw, &mut packed, Level::new(level).unwrap()).unwrap();
            let mut back = vec![0xAAu8; raw.len()];
            assert_eq!(
                block::decompress_into(&packed[..n], &mut back).unwrap(),
                raw.len()
            );
            assert_eq!(back, raw, "level {level}");
        }
    }

    #[test]
    fn short_inputs_are_all_literals() {
        for len in [0usize, 1, 12] {
            let raw = vec![b'z'; len];
            let mut packed = vec![0u8; block::compress_bound(len)];
            let n = compress_into(&raw, &mut packed, 9).unwrap();
            let mut back = vec![0u8; len];
            assert_eq!(
                block::decompress_into(&packed[..n], &mut back).unwrap(),
                len
            );
            assert_eq!(back, raw);
        }
    }

    #[test]
    fn small_output_buffer_is_an_error() {
        let raw = vec![b'z'; 1000];
        for level in [2u8, 3, 12] {
            let mut packed = vec![0u8; block::compress_bound(raw.len()) - 1];
            assert!(matches!(
                compress_into(&raw, &mut packed, level),
                Err(Error::OutputTooSmall { .. })
            ));
        }
        // and levels outside 2..=12 are refused rather than clamped
        let mut packed = vec![0u8; block::compress_bound(raw.len())];
        for level in [1u8, 13] {
            assert!(matches!(
                compress_into(&raw, &mut packed, level),
                Err(Error::Unsupported(_))
            ));
        }
    }
}
