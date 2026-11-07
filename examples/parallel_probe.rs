//! Throughput of the `par` module at several worker counts (docs/DESIGN.md §9).
//!
//!   cargo run --release --example parallel_probe -- <file> [rounds]
//!
//! Reports compression/decompression MiB/s for `Single`, `Fixed(2|4|8)` and `Auto`
//! (which uses the adaptive controller), plus the output-size cost of splitting.
//!
//! Every figure is the **minimum** over the rounds, not the average: this machine's clock
//! drifts by up to 3x between runs, and only the minimum is comparable across runs
//! (docs/DESIGN.md §5). It also times the streaming `compress_each`, which is what the
//! frame encoder actually calls.

use std::time::Instant;

use lz4_in_rust::par::{self, Opts, Workers, MAX_BLOCK_SIZE};

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: parallel_probe <file> [rounds]");
    let rounds: usize = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(3);
    let data = std::fs::read(&path).expect("read input");
    let mib = data.len() as f64 / (1024.0 * 1024.0);

    // single-block reference (the block API, no frames)
    let mut one = vec![0u8; lz4_in_rust::block::compress_bound(data.len())];
    let mut back = vec![0u8; data.len()];
    let csize = lz4_in_rust::block::compress_into(&data, &mut one, 1).unwrap();
    let mut base_c = f64::MAX;
    for _ in 0..rounds {
        let t = Instant::now();
        std::hint::black_box(lz4_in_rust::block::compress_into(&data, &mut one, 1).unwrap());
        base_c = base_c.min(t.elapsed().as_secs_f64());
    }
    let mut base_d = f64::MAX;
    for _ in 0..rounds {
        let t = Instant::now();
        std::hint::black_box(
            lz4_in_rust::block::decompress_into(&one[..csize], &mut back).unwrap(),
        );
        base_d = base_d.min(t.elapsed().as_secs_f64());
    }
    println!("{path}: {mib:.1} MiB, {rounds} rounds");
    println!(
        "  block API, 1 thread : compress {:6.0} MiB/s | decompress {:6.0} MiB/s | {} B",
        mib / base_c,
        mib / base_d,
        csize
    );

    for workers in [
        Workers::Single,
        Workers::Fixed(2),
        Workers::Fixed(4),
        Workers::Fixed(8),
        Workers::Auto,
    ] {
        let o = Opts {
            workers,
            block_size: MAX_BLOCK_SIZE,
            ..Opts::default()
        };
        let plan = par::plan(&o, data.len());
        let blocks = par::compress(&data, &o).unwrap();
        let refs: Vec<&[u8]> = blocks.iter().map(|b| b.as_slice()).collect();

        let mut c = f64::MAX;
        for _ in 0..rounds {
            let t = Instant::now();
            std::hint::black_box(par::compress(&data, &o).unwrap());
            c = c.min(t.elapsed().as_secs_f64());
        }
        let mut decoded = vec![0u8; data.len()];
        let mut d = f64::MAX;
        for _ in 0..rounds {
            let t = Instant::now();
            par::decompress_into(&refs, &mut decoded, &o).unwrap();
            std::hint::black_box(&decoded);
            d = d.min(t.elapsed().as_secs_f64());
        }
        let total: usize = blocks.iter().map(|b| b.len()).sum();
        println!(
            "  {:14?} ({:2} jobs, {:2} workers): compress {:6.0} MiB/s ({:.2}x) | decompress {:6.0} MiB/s ({:.2}x) | {total} B ({:+.2}%)",
            workers,
            blocks.len(),
            plan.workers,
            mib / c,
            base_c / c,
            mib / d,
            base_d / d,
            100.0 * (total as f64 - csize as f64) / csize as f64
        );
    }

    // The frame encoder's path: blocks streamed to a sink, never collected.
    let o = Opts {
        workers: Workers::Auto,
        block_size: MAX_BLOCK_SIZE,
        ..Opts::default()
    };
    let mut streamed = 0usize;
    let mut best = f64::MAX;
    for _ in 0..rounds {
        streamed = 0;
        let t = Instant::now();
        par::compress_each(&data, &o, |_, bytes| {
            streamed += bytes.len();
            Ok::<(), lz4_in_rust::Error>(())
        })
        .unwrap();
        best = best.min(t.elapsed().as_secs_f64());
    }
    println!(
        "  compress_each (Auto, streaming): compress {:6.0} MiB/s | {streamed} B",
        mib / best
    );
}
