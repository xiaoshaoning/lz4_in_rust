//! XXH32 (`lib/xxhash.c`, `doc/lz4_Frame_format.md`).
//!
//! The frame format needs exactly two things from xxhash: a one-shot digest of the
//! frame descriptor, and a streaming digest of block/content data. Both live on one
//! type so the state machine is not duplicated.

const P1: u32 = 0x9E37_79B1;
const P2: u32 = 0x85EB_CA77;
const P3: u32 = 0xC2B2_AE3D;
const P4: u32 = 0x27D4_EB2F;
const P5: u32 = 0x1656_67B1;

const BLOCK: usize = 16;

/// Incremental XXH32 hasher.
#[derive(Clone)]
pub struct Xxh32 {
    lanes: [u32; 4],
    total: u64,
    buf: [u8; BLOCK],
    buf_len: usize,
    seed: u32,
}

impl Xxh32 {
    pub fn new(seed: u32) -> Self {
        Xxh32 {
            lanes: [
                seed.wrapping_add(P1).wrapping_add(P2),
                seed.wrapping_add(P2),
                seed,
                seed.wrapping_sub(P1),
            ],
            total: 0,
            buf: [0; BLOCK],
            buf_len: 0,
            seed,
        }
    }

    /// Digest of `data`, seed 0 — the only form the frame format uses.
    pub fn oneshot(data: &[u8]) -> u32 {
        let mut h = Xxh32::new(0);
        h.update(data);
        h.digest()
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.total += data.len() as u64;

        // top up a partially filled block first
        if self.buf_len > 0 {
            let want = BLOCK - self.buf_len;
            let take = want.min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            data = &data[take..];
            if self.buf_len == BLOCK {
                let lane = read_lane(&self.buf, 0);
                self.lanes[0] = round(self.lanes[0], lane);
                let lane = read_lane(&self.buf, 4);
                self.lanes[1] = round(self.lanes[1], lane);
                let lane = read_lane(&self.buf, 8);
                self.lanes[2] = round(self.lanes[2], lane);
                let lane = read_lane(&self.buf, 12);
                self.lanes[3] = round(self.lanes[3], lane);
                self.buf_len = 0;
            }
        }

        let mut chunks = data.chunks_exact(BLOCK);
        for chunk in &mut chunks {
            self.lanes[0] = round(self.lanes[0], read_lane(chunk, 0));
            self.lanes[1] = round(self.lanes[1], read_lane(chunk, 4));
            self.lanes[2] = round(self.lanes[2], read_lane(chunk, 8));
            self.lanes[3] = round(self.lanes[3], read_lane(chunk, 12));
        }

        // keep a partial block for the next update; an empty remainder must not
        // clobber bytes still buffered from a previous call
        let rest = chunks.remainder();
        if !rest.is_empty() {
            self.buf[..rest.len()].copy_from_slice(rest);
            self.buf_len = rest.len();
        }
    }

    pub fn digest(&self) -> u32 {
        let mut h = if self.total >= BLOCK as u64 {
            let [v1, v2, v3, v4] = self.lanes;
            v1.rotate_left(1)
                .wrapping_add(v2.rotate_left(7))
                .wrapping_add(v3.rotate_left(12))
                .wrapping_add(v4.rotate_left(18))
        } else {
            self.seed.wrapping_add(P5)
        };
        h = h.wrapping_add(self.total as u32);

        let tail = &self.buf[..self.buf_len];
        let mut words = tail.chunks_exact(4);
        for word in &mut words {
            h = h.wrapping_add(read_lane(word, 0).wrapping_mul(P3));
            h = h.rotate_left(17).wrapping_mul(P4);
        }
        for &b in words.remainder() {
            h = h.wrapping_add((b as u32).wrapping_mul(P5));
            h = h.rotate_left(11).wrapping_mul(P1);
        }

        avalanche(h)
    }
}

#[inline]
fn read_lane(buf: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([buf[at], buf[at + 1], buf[at + 2], buf[at + 3]])
}

#[inline]
fn round(acc: u32, lane: u32) -> u32 {
    acc.wrapping_add(lane.wrapping_mul(P2))
        .rotate_left(13)
        .wrapping_mul(P1)
}

#[inline]
fn avalanche(mut h: u32) -> u32 {
    h ^= h >> 15;
    h = h.wrapping_mul(P2);
    h ^= h >> 13;
    h = h.wrapping_mul(P3);
    h ^= h >> 16;
    h
}

#[cfg(test)]
mod tests {
    use super::Xxh32;

    /// Values produced by the C library: `c_oracle xxh32 <file>` (see tools/).
    #[test]
    fn matches_c_oneshot() {
        assert_eq!(Xxh32::oneshot(b""), 0x02cc_5d05);
        assert_eq!(Xxh32::oneshot(b"a"), 0x550d_7456);
        assert_eq!(Xxh32::oneshot(b"abc"), 0x32d1_53ff);
        assert_eq!(
            Xxh32::oneshot(b"Nobody inspects the spammish repetition"),
            0xe229_3b2f
        );
    }

    #[test]
    fn streaming_equals_oneshot_for_any_split() {
        let data: Vec<u8> = (0..1000u32).map(|i| (i * 7 % 251) as u8).collect();
        let want = Xxh32::oneshot(&data);
        for split in [0, 1, 3, 15, 16, 17, 100, 999, 1000] {
            let mut h = Xxh32::new(0);
            h.update(&data[..split]);
            h.update(&data[split..]);
            assert_eq!(h.digest(), want, "split at {split}");
        }
        let mut h = Xxh32::new(0);
        for byte in &data {
            h.update(&[*byte]);
        }
        assert_eq!(h.digest(), want);
    }
}
