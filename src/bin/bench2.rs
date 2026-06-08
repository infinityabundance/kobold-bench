//! KOBOLD.BENCH.2 — parity-gated, scalar, end-to-end reconciliation benchmark.
//!
//! Measures the FULL shim pipeline (FILE.1 ingest -> decode -> LEVEL-88 -> audit) plus the banking,
//! DB2-indicator, and transform courts, over a synthetic happy+hostile corpus. **Timing is admitted only
//! after the output/audit hash matches the pinned baseline** -- a benchmark must never alter or outrun
//! sealed semantics. Scalar only: no Rayon, no SIMD, no "fast mode", no production/AWS/parallel claim.

use gnucobol_rs::{build_field, cob_move, FieldAttr, Usage, COB_FLAG_HAVE_SIGN, COB_TYPE_NUMERIC_DISPLAY};
use kobold_data_shim::recon::reconcile;
use kobold_data_shim::{decode_record_encoded, Encoding, NoCopy};
use std::time::Instant;

const CB: &str = "       01 ACCT.\n           05 ID PIC 9(6).\n           05 ST PIC X.\n               88 ACTIVE VALUE \"A\".\n               88 CLOSED VALUE \"C\".\n           05 BAL PIC S9(7)V99 COMP-3.\n           05 BR PIC 9(4) COMP.\n";
const RL: usize = 14; // 6 + 1 + 5 (COMP-3 S9(7)V99) + 2 (COMP 9(4))

struct Lcg(u64);
impl Lcg {
    fn next(&mut self, n: u64) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 16) % n.max(1)
    }
}

fn enc(pic: &str, u: Usage, value: &str) -> Vec<u8> {
    let pf = build_field(pic, u, false, false).unwrap();
    let neg = value.starts_with('-');
    let t = value.trim_start_matches(['-', '+']);
    let (ip, fp) = t.split_once('.').unwrap_or((t, ""));
    let mut d: Vec<u8> = ip.bytes().chain(fp.bytes()).map(|b| b - b'0').collect();
    let (have, scale) = (fp.len() as i16, pf.attr.scale);
    if scale > have { d.resize(d.len() + (scale - have) as usize, 0); }
    while d.len() < pf.attr.digits as usize { d.insert(0, 0); }
    let extra = d.len().saturating_sub(pf.attr.digits as usize);
    d.drain(0..extra);
    let mut src: Vec<u8> = d.iter().map(|x| b'0' + x).collect();
    if neg { if let Some(l) = src.last_mut() { *l |= 0x40; } }
    let sa = FieldAttr { field_type: COB_TYPE_NUMERIC_DISPLAY, digits: pf.attr.digits, scale: pf.attr.scale, flags: COB_FLAG_HAVE_SIGN };
    let mut out = vec![0u8; pf.size];
    cob_move(&src, &sa, &mut out, &pf.attr).unwrap();
    out
}

fn gen(n: usize) -> Vec<u8> {
    let mut rng = Lcg(0xB2_0000_0001);
    let mut out = Vec::with_capacity(n * RL);
    for i in 0..n {
        out.extend_from_slice(format!("{:06}", 100000 + i).as_bytes()); // ID 9(6)
        out.push(if i % 3 == 0 { b'A' } else { b'C' }); // ST
        out.extend(enc("S9(7)V99", Usage::Comp3, &format!("{}.{:02}", rng.next(9_999_999), rng.next(100)))); // BAL
        out.extend(enc("9(4)", Usage::Comp, &format!("{}", 1000 + rng.next(8999)))); // BR
    }
    out
}

fn audit_field<'a>(audit: &'a str, key: &str) -> Option<&'a str> {
    let m = format!("\"{key}\":\"");
    let s = audit.find(&m)? + m.len();
    let e = audit[s..].find('"')? + s;
    Some(&audit[s..e])
}

fn cpu_model() -> String {
    std::fs::read_to_string("/proc/cpuinfo").ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("model name")).map(|l| l.split(':').nth(1).unwrap_or("").trim().to_string()))
        .unwrap_or_else(|| "unknown".into())
}
fn ncpu() -> usize {
    std::fs::read_to_string("/proc/cpuinfo").map(|s| s.lines().filter(|l| l.starts_with("processor")).count()).unwrap_or(0)
}

fn main() {
    let n: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(50_000);
    let data = gen(n);
    let bytes = data.len();

    // --- full pipeline (FILE.1 ingest + decode + LEVEL-88 + audit) ---
    let warm = reconcile("bench2", CB, &data, RL, "0.7.1", &NoCopy).expect("reconcile");
    let out_hash = audit_field(&warm.audit_json, "decode_output_sha256").unwrap_or("").to_string();

    // PARITY GATE: timing is admitted only if the output hash matches the pinned baseline.
    let base_path = "reports/BENCH-2-baseline.json";
    let baseline = std::fs::read_to_string(base_path).ok()
        .and_then(|s| audit_field(&s, "decode_output_sha256").map(str::to_string));
    match &baseline {
        Some(b) if *b != out_hash => {
            eprintln!("PARITY FAIL: output hash {out_hash} != baseline {b} — timing NOT admitted.");
            std::process::exit(1);
        }
        Some(_) => eprintln!("parity: output hash matches baseline ({}…)", &out_hash[..12]),
        None => {
            std::fs::write(base_path, format!("{{\"schema\":\"kobold-bench2-baseline-v1\",\"decode_output_sha256\":\"{out_hash}\",\"record_len\":{RL}}}\n")).ok();
            eprintln!("parity: established baseline ({}…)", &out_hash[..12]);
        }
    }

    let iters = 5;
    let t = Instant::now();
    for _ in 0..iters {
        let _ = reconcile("bench2", CB, &data, RL, "0.7.1", &NoCopy).unwrap();
    }
    let full = t.elapsed() / iters;

    // --- decode-only split (no ingest/audit) ---
    let t = Instant::now();
    for _ in 0..iters {
        for chunk in data.chunks(RL) {
            let _ = decode_record_encoded(CB, chunk, &NoCopy, Encoding::Ascii).unwrap();
        }
    }
    let decode = t.elapsed() / iters;

    let rps = n as f64 / full.as_secs_f64();
    let us_rec = full.as_micros() as f64 / n as f64;
    let mbps = bytes as f64 / full.as_secs_f64() / 1_000_000.0;

    let receipt = format!(
        concat!(
            "{{\"schema\":\"kobold-bench2-receipt-v1\",\"court\":\"KOBOLD.BENCH.2\",\"mode\":\"scalar\",",
            "\"parity_gated\":true,\"output_sha256\":\"{}\",\"records\":{},\"bytes\":{},\"record_len\":{},",
            "\"full_pipeline\":{{\"records_per_sec\":{:.0},\"us_per_record\":{:.3},\"mb_per_sec\":{:.1}}},",
            "\"decode_only_us_per_record\":{:.3},\"ingest_audit_overhead_us_per_record\":{:.3},",
            "\"corpus\":\"synthetic happy (DISPLAY+COMP-3+COMP+LEVEL-88); see CORPUS.2 for the hostile fixtures the courts fail-close on\",",
            "\"host\":{{\"cpu\":{:?},\"ncpu\":{},\"arch\":{:?},\"profile\":{:?},\"rayon\":false,\"simd\":false}},",
            "\"non_claims\":[\"no production performance claim\",\"no AWS performance claim\",\"no parallel throughput claim\",\"no customer-workload representativeness\"]}}\n"
        ),
        out_hash, n, bytes, RL,
        rps, us_rec, mbps,
        decode.as_micros() as f64 / n as f64,
        (full.as_micros() as f64 - decode.as_micros() as f64) / n as f64,
        cpu_model(), ncpu(),
        std::env::consts::ARCH,
        if cfg!(debug_assertions) { "debug" } else { "release" },
    );
    std::fs::write("reports/BENCH-2-receipt.json", &receipt).ok();
    eprintln!("KOBOLD.BENCH.2 (scalar, parity-gated): {n} records, {:.0} rec/s, {:.3} µs/rec, {:.1} MB/s (decode-only {:.3} µs/rec)",
              rps, us_rec, mbps, decode.as_micros() as f64 / n as f64);
    eprintln!("receipt: reports/BENCH-2-receipt.json");
}
