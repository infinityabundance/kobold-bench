//! Criterion micro-benchmarks for the gnucobol-rs hot path. Baseline (correctness mode) — no SIMD,
//! no parallelism. These measure the per-conversion cost of `cob_move` for the dominant ingestion
//! direction (DISPLAY → COMP-3) and the reverse (COMP-3 → DISPLAY).

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use gnucobol_rs::cob_move;
use kobold_bench::{gen_display_batch, money_display, money_packed};

fn bench_move(c: &mut Criterion) {
    let batch = gen_display_batch(100_000, 7);
    let (sd, sp) = (money_display(), money_packed());

    // Pre-encode a packed batch for the reverse direction.
    let packed: Vec<[u8; 5]> = batch
        .iter()
        .map(|src| {
            let mut d = [0u8; 5];
            let _ = cob_move(src, &sd, &mut d, &sp);
            d
        })
        .collect();

    let mut g = c.benchmark_group("cob_move");
    g.throughput(Throughput::Elements(batch.len() as u64));

    g.bench_function("display_to_packed", |b| {
        let mut dst = [0u8; 5];
        b.iter(|| {
            for src in black_box(&batch) {
                let _ = cob_move(src, &sd, &mut dst, &sp);
            }
        });
    });

    g.bench_function("packed_to_display", |b| {
        let mut dst = [0u8; 9];
        b.iter(|| {
            for src in black_box(&packed) {
                let _ = cob_move(src, &sp, &mut dst, &sd);
            }
        });
    });

    g.finish();
}

criterion_group!(benches, bench_move);
criterion_main!(benches);
