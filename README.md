# kobold-bench

<img src="assets/kobold_data_shim.png" width="200">

**Benchmark harness for the [`gnucobol-rs`](https://github.com/infinityabundance/gnucobol-rs) hot
path** — the `cob_move` DISPLAY ↔ COMP-3 conversions that dominate legacy-file ingestion — with a
**parity re-check after every run**. The doctrine: *performance work never alters sealed semantics.*
A throughput number is never reported without re-confirming the byte-exact result.

## Baseline (correctness mode — no SIMD, no parallelism)

Measured by `kobold-throughput` on one developer machine (x86-64, single thread, `--release`,
`overflow-checks = true`). Your numbers will differ; reproduce with `cargo run --release --bin
kobold-throughput -- 5000000`.

| Conversion | Throughput | Bytes | Parity |
|------------|-----------:|------:|:------:|
| DISPLAY `S9(7)V99` → COMP-3 | **~95 M records/sec** | ~860 MB/sec (source) | re-checked **byte-exact** |

For scale intuition: a 100-million-record nightly batch with this single field is **~1 second** of
CPU for the decimal conversion, single-threaded — before any `parallel`/SIMD feature. The kernel is
allocation-free per conversion (fixed stack buffers), which is what makes it Lambda/Glue-friendly.

> These are **baseline** numbers on purpose. The point is a *provably correct* primitive that is
> already fast; optional acceleration is gravy, gated, and re-proven.

## Run it

```sh
cargo run --release --bin kobold-throughput -- 10000000   # quick throughput probe + parity re-check
cargo bench                                                # criterion micro-benchmarks (per-direction)
```

`cargo bench` runs `benches/cob_move.rs` (Criterion) for `display_to_packed` and `packed_to_display`
with `Throughput::Elements`, reporting time/conversion and elements/sec.

## Methodology

- **Synthetic, reproducible batches** — a deterministic LCG (`gen_display_batch`) so the input is
  identical across machines and runs; mixed signs (overpunch) to exercise the sign path.
- **Parity re-check is mandatory** — `parity_holds` round-trips a sample (DISPLAY → COMP-3 →
  DISPLAY) and asserts the decoded value is unchanged. `kobold-throughput` exits non-zero if it
  fails. No number ships without this.
- **Honest accounting** — single-threaded, one field; multi-field records and end-to-end shim decode
  (with copybook layout) are heavier. This harness measures the *kernel conversion*, the part that
  must be both correct and fast.

## Gated acceleration — the plan (not yet implemented, never default)

Performance features must be **strictly optional, gated, and semantics-preserving**. The intended
shape (tracked, with each path re-running the full `gnucobol-rs` differential sweep + Kani suite in
CI before it can be claimed):

- **`parallel` (Rayon).** Batch-level `par_iter` over independent records — embarrassingly parallel,
  near-linear on the 8–64 vCPU instances common on AWS. No change to per-record bytes.
- **`simd`.** Vectorized nibble pack/unpack and overpunch handling for the COMP-3 inner loop, in an
  isolated `unsafe` module **with a scalar fallback** and runtime CPU-feature detection. The default
  build stays `#![forbid(unsafe_code)]`.

Each, if added, is labelled **"accelerated (feature-enabled)"** vs the **"baseline (correctness
mode)"** here, reported in `compat_profile`, and listed as **not part of the sealed courts**.

## AWS cost framing

Lambda/Glue bill on duration. A correct-and-fast kernel turns a decimal-heavy batch from a
multi-hour job into minutes, cutting compute spend and shortening reconciliation windows. Pair these
numbers with the [`kobold-data-shim`](https://github.com/infinityabundance/kobold-data-shim) parity
receipts and the [`kobold-lambda-layer`](https://github.com/infinityabundance/kobold-lambda-layer)
packaging for a full S3 → verified-records reference architecture.

## License

Apache-2.0 (`LICENSE`). Links `gnucobol-rs` (LGPL-3.0-or-later) — see [`NOTICE`](NOTICE) for the
binary-distribution obligations.
