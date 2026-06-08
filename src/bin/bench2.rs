//! KOBOLD.BENCH.2 — parity-gated, scalar, end-to-end reconciliation benchmark.
//!
//! Measures the FULL shim pipeline (FILE.1 ingest -> decode -> LEVEL-88 -> audit) plus the banking,
//! DB2-indicator, and transform courts, over a synthetic happy+hostile corpus. **Timing is admitted only
//! after the output/audit hash matches the pinned baseline** -- a benchmark must never alter or outrun
//! sealed semantics. Scalar only: no Rayon, no SIMD, no "fast mode", no production/AWS/parallel claim.

use gnucobol_rs::{build_field, cob_move, FieldAttr, Usage, COB_FLAG_HAVE_SIGN, COB_TYPE_NUMERIC_DISPLAY};
use kobold_data_shim::recon::reconcile;
use kobold_data_shim::{
    decode_record_encoded, extract_manifest, posting_manifest, Encoding, ExtractMethod, ExtractProfile,
    FileOrganization, NoCopy, PostingProfile, RecordLengthSource,
};
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

    // The parity baseline is over a FIXED reference corpus (independent of the timing N), so the gate is
    // stable no matter how many records you bench. Timing then runs over `data` = gen(n).
    const REF_N: usize = 1000;
    let ref_data = gen(REF_N);
    let warm = reconcile("bench2", CB, &ref_data, RL, "0.7.1", &NoCopy).expect("reconcile");
    let out_hash = audit_field(&warm.audit_json, "decode_output_sha256").unwrap_or("").to_string();

    // PARITY GATE: timing is admitted only if the reference output hash matches the pinned baseline.
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

    // --- custody spine over the same corpus (POSTING.1 + EXTRACT.PROFILE.1) ---
    let prof = PostingProfile {
        posting_unit_id: "bench2", business_date: "2026-06-08", extract_time_utc: "2026-06-08T00:00:00Z",
        source_system: "synthetic", sequence_field: Some("ID"), sequence_contiguous: false, txn_id_field: None,
    };
    let xp = ExtractProfile {
        source_file_organization: FileOrganization::Sequential, extract_method: ExtractMethod::UnloadedFixedRecord,
        record_length_source: RecordLengthSource::Copybook, copybook_source: "synthetic",
        code_set_conversion_before_kobold: None, source_system_cutoff: None, business_date: Some("2026-06-08"),
        operator_declared_assumptions: &["fixed 14-byte records"],
    };
    let t = Instant::now();
    for _ in 0..iters {
        let _ = posting_manifest(CB, &data, RL, &prof, &NoCopy, Encoding::Ascii).unwrap();
        let _ = extract_manifest(CB, &data, &xp);
    }
    let custody = t.elapsed() / iters;

    // --- KOBOLD.PERF.2: per-stage profiling (parse / per-record / aggregate) over the reference corpus ---
    let prof = {
        use kobold_data_shim::recon::reconcile_profile;
        let mut acc = (0u128, 0u128, 0u128);
        for _ in 0..iters {
            let (_, p) = reconcile_profile("bench2", CB, &ref_data, RL, "0.7.1", &NoCopy, Encoding::Ascii).unwrap();
            acc = (acc.0 + p.parse_ns, acc.1 + p.record_ns, acc.2 + p.aggregate_ns);
        }
        (acc.0 / iters as u128, acc.1 / iters as u128, acc.2 / iters as u128)
    };
    let stage_total = (prof.0 + prof.1 + prof.2).max(1) as f64;
    let pct = |x: u128| 100.0 * x as f64 / stage_total;
    let bottleneck = if prof.1 >= prof.0 && prof.1 >= prof.2 { "per_record" }
        else if prof.2 >= prof.0 { "aggregate" } else { "parse" };

    // --- KOBOLD.PERF.1: record-level Rayon, ADMITTED ONLY if byte-identical to the scalar baseline ---
    #[cfg(feature = "rayon")]
    let (rayon_rps, rayon_us, speedup): (f64, f64, f64) = {
        use kobold_data_shim::recon::reconcile_encoded_parallel;
        let warm = reconcile_encoded_parallel("bench2", CB, &ref_data, RL, "0.7.1", &NoCopy, Encoding::Ascii).unwrap();
        let rhash = audit_field(&warm.audit_json, "decode_output_sha256").unwrap_or("");
        if rhash != out_hash {
            eprintln!("RAYON PARITY FAIL: {rhash} != scalar {out_hash} — refusing Rayon timing.");
            std::process::exit(1);
        }
        eprintln!("perf1 parity: rayon output hash == scalar baseline ({}…)", &out_hash[..12]);
        let t = Instant::now();
        for _ in 0..iters {
            let _ = reconcile_encoded_parallel("bench2", CB, &data, RL, "0.7.1", &NoCopy, Encoding::Ascii).unwrap();
        }
        let rfull = t.elapsed() / iters;
        let rrps = n as f64 / rfull.as_secs_f64();
        (rrps, rfull.as_micros() as f64 / n as f64, rrps / rps)
    };
    #[cfg(not(feature = "rayon"))]
    let (rayon_rps, rayon_us, speedup): (f64, f64, f64) = (0.0, 0.0, 0.0);
    let rayon_on = cfg!(feature = "rayon");

    let receipt = format!(
        concat!(
            "{{\"schema\":\"kobold-bench2-receipt-v1\",\"court\":\"KOBOLD.BENCH.2 (+PERF.1)\",\"mode\":\"scalar\",",
            "\"parity_gated\":true,\"output_sha256\":\"{}\",\"records\":{},\"bytes\":{},\"record_len\":{},",
            "\"full_pipeline\":{{\"records_per_sec\":{:.0},\"us_per_record\":{:.3},\"mb_per_sec\":{:.1}}},",
            "\"decode_only_us_per_record\":{:.3},\"ingest_audit_overhead_us_per_record\":{:.3},",
            "\"custody_us_per_record\":{:.3},",
            "\"perf2_stage_profile\":{{\"parse_ns\":{},\"per_record_ns\":{},\"aggregate_ns\":{},\"per_record_pct\":{:.1},\"aggregate_pct\":{:.1},\"bottleneck\":{:?}}},",
            "\"perf1_rayon\":{{\"enabled\":{},\"parity_with_scalar\":{},\"records_per_sec\":{:.0},\"us_per_record\":{:.3},\"speedup\":{:.2}}},",
            "\"corpus\":\"synthetic happy (DISPLAY+COMP-3+COMP+LEVEL-88) + POSTING.1/EXTRACT.PROFILE.1 custody; CORPUS.2 holds the hostile fixtures\",",
            "\"host\":{{\"cpu\":{:?},\"ncpu\":{},\"arch\":{:?},\"profile\":{:?},\"rayon\":{},\"simd\":false}},",
            "\"non_claims\":[\"no production performance claim\",\"no AWS performance claim\",\"no SIMD claim\",\"no deterministic-scheduling claim beyond identical artifacts\",\"no customer-workload representativeness\",\"no semantic change\"]}}\n"
        ),
        out_hash, n, bytes, RL,
        rps, us_rec, mbps,
        decode.as_micros() as f64 / n as f64,
        (full.as_micros() as f64 - decode.as_micros() as f64) / n as f64,
        custody.as_micros() as f64 / n as f64,
        prof.0, prof.1, prof.2, pct(prof.1), pct(prof.2), bottleneck,
        rayon_on, rayon_on, rayon_rps, rayon_us, speedup,
        cpu_model(), ncpu(),
        std::env::consts::ARCH,
        if cfg!(debug_assertions) { "debug" } else { "release" },
        rayon_on,
    );
    std::fs::write("reports/BENCH-2-receipt.json", &receipt).ok();
    eprintln!("KOBOLD.BENCH.2 (scalar, parity-gated): {n} records, {:.0} rec/s, {:.3} µs/rec, {:.1} MB/s (decode-only {:.3}, custody {:.3} µs/rec)",
              rps, us_rec, mbps, decode.as_micros() as f64 / n as f64, custody.as_micros() as f64 / n as f64);
    if rayon_on {
        eprintln!("KOBOLD.PERF.1 (rayon, byte-identical): {:.0} rec/s, {:.3} µs/rec, {:.2}× speedup", rayon_rps, rayon_us, speedup);
    }
    eprintln!("KOBOLD.PERF.2 stage profile: parse {:.1}% · per-record {:.1}% · aggregate {:.1}% — bottleneck = {bottleneck}",
              pct(prof.0), pct(prof.1), pct(prof.2));
    eprintln!("receipt: reports/BENCH-2-receipt.json");
}
