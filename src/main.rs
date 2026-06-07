//! `kobold-throughput` — a quick, dependency-free throughput probe for the gnucobol-rs hot path
//! (DISPLAY S9(7)V99 → COMP-3), printing records/sec and MB/sec, then **re-checking parity** so a
//! number is never reported without re-confirming byte-exactness. Wall-clock comes from the OS.

use kobold_bench::{
    convert_display_to_packed, gen_display_batch, money_display, money_packed, parity_holds,
};
use std::time::Instant;

fn main() {
    let n: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(10_000_000);

    let batch = gen_display_batch(n, 0x1234_5678);
    let (sd, sp) = (money_display(), money_packed());

    // Warm up, then measure a few passes for a stable number.
    convert_display_to_packed(&batch, &sp, &sd);
    let passes = 5;
    let start = Instant::now();
    let mut done = 0usize;
    for _ in 0..passes {
        done += convert_display_to_packed(&batch, &sp, &sd);
    }
    let elapsed = start.elapsed().as_secs_f64();

    let recs_per_sec = done as f64 / elapsed;
    let bytes = done * 9; // source bytes processed
    let mb_per_sec = bytes as f64 / elapsed / 1.0e6;

    // Parity re-check — required before reporting.
    let ok = parity_holds(&batch);

    println!("kobold-throughput: DISPLAY S9(7)V99 -> COMP-3");
    println!("  records            : {n} x {passes} passes = {done}");
    println!("  wall               : {elapsed:.3} s");
    println!(
        "  throughput         : {:.1} M records/sec",
        recs_per_sec / 1.0e6
    );
    println!("  throughput (bytes) : {mb_per_sec:.0} MB/sec (source)");
    println!(
        "  parity re-check    : {}",
        if ok { "PASS (byte-exact)" } else { "FAIL" }
    );
    if !ok {
        std::process::exit(1);
    }
}
