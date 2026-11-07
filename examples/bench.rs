//! Rough throughput check against the C reference:
//!
//! ```text
//! cargo run --release --example bench <file>
//! lz4 -b1 <file>          # same machine, same data, block-mode benchmark
//! ```
//!
//! Not a microbenchmark harness: 30 passes, minimum reported, which is enough to catch a
//! port that is 3x off. The minimum is the stable statistic on a machine whose clock
//! drifts by up to 3x; a mean would just measure the fan.

use std::time::Instant;

fn main() {
    let path = std::env::args().nth(1).expect("usage: bench <file>");
    let data = std::fs::read(&path).expect("read input");
    let mut packed = vec![0u8; lz4_in_rust::block::compress_bound(data.len())];
    let mut out = vec![0u8; data.len()];

    let mut best_c = f64::MAX;
    let mut csize = 0;
    for _ in 0..30 {
        let t = Instant::now();
        csize = lz4_in_rust::block::compress_into(&data, &mut packed, 1).unwrap();
        best_c = best_c.min(t.elapsed().as_secs_f64());
    }

    let mut best_d = f64::MAX;
    for _ in 0..30 {
        let t = Instant::now();
        let n = lz4_in_rust::block::decompress_into(&packed[..csize], &mut out).unwrap();
        assert_eq!(n, data.len());
        best_d = best_d.min(t.elapsed().as_secs_f64());
    }

    let mib = data.len() as f64 / (1024.0 * 1024.0);
    println!(
        "{path}: {} bytes -> {} bytes ({:.1}%), compress {:.0} MiB/s, decompress {:.0} MiB/s",
        data.len(),
        csize,
        100.0 * csize as f64 / data.len() as f64,
        mib / best_c,
        mib / best_d
    );
}
