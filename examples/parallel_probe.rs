//! Throughput of the `par` module at several worker counts (docs/DESIGN.md §9).
//!
//!   cargo run --release --example parallel_probe -- <file> [rounds]
//!
//! Reports compression/decompression MiB/s for `Single`, `Fixed(2|4|8)` and `Auto`
//! (which uses the adaptive controller), plus the output-size cost of splitting.

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
    let t = Instant::now();
    for _ in 0..rounds {
        std::hint::black_box(lz4_in_rust::block::compress_into(&data, &mut one, 1).unwrap());
    }
    let base_c = t.elapsed().as_secs_f64() / rounds as f64;
    let t = Instant::now();
    for _ in 0..rounds {
        std::hint::black_box(
            lz4_in_rust::block::decompress_into(&one[..csize], &mut back).unwrap(),
        );
    }
    let base_d = t.elapsed().as_secs_f64() / rounds as f64;
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

        let t = Instant::now();
        for _ in 0..rounds {
            std::hint::black_box(par::compress(&data, &o).unwrap());
        }
        let c = t.elapsed().as_secs_f64() / rounds as f64;
        let mut decoded = vec![0u8; data.len()];
        let t = Instant::now();
        for _ in 0..rounds {
            par::decompress_into(&refs, &mut decoded, &o).unwrap();
            std::hint::black_box(&decoded);
        }
        let d = t.elapsed().as_secs_f64() / rounds as f64;
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
}
