//! Parallel block compression/decompression, with the adaptive worker policy of
//! `docs/DESIGN.md` §9.
//!
//! A block is a serial chain (matches reach back up to 64 KiB), so the only parallelism
//! is *between* independent blocks. This module is the engine a frame encoder will call:
//! it cuts the input into `block_size` jobs, hands them to a pool of workers through one
//! atomic cursor (dynamic dispatch — fast cores take more jobs, background load is
//! absorbed), and reassembles in input order, so **the output never depends on the
//! worker count**.
//!
//! Nothing here hardcodes a machine: `plan` derives everything from
//! `available_parallelism()`, the block size, the memory budget and the input size.
//!
//! Everything is `unsafe`-free: the shared job list is an `AtomicUsize` cursor, worker
//! results are returned through their join handles, and decoding hands out disjoint
//! `&mut [u8]` slices from a `Mutex`-guarded stack.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use crate::level::{self, Level};
use crate::{block, Error};

/// Smallest block size worth parallelising: below this the lost cross-block matches cost
/// more than the threads win (64 KiB blocks measured +13.7% output size).
pub const MIN_PARALLEL_BLOCK: usize = 1 << 20;
/// Input must hold at least this many blocks for threads to pay off.
pub const BLOCKS_PER_WORKER: usize = 8;
/// At or below this core count, `Auto` uses every core: on a 2-core machine one worker
/// measured 1.0x while two measured 1.62x, and there is no headroom worth sparing.
pub const SMALL_HOST_CORES: usize = 4;
/// Default peak memory for the pool (compressed + decoded buffers per worker).
pub const DEFAULT_MEMORY_BUDGET: usize = 64 << 20;
/// Largest block the LZ4 frame format can describe (`-B7`).
pub const MAX_BLOCK_SIZE: usize = 4 << 20;

/// Who chooses the worker count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Workers {
    /// Decide from the machine and the input (`-T0` / no `-T`).
    Auto,
    /// Use exactly this many (`-T#`); the user overrides the heuristics.
    Fixed(usize),
    /// One thread (`--single-thread`), the default until a caller opts in.
    Single,
}

/// Knobs for [`plan`] / [`compress`] / [`decompress`].
///
/// The library default is single-threaded; the `lz4`-compatible CLI maps "no `-T`" to
/// [`Workers::Auto`] and `--single-thread` to [`Workers::Single`], as the reference does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Opts {
    /// Who picks the worker count.
    pub workers: Workers,
    /// Job size in bytes; the frame layer passes its block size.
    pub block_size: usize,
    /// Peak-bytes ceiling for all in-flight buffers.
    pub memory_budget: usize,
    /// Compression level; the parser choice lives in [`crate::level`], not here.
    pub level: Level,
}

/// The default policy: one thread, 4 MiB jobs, the default memory budget.
impl Default for Opts {
    fn default() -> Self {
        Opts {
            workers: Workers::Single,
            block_size: MAX_BLOCK_SIZE,
            memory_budget: DEFAULT_MEMORY_BUDGET,
            level: Level::FAST,
        }
    }
}

/// What the strategy decided, before any thread is created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    /// Workers the policy chose.
    pub workers: usize,
    /// Job size after clamping to the frame format's limits.
    pub block_size: usize,
    /// Number of jobs the input splits into.
    pub blocks: usize,
}

/// [`plan`] using the machine's own core count.
pub fn plan(opts: &Opts, input_len: usize) -> Plan {
    plan_with(opts, input_len, available_workers())
}

/// The strategy, as a pure function so it can be tested without threads.
///
/// `avail` is the logical CPU count (which respects affinity and cgroup limits).
pub fn plan_with(opts: &Opts, input_len: usize, avail: usize) -> Plan {
    let block_size = opts.block_size.clamp(1, MAX_BLOCK_SIZE);
    let blocks = input_len.div_ceil(block_size);
    let budget_cap = (opts.memory_budget / (2 * block_size)).max(1);

    let workers = match opts.workers {
        Workers::Single => 1,
        // an explicit request is honoured; only physical limits (jobs, memory) apply
        Workers::Fixed(n) => n.max(1).min(blocks.max(1)).min(budget_cap),
        Workers::Auto => {
            // Small machines have nothing worth sparing (the OS preempts anyway, and
            // measured 2 cores / 2 workers = 1.62x vs 1.0x), so use them all. Above that,
            // the reference CLI's headroom formula (LZ4IO_defaultNbWorkers).
            let k0 = if avail <= SMALL_HOST_CORES {
                avail.max(1)
            } else {
                avail.saturating_sub(1 + avail / 8).max(1)
            };
            let worth_it = avail >= 2
                && block_size >= MIN_PARALLEL_BLOCK
                && input_len >= BLOCKS_PER_WORKER * block_size;
            if worth_it {
                k0.min(blocks.max(1)).min(budget_cap)
            } else {
                1
            }
        }
    };

    Plan {
        workers,
        block_size,
        blocks,
    }
}

/// A poisoned lock means a worker panicked; the guarded data here is a counter and a
/// worker count, which are consistent either way. Cascading `expect("control")` panics
/// from every *other* worker would only bury the original failure.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn available_workers() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get())
}

/// The adaptive part: given observed throughput, decide how many workers to keep.
///
/// It can only *shrink*, within one call — the threads are already spawned — and it needs
/// at least two sampling windows before it ever acts, so `Auto` on an input of a few
/// windows is simply the plan's `k0` (see `docs/REVIEW.md` R7 for the stateful version
/// that would also grow and would let the next call start from the last measured best;
/// this API is stateless, so nothing carries a measurement across calls).
#[derive(Debug)]
struct Control {
    k: usize,
    k_lo: usize,
    k_hi: usize,
    best: f64,
    cooldown: u32,
    window: usize,
    bytes: usize,
    secs: f64,
}

impl Control {
    fn new(plan: Plan, workers: usize) -> Self {
        Control {
            k: workers,
            k_lo: 1,
            k_hi: workers,
            best: 0.0,
            cooldown: 0,
            // a window is deliberately large (>= 8 blocks, >= 8 MiB) so that one noisy
            // sample cannot move the worker count; the price is that an input smaller
            // than ~2 windows keeps `k0` for its whole run
            window: (8 * plan.block_size).max(8 << 20),
            bytes: 0,
            secs: 0.0,
        }
    }

    /// Record a finished block; returns the worker count to use from now on.
    ///
    /// Compares against the historical best (not the previous sample), so one noisy
    /// window never causes a bounce back, and only moves by one worker per window.
    fn report(&mut self, bytes: usize, secs: f64) -> usize {
        self.bytes += bytes;
        self.secs += secs;
        if self.bytes < self.window {
            return self.k;
        }
        let now = self.bytes as f64 / self.secs;
        self.bytes = 0;
        self.secs = 0.0;

        if now > self.best * 1.10 {
            self.best = now;
        }
        if self.cooldown > 0 {
            self.cooldown -= 1; // quiet window: no adjustment, by design
        } else if now < self.best * 0.80 {
            self.k = self.k.saturating_sub(1).max(self.k_lo);
            self.cooldown = 3;
        }
        self.k.min(self.k_hi)
    }
}

/// Decode one block into the exact slot the layout assigned it.
///
/// `block::decompress_into` returns however many bytes the stream holds, which may be
/// fewer than the slot: accepting that would leave the caller's buffer partly stale and
/// call it success, so the size check lives here, once, for both the sequential and the
/// pooled path.
fn decode_block(packed: &[u8], dst: &mut [u8]) -> Result<usize, Error> {
    let n = block::decompress_into(packed, dst)?;
    if n != dst.len() {
        return Err(Error::BlockSizeMismatch {
            expected: dst.len(),
            got: n,
        });
    }
    Ok(n)
}

/// Compress `src` as independent blocks of `plan.block_size`.
///
/// The result is in input order and does not depend on the worker count.
pub fn compress(src: &[u8], opts: &Opts) -> Result<Vec<Vec<u8>>, Error> {
    compress_with(src, opts, available_workers())
}

/// [`compress`] with an explicit core count (tests / `-T#` on a restricted host).
pub fn compress_with(src: &[u8], opts: &Opts, avail: usize) -> Result<Vec<Vec<u8>>, Error> {
    let mut blocks = Vec::with_capacity(plan_with(opts, src.len(), avail).blocks);
    compress_each_with(src, opts, avail, |_, bytes| {
        blocks.push(bytes);
        Ok(())
    })?;
    Ok(blocks)
}

/// Compress `src` block by block, handing each finished block to `sink` **in input order**.
///
/// This is the streaming form: a frame encoder writes each block out and drops it, so
/// peak memory is the pool's window (`workers + 1` blocks, bounded by
/// [`Opts::memory_budget`]) instead of the whole compressed output. [`compress`] is the
/// convenience wrapper that collects them instead.
///
/// `sink` receives the block index and owns the block's buffer. Its error type is free
/// (`E: From<Error>`), so a caller that also does I/O keeps its own error type — the codec
/// errors are converted into it instead of being flattened into the caller's API.
pub fn compress_each<F, E>(src: &[u8], opts: &Opts, sink: F) -> Result<(), E>
where
    F: FnMut(usize, Vec<u8>) -> Result<(), E>,
    E: From<Error> + Send,
{
    compress_each_with(src, opts, available_workers(), sink)
}

/// [`compress_each`] with an explicit core count.
pub fn compress_each_with<F, E>(src: &[u8], opts: &Opts, avail: usize, mut sink: F) -> Result<(), E>
where
    F: FnMut(usize, Vec<u8>) -> Result<(), E>,
    E: From<Error> + Send,
{
    let plan = plan_with(opts, src.len(), avail);
    let range = |i: usize| {
        let start = i * plan.block_size;
        let end = (start + plan.block_size).min(src.len());
        start..end
    };
    let one = |i: usize| -> Result<Vec<u8>, Error> {
        let r = range(i);
        let mut buf = vec![0u8; block::compress_bound(r.len())];
        let n = level::compress_into(&src[r], &mut buf, opts.level)?;
        buf.truncate(n);
        Ok(buf)
    };

    if plan.workers <= 1 || plan.blocks <= 1 {
        for i in 0..plan.blocks {
            sink(i, one(i)?)?;
        }
        return Ok(());
    }

    // Workers fill a window of finished blocks; the calling thread drains it in order.
    // `announce` wakes the drainer, `cancel` stops the workers when the drainer gives up
    // (sink error) or when a worker fails.
    let cursor = AtomicUsize::new(0);
    let cancel = AtomicBool::new(false);
    let control = Mutex::new(Control::new(plan, plan.workers));
    let ready: Mutex<BTreeMap<usize, Vec<u8>>> = Mutex::new(BTreeMap::new());
    let announce = Condvar::new();

    std::thread::scope(|s| -> Result<(), E> {
        let mut handles = Vec::with_capacity(plan.workers);
        for id in 0..plan.workers {
            let (cursor, cancel, control) = (&cursor, &cancel, &control);
            let (ready, announce) = (&ready, &announce);
            let range = &range;
            handles.push(s.spawn(move || -> Result<(), E> {
                let mut wake = WakeOnFailure {
                    cancel,
                    announce,
                    armed: true,
                };
                loop {
                    if cancel.load(Ordering::Relaxed) || id >= lock(control).k {
                        break; // cancelled, or the controller shrank below us
                    }
                    let i = cursor.fetch_add(1, Ordering::Relaxed);
                    if i >= plan.blocks {
                        break; // the queue is empty
                    }
                    let r = range(i);
                    let raw_len = r.len();
                    let started = Instant::now();
                    let mut buf = vec![0u8; block::compress_bound(raw_len)];
                    let n = level::compress_into(&src[r], &mut buf, opts.level)?;
                    buf.truncate(n);
                    lock(ready).insert(i, buf);
                    announce.notify_all();
                    lock(control).report(raw_len, started.elapsed().as_secs_f64());
                }
                wake.armed = false;
                Ok(())
            }));
        }

        let mut next = 0usize;
        while next < plan.blocks {
            let block = {
                let mut q: MutexGuard<BTreeMap<usize, Vec<u8>>> = lock(&ready);
                loop {
                    if let Some(bytes) = q.remove(&next) {
                        break Some(bytes);
                    }
                    if cancel.load(Ordering::Relaxed) {
                        break None; // a worker gave up; its error comes back at the join
                    }
                    q = announce.wait(q).unwrap_or_else(PoisonError::into_inner);
                }
            };
            let Some(bytes) = block else { break };
            if let Err(e) = sink(next, bytes) {
                cancel.store(true, Ordering::Relaxed);
                announce.notify_all();
                return Err(e);
            }
            next += 1;
        }

        let mut failure: Option<E> = None;
        for h in handles {
            match h
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            {
                Ok(()) => {}
                Err(e) => failure = failure.or(Some(e)),
            }
        }
        failure.map_or(Ok(()), Err)
    })
}

/// Makes sure the draining thread is woken when a worker fails or panics, so it cannot
/// block forever on `announce`. A worker that finished normally disarms it: setting
/// `cancel` then would abort its still-running siblings.
struct WakeOnFailure<'a> {
    cancel: &'a AtomicBool,
    announce: &'a Condvar,
    armed: bool,
}

impl Drop for WakeOnFailure<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.cancel.store(true, Ordering::Relaxed);
            self.announce.notify_all();
        }
    }
}

/// Decompress independent blocks into `out`, which must be exactly `original_len` bytes.
///
/// `blocks` are the compressed blocks in input order; block *i* decodes to
/// `min(block_size, out.len() - i*block_size)` bytes. Taking the buffer from the caller is
/// what lets a frame decoder reuse one allocation instead of zeroing a fresh one per call.
pub fn decompress_into(blocks: &[&[u8]], out: &mut [u8], opts: &Opts) -> Result<(), Error> {
    decompress_into_with(blocks, out, opts, available_workers())
}

/// Allocating convenience wrapper around [`decompress_into`].
pub fn decompress(blocks: &[&[u8]], original_len: usize, opts: &Opts) -> Result<Vec<u8>, Error> {
    let mut out = vec![0u8; original_len];
    decompress_into(blocks, &mut out, opts)?;
    Ok(out)
}

/// [`decompress_into`] with an explicit core count.
pub fn decompress_into_with(
    blocks: &[&[u8]],
    out: &mut [u8],
    opts: &Opts,
    avail: usize,
) -> Result<(), Error> {
    let plan = plan_with(opts, out.len(), avail);

    if blocks.len() != plan.blocks {
        return Err(Error::BlockCountMismatch {
            expected: plan.blocks,
            got: blocks.len(),
        });
    }

    // one linear pass over the output, split into per-block slices
    let mut jobs: Vec<(usize, &mut [u8])> = Vec::with_capacity(plan.blocks);
    let mut rest: &mut [u8] = out;
    for i in 0..plan.blocks {
        let len = plan.block_size.min(rest.len());
        let (head, tail) = rest.split_at_mut(len);
        jobs.push((i, head));
        rest = tail;
    }

    if plan.workers <= 1 || plan.blocks <= 1 {
        for (i, dst) in jobs {
            decode_block(blocks[i], dst)?;
        }
        return Ok(());
    }

    let queue = Mutex::new(jobs);
    let control = Mutex::new(Control::new(plan, plan.workers));
    std::thread::scope(|s| -> Result<(), Error> {
        let handles: Vec<_> = (0..plan.workers)
            .map(|id| {
                let (queue, control) = (&queue, &control);
                s.spawn(move || -> Result<(), Error> {
                    loop {
                        if id >= lock(control).k {
                            break;
                        }
                        let job = lock(queue).pop();
                        let Some((i, dst)) = job else { break };
                        let started = Instant::now();
                        let n = decode_block(blocks[i], dst)?;
                        lock(control).report(n, started.elapsed().as_secs_f64());
                    }
                    Ok(())
                })
            })
            .collect();
        for h in handles {
            h.join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic))?;
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(workers: Workers, block_size: usize) -> Opts {
        Opts {
            workers,
            block_size,
            memory_budget: DEFAULT_MEMORY_BUDGET,
            ..Opts::default()
        }
    }

    /// Text-like data so blocks actually contain matches.
    fn corpus(len: usize) -> Vec<u8> {
        let words = [
            &b"the "[..],
            b"quick ",
            b"brown ",
            b"fox ",
            b"lz4 ",
            b"block ",
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

    // ---------------------------------------------------------------- strategy

    #[test]
    fn plan_worker_count_is_small_host_aware() {
        // budget / cores, not this machine: give it enough memory to see the rule alone
        let o = Opts {
            workers: Workers::Auto,
            block_size: MAX_BLOCK_SIZE,
            memory_budget: 1 << 30,
            ..Opts::default()
        };
        let big = 512 << 20;
        // <= SMALL_HOST_CORES: use them all; above: avail - (1 + avail/8)
        for (avail, want) in [
            (1, 1),
            (2, 2),
            (3, 3),
            (4, 4),
            (5, 4), // the formula takes over: 5 - (1 + 0)
            (8, 6),
            (12, 10),
            (16, 13),
            (64, 55),
        ] {
            assert_eq!(plan_with(&o, big, avail).workers, want, "avail {avail}");
        }
    }

    #[test]
    fn plan_refuses_parallel_when_it_cannot_pay_off() {
        // one core
        assert_eq!(
            plan_with(&opts(Workers::Auto, MAX_BLOCK_SIZE), 1 << 30, 1).workers,
            1
        );
        // block too small (ratio loss dominates)
        assert_eq!(
            plan_with(&opts(Workers::Auto, 64 << 10), 1 << 30, 16).workers,
            1
        );
        assert_eq!(
            plan_with(&opts(Workers::Auto, MIN_PARALLEL_BLOCK), 1 << 30, 16).workers,
            13
        );
        // too few blocks
        let o = opts(Workers::Auto, MIN_PARALLEL_BLOCK);
        assert_eq!(
            plan_with(&o, 8 * MIN_PARALLEL_BLOCK, 16).workers,
            8,
            "8 jobs cap it"
        );
        assert_eq!(plan_with(&o, 8 * MIN_PARALLEL_BLOCK - 1, 16).workers, 1);
        // never more workers than blocks (with enough jobs to pass the gate)
        assert_eq!(plan_with(&o, 9 * MIN_PARALLEL_BLOCK, 16).workers, 9);
    }

    #[test]
    fn plan_honours_memory_budget_and_explicit_requests() {
        let o = Opts {
            workers: Workers::Auto,
            block_size: MAX_BLOCK_SIZE,
            memory_budget: 8 << 20,
            ..Opts::default()
        };
        assert_eq!(
            plan_with(&o, 1 << 30, 16).workers,
            1,
            "8 MiB budget fits one 4 MiB block"
        );
        let o = Opts {
            workers: Workers::Auto,
            block_size: MAX_BLOCK_SIZE,
            memory_budget: 64 << 20,
            ..Opts::default()
        };
        assert_eq!(
            plan_with(&o, 1 << 30, 16).workers,
            8,
            "64 MiB / (2 x 4 MiB)"
        );
        // -T# overrides the heuristics but not physics
        let o = opts(Workers::Fixed(6), MAX_BLOCK_SIZE);
        assert_eq!(
            plan_with(&o, 1 << 30, 1).workers,
            6,
            "explicit wins over core count"
        );
        assert_eq!(plan_with(&o, 6 * MAX_BLOCK_SIZE, 16).workers, 6);
        assert_eq!(
            plan_with(&o, MAX_BLOCK_SIZE, 16).workers,
            1,
            "one job, one worker"
        );
        assert_eq!(
            plan_with(&opts(Workers::Single, MAX_BLOCK_SIZE), 1 << 30, 16).workers,
            1
        );
    }

    #[test]
    fn control_shrinks_on_sustained_slowdown_only() {
        let plan = Plan {
            workers: 4,
            block_size: 4 << 10,
            blocks: 1024,
        };
        let mut c = Control::new(plan, 4);
        c.window = 1000;
        // first window sets the reference point
        assert_eq!(c.report(1000, 1.0), 4); // 1000 B/s
        assert_eq!(c.best, 1000.0);
        // +5%: inside the hysteresis band, no change and no new reference
        assert_eq!(c.report(1000, 0.95), 4);
        assert_eq!(c.best, 1000.0);
        // -30%: one worker dropped, then three quiet windows, then one more
        assert_eq!(c.report(1000, 1.35), 3);
        assert_eq!(c.report(1000, 1.35), 3, "cooldown");
        assert_eq!(c.report(1000, 1.35), 3, "cooldown");
        assert_eq!(c.report(1000, 1.35), 3, "cooldown");
        assert_eq!(c.report(1000, 1.35), 2, "shrink again after the cooldown");
        // never below one worker
        for _ in 0..40 {
            c.report(1000, 1.35);
        }
        assert_eq!(c.k, 1);
        // and never above the highest we ever spawned
        let mut c = Control::new(plan, 3);
        c.window = 1;
        for _ in 0..10 {
            c.report(1, 0.001); // huge throughput
        }
        assert_eq!(c.k, 3);
    }

    // ---------------------------------------------------------------- roundtrip

    fn decode(blocks: &[&[u8]], len: usize, o: &Opts, avail: usize) -> Result<Vec<u8>, Error> {
        let mut out = vec![0u8; len];
        decompress_into_with(blocks, &mut out, o, avail).map(|()| out)
    }

    fn roundtrip(len: usize, block_size: usize, workers: Workers) {
        let src = corpus(len);
        let o = opts(workers, block_size);
        let blocks = compress_with(&src, &o, 16).expect("compress");
        let expected = len.div_ceil(block_size);
        assert_eq!(blocks.len(), expected, "block count for {len} bytes");

        let refs: Vec<&[u8]> = blocks.iter().map(|b| b.as_slice()).collect();
        let back = decode(&refs, len, &o, 16).expect("decompress");
        assert_eq!(back, src, "roundtrip {len} bytes with {workers:?}");
    }

    #[test]
    fn roundtrip_across_sizes_and_worker_counts() {
        for len in [0, 1, 1000, 4095, 4096, 4097, 8192, 40_000] {
            for workers in [
                Workers::Single,
                Workers::Fixed(1),
                Workers::Fixed(2),
                Workers::Fixed(4),
                Workers::Fixed(16),
            ] {
                roundtrip(len, 4096, workers);
            }
        }
        // empty input has no blocks at all
        let blocks = compress(&[], &opts(Workers::Auto, 4096)).unwrap();
        assert!(blocks.is_empty());
        assert!(decompress(&[], 0, &opts(Workers::Auto, 4096))
            .unwrap()
            .is_empty());
    }

    /// The whole reason for the design: the byte stream must not depend on the pool.
    #[test]
    fn output_is_independent_of_worker_count() {
        let src = corpus(300_000);
        let mut reference: Option<Vec<Vec<u8>>> = None;
        for workers in [
            Workers::Single,
            Workers::Fixed(1),
            Workers::Fixed(2),
            Workers::Fixed(3),
            Workers::Fixed(8),
            Workers::Auto,
        ] {
            let blocks = compress_with(&src, &opts(workers, 16 << 10), 16).unwrap();
            match &reference {
                None => reference = Some(blocks),
                Some(r) => assert_eq!(&blocks, r, "{workers:?} changed the output"),
            }
        }
    }

    /// The strategy must degrade to one thread on small hosts, and say so.
    #[test]
    fn auto_degrades_on_small_hosts() {
        let src = corpus(40_000);
        let o = opts(Workers::Auto, 4096); // 4 KiB blocks: below the 1 MiB parallel floor
        for avail in [1, 2, 4, 16] {
            assert_eq!(plan_with(&o, src.len(), avail).workers, 1, "avail {avail}");
            let blocks = compress_with(&src, &o, avail).unwrap();
            let refs: Vec<&[u8]> = blocks.iter().map(|b| b.as_slice()).collect();
            assert_eq!(decode(&refs, src.len(), &o, avail).unwrap(), src);
        }

        // with a real block size the same input is worth parallelising, but not with one core
        let o = opts(Workers::Auto, MIN_PARALLEL_BLOCK);
        let src = corpus(16 * MIN_PARALLEL_BLOCK);
        assert_eq!(plan_with(&o, src.len(), 1).workers, 1);
        assert!(plan_with(&o, src.len(), 8).workers > 1);
    }

    /// `Auto` must produce exactly the same bytes as `Single` — this is what lets a CLI
    /// change the thread count without changing the output.
    #[test]
    fn auto_matches_single() {
        let src = corpus(3 * MIN_PARALLEL_BLOCK + 1234);
        let auto = par_opts(Workers::Auto);
        let single = par_opts(Workers::Single);
        let a = compress(&src, &auto).unwrap();
        let s = compress(&src, &single).unwrap();
        assert_eq!(a, s);
        let refs: Vec<&[u8]> = a.iter().map(|b| b.as_slice()).collect();
        assert_eq!(decompress(&refs, src.len(), &auto).unwrap(), src);
    }

    fn par_opts(workers: Workers) -> Opts {
        Opts {
            workers,
            block_size: MIN_PARALLEL_BLOCK,
            ..Opts::default()
        }
    }

    /// The streaming form: blocks arrive in input order, exactly once, and concatenate to
    /// the same bytes the collecting form returns.
    #[test]
    fn compress_each_delivers_in_order_exactly_once() {
        let src = corpus(120_000);
        let o = opts(Workers::Fixed(4), 4096);
        let mut seen: Vec<(usize, Vec<u8>)> = Vec::new();
        compress_each_with(&src, &o, 8, |i, bytes| {
            seen.push((i, bytes));
            Ok::<(), Error>(())
        })
        .unwrap();

        let expected = compress_with(&src, &o, 8).unwrap();
        let indices: Vec<usize> = seen.iter().map(|(i, _)| *i).collect();
        assert_eq!(indices, (0..expected.len()).collect::<Vec<_>>(), "order");
        let streamed: Vec<Vec<u8>> = seen.into_iter().map(|(_, b)| b).collect();
        assert_eq!(streamed, expected);

        // empty input: no blocks, no calls
        let mut calls = 0;
        compress_each(&[], &o, |_, _| {
            calls += 1;
            Ok::<(), Error>(())
        })
        .unwrap();
        assert_eq!(calls, 0);
    }

    /// A caller with its own error type (the frame writer will have one for I/O) keeps it:
    /// codec errors convert in, and the sink's error comes back out unchanged.
    #[test]
    fn compress_each_propagates_the_sink_error_without_hanging() {
        #[derive(Debug, PartialEq)]
        enum Sink {
            Codec(Error),
            Stop(usize),
        }
        impl From<Error> for Sink {
            fn from(e: Error) -> Self {
                Sink::Codec(e)
            }
        }

        let src = corpus(200_000);
        let o = opts(Workers::Fixed(4), 4096);
        let got = compress_each_with(&src, &o, 8, |i, _bytes| {
            if i == 2 {
                Err(Sink::Stop(i))
            } else {
                Ok(())
            }
        });
        assert_eq!(got, Err(Sink::Stop(2)));

        // codec errors reach the caller through the same type (all twelve levels are
        // ported now, so the conversion is checked directly instead of via a driver error)
        assert_eq!(Sink::from(Error::Truncated), Sink::Codec(Error::Truncated));
    }

    /// End to end: the level is honoured by the pool, and levels above 1 report the gap
    /// rather than silently compressing at level 1.
    #[test]
    fn level_flows_through_the_pool() {
        let src = corpus(40_000);
        let fast = opts(Workers::Fixed(2), 4096);
        let ours = compress_with(&src, &fast, 4).unwrap();
        let mut one = vec![0u8; block::compress_bound(4096)];
        let mut expected = Vec::new();
        for r in src.chunks(4096) {
            let n = block::compress_into(r, &mut one, 1).unwrap();
            expected.push(one[..n].to_vec());
        }
        assert_eq!(ours, expected, "level 1 must be the C default parser");

        // all twelve levels are ported: the pool must now accept every one of them
        for n in 1..=12u8 {
            let o = Opts {
                level: Level::new(n).unwrap(),
                ..opts(Workers::Fixed(2), 4096)
            };
            let blocks = compress_with(&src, &o, 4).unwrap();
            let refs: Vec<&[u8]> = blocks.iter().map(|b| b.as_slice()).collect();
            assert_eq!(decode(&refs, src.len(), &o, 4).unwrap(), src, "level {n}");
        }
    }

    #[test]
    fn more_workers_than_blocks_is_fine() {
        let src = corpus(10_000);
        let o = opts(Workers::Fixed(16), 4096); // 3 blocks, 16 workers
        let blocks = compress_with(&src, &o, 16).unwrap();
        assert_eq!(blocks.len(), 3);
        let refs: Vec<&[u8]> = blocks.iter().map(|b| b.as_slice()).collect();
        assert_eq!(
            {
                let mut b = vec![0u8; 10_000];
                decompress_into_with(&refs, &mut b, &o, 16).unwrap();
                b
            },
            src
        );
    }

    /// Regression: a block that decodes to *fewer* bytes than its slot used to be
    /// accepted, leaving stale bytes in the caller's buffer and returning `Ok`.
    #[test]
    fn short_block_is_rejected() {
        let src = corpus(9000);
        let o = opts(Workers::Fixed(2), 4096);
        let blocks = compress_with(&src, &o, 16).unwrap();
        let mut short = vec![0u8; block::compress_bound(20)];
        let n = block::compress_into(&src[..20], &mut short, 1).unwrap();
        let mut broken: Vec<Vec<u8>> = blocks.clone();
        broken[2] = short[..n].to_vec(); // last slot wants 808 bytes
        let refs: Vec<&[u8]> = broken.iter().map(|b| b.as_slice()).collect();
        assert!(matches!(
            decode(&refs, 9000, &o, 16),
            Err(Error::BlockSizeMismatch {
                expected: 808,
                got: 20
            })
        ));
        // and through the single-threaded path too: same helper, same answer
        assert!(matches!(
            decode(&refs, 9000, &opts(Workers::Single, 4096), 16),
            Err(Error::BlockSizeMismatch {
                expected: 808,
                got: 20
            })
        ));
    }

    #[test]
    fn corrupt_block_is_an_error_not_a_panic() {
        let src = corpus(40_000);
        let o = opts(Workers::Fixed(4), 4096);
        let blocks = compress_with(&src, &o, 16).unwrap();
        let mut broken: Vec<Vec<u8>> = blocks.clone();
        broken[3].truncate(3);
        let refs: Vec<&[u8]> = broken.iter().map(|b| b.as_slice()).collect();
        assert!(matches!(
            decode(&refs, 40_000, &o, 16),
            Err(Error::Truncated | Error::InvalidOffset(_) | Error::OutputTooSmall { .. })
        ));
    }

    #[test]
    fn wrong_block_count_and_wrong_length_are_errors() {
        let src = corpus(9000);
        let o = opts(Workers::Fixed(2), 4096);
        let blocks = compress_with(&src, &o, 16).unwrap();
        let refs: Vec<&[u8]> = blocks.iter().map(|b| b.as_slice()).collect();
        // one block missing
        assert!(matches!(
            decode(&refs[..1], 9000, &o, 16),
            Err(Error::BlockCountMismatch {
                expected: 3,
                got: 1
            })
        ));
        // claiming a shorter original than the blocks decode to
        assert!(decode(&refs, 4096, &o, 16).is_err());
    }

    #[test]
    fn block_sizes_are_clamped_to_the_frame_format() {
        let o = Opts {
            workers: Workers::Auto,
            block_size: 1 << 30,
            memory_budget: DEFAULT_MEMORY_BUDGET,
            ..Opts::default()
        };
        assert_eq!(plan_with(&o, 1 << 30, 16).block_size, MAX_BLOCK_SIZE);
        let o = Opts {
            workers: Workers::Auto,
            block_size: 0,
            memory_budget: DEFAULT_MEMORY_BUDGET,
            ..Opts::default()
        };
        assert_eq!(plan_with(&o, 10, 16).block_size, 1);
    }
}
