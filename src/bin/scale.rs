//! KOBOLD.SCALE.1 — local synthetic scale measurement (separate from PERF.1).
//!
//! Generates a declared synthetic **mixed fixed-record** corpus to a temp file, then STREAMS it through
//! the sealed reconcile pipeline in fixed reconcile-blocks (dropping each block's output after hashing, so
//! memory stays bounded even at multi-GB). Scalar and Rayon use the **same** block unit, so their combined
//! output hashes are byte-identical by construction; Rayon timing is admitted only after that match (and a
//! pinned baseline). NO production SLA / AWS cost / customer-workload / mainframe / universal-throughput claim.

use gnucobol_rs::{build_field, cob_move, FieldAttr, Usage, COB_FLAG_HAVE_SIGN, COB_TYPE_NUMERIC_DISPLAY};
use kobold_data_shim::recon::reconcile_encoded;
use kobold_data_shim::{Encoding, NoCopy};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::time::Instant;

const CB: &str = "       01 SCALE-REC.\n           05 SEQ-NO PIC 9(8).\n           05 NAME PIC X(10).\n           05 AMOUNT PIC S9(7)V99 COMP-3.\n           05 BRANCH PIC 9(4) COMP.\n           05 RISK PIC 9(6) COMP-X.\n           05 STATUS PIC X.\n               88 ACTIVE VALUE \"A\".\n";
const RECON_RECORDS: usize = 2_000; // one reconcile/hash unit (same for scalar + rayon)
const WAVE: usize = 64; // reconcile-blocks read per wave (bounds memory)

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
    if scale > have {
        d.resize(d.len() + (scale - have) as usize, 0);
    }
    while d.len() < pf.attr.digits as usize {
        d.insert(0, 0);
    }
    let extra = d.len().saturating_sub(pf.attr.digits as usize);
    d.drain(0..extra);
    let mut src: Vec<u8> = d.iter().map(|x| b'0' + x).collect();
    if neg {
        if let Some(l) = src.last_mut() {
            *l |= 0x40;
        }
    }
    let sa = FieldAttr { field_type: COB_TYPE_NUMERIC_DISPLAY, digits: pf.attr.digits, scale: pf.attr.scale, flags: COB_FLAG_HAVE_SIGN };
    let mut out = vec![0u8; pf.size];
    cob_move(&src, &sa, &mut out, &pf.attr).unwrap();
    out
}

fn template(rng: &mut Lcg) -> Vec<u8> {
    let mut r = Vec::new();
    r.extend(enc("9(8)", Usage::Display, "00000000")); // SEQ-NO placeholder (overwritten per record)
    let mut name = format!("CUST{:06}", rng.next(999999)).into_bytes();
    name.resize(10, b' ');
    r.extend(name); // NAME X(10)
    r.extend(enc("S9(7)V99", Usage::Comp3, &format!("{}.{:02}", rng.next(9_999_999), rng.next(100))));
    r.extend(enc("9(4)", Usage::Comp, &format!("{}", 1000 + rng.next(8999))));
    r.extend(enc("9(6)", Usage::CompX, &format!("{}", rng.next(999_999))));
    r.push(if rng.next(2) == 0 { b'A' } else { b'C' }); // STATUS
    r
}

fn peak_rss_kb() -> u64 {
    std::fs::read_to_string("/proc/self/status").ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("VmHWM")).and_then(|l| l.split_whitespace().nth(1).and_then(|n| n.parse().ok())))
        .unwrap_or(0)
}
fn cpu_model() -> String {
    std::fs::read_to_string("/proc/cpuinfo").ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("model name")).map(|l| l.split(':').nth(1).unwrap_or("").trim().to_string()))
        .unwrap_or_else(|| "unknown".into())
}
fn audit_field<'a>(a: &'a str, k: &str) -> Option<&'a str> {
    let m = format!("\"{k}\":\"");
    let s = a.find(&m)? + m.len();
    let e = a[s..].find('"')? + s;
    Some(&a[s..e])
}
fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Reconcile one block and fold its content into a 32-byte digest (its decode_output_sha256, re-hashed).
fn block_digest(block: &[u8], rl: usize) -> [u8; 32] {
    let r = reconcile_encoded("scale", CB, block, rl, "0.7.1", &NoCopy, Encoding::Ascii).unwrap();
    let h = audit_field(&r.audit_json, "decode_output_sha256").unwrap_or("");
    let mut d = Sha256::new();
    d.update(h.as_bytes());
    d.finalize().into()
}

fn main() {
    let arg = std::env::args().nth(1).unwrap_or_else(|| "100m".into());
    let target_bytes: u64 = match arg.as_str() {
        "100m" => 100 << 20,
        "1g" => 1 << 30,
        "5g" => 5u64 << 30,
        "10g" => 10u64 << 30,
        s => s.parse().unwrap_or(100 << 20),
    };

    let mut rng = Lcg(0x5CA1E_00001);
    let templates: Vec<Vec<u8>> = (0..1000).map(|_| template(&mut rng)).collect();
    let rl = templates[0].len();
    // record_count rounded to a whole number of reconcile-blocks (no partial record in any block)
    let record_count = ((target_bytes / rl as u64) as usize / RECON_RECORDS) * RECON_RECORDS;
    let path = format!("/tmp/kobold-scale-{}.dat", std::process::id());
    // Fold the POSTING.1 hash chain over record bytes DURING generation (free), so the scalar/rayon decode
    // passes measure pure decode throughput, not the chain cost.
    let mut chain: Option<[u8; 32]> = None;
    {
        let mut w = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(&path).unwrap());
        let mut rec = vec![0u8; rl];
        for i in 0..record_count {
            rec.copy_from_slice(&templates[i % 1000]);
            rec[..8].copy_from_slice(format!("{:08}", i % 100_000_000).as_bytes());
            w.write_all(&rec).unwrap();
            let mut d = Sha256::new();
            if let Some(prev) = chain {
                d.update(hex(&prev).as_bytes());
            }
            d.update(&rec);
            chain = Some(d.finalize().into());
        }
        w.flush().unwrap();
    }
    let posting_chain = chain.map(|c| hex(&c));
    let temp_disk_bytes = std::fs::metadata(&path).unwrap().len();
    let corpus_manifest = {
        let mut d = Sha256::new();
        d.update(format!("rl={rl};n={record_count};block={RECON_RECORDS};").as_bytes());
        for t in &templates {
            d.update(t);
        }
        hex(&d.finalize())
    };
    eprintln!("corpus: {} records × {} B = {:.3} GB at {path}", record_count, rl, temp_disk_bytes as f64 / 1e9);

    let recon_bytes = RECON_RECORDS * rl;
    // Stream the corpus in waves of reconcile-blocks. Scalar = serial; Rayon = par over the SAME blocks
    // (order-preserving collect) -> identical combined hash by construction. Decode-only (no chain cost).
    let scan = |parallel: bool| -> (String, u128) {
        let t = Instant::now();
        let mut f = std::io::BufReader::with_capacity(1 << 22, std::fs::File::open(&path).unwrap());
        let mut master = Sha256::new();
        loop {
            let mut blocks: Vec<Vec<u8>> = Vec::with_capacity(WAVE);
            for _ in 0..WAVE {
                let mut b = vec![0u8; recon_bytes];
                let mut filled = 0;
                while filled < recon_bytes {
                    let n = f.read(&mut b[filled..]).unwrap();
                    if n == 0 {
                        break;
                    }
                    filled += n;
                }
                if filled == 0 {
                    break;
                }
                b.truncate(filled);
                blocks.push(b);
            }
            if blocks.is_empty() {
                break;
            }
            let digests: Vec<[u8; 32]> = if parallel {
                #[cfg(feature = "rayon")]
                {
                    use rayon::prelude::*;
                    blocks.par_iter().map(|b| block_digest(b, rl)).collect()
                }
                #[cfg(not(feature = "rayon"))]
                {
                    blocks.iter().map(|b| block_digest(b, rl)).collect()
                }
            } else {
                blocks.iter().map(|b| block_digest(b, rl)).collect()
            };
            for d in digests {
                master.update(d);
            }
            if blocks.len() < WAVE {
                break;
            }
        }
        (hex(&master.finalize()), t.elapsed().as_millis())
    };

    let (scalar_hash, scalar_ms) = scan(false);
    let rayon_on = cfg!(feature = "rayon");
    let (rayon_hash, rayon_ms) = if rayon_on { scan(true) } else { (scalar_hash.clone(), 0) };
    let peak = peak_rss_kb();
    let _ = std::fs::remove_file(&path);

    // PARITY GATE: rayon output hash must equal scalar; and (if pinned) match the baseline.
    if rayon_on && rayon_hash != scalar_hash {
        eprintln!("SCALE PARITY FAIL: rayon {rayon_hash} != scalar {scalar_hash} — measurement NOT admitted.");
        std::process::exit(1);
    }
    let base_path = format!("reports/SCALE-1-baseline-{arg}.json");
    let baseline = std::fs::read_to_string(&base_path).ok().and_then(|s| audit_field(&s, "output_sha256").map(str::to_string));
    match &baseline {
        Some(b) if *b != scalar_hash => {
            eprintln!("SCALE PARITY FAIL: output {scalar_hash} != baseline {b} — measurement NOT admitted.");
            std::process::exit(1);
        }
        Some(_) => eprintln!("scale parity: output hash matches scalar + baseline ({}…)", &scalar_hash[..12]),
        None => {
            let _ = std::fs::create_dir_all("reports");
            std::fs::write(&base_path, format!("{{\"schema\":\"kobold-scale-baseline-v1\",\"size\":\"{arg}\",\"output_sha256\":\"{scalar_hash}\"}}\n")).ok();
            eprintln!("scale parity: established baseline ({}…)", &scalar_hash[..12]);
        }
    }

    let spr = record_count as f64 / (scalar_ms.max(1) as f64 / 1000.0);
    let rpr = if rayon_ms > 0 { record_count as f64 / (rayon_ms as f64 / 1000.0) } else { 0.0 };
    let receipt = format!(
        concat!(
            "{{\"schema\":\"kobold-scale-receipt-v1\",\"campaign\":\"KOBOLD.SCALE.1\",\"admitted\":{},",
            "\"corpus\":{{\"kind\":\"synthetic_mixed_fixed_record\",\"target_size_bytes\":{},\"record_count\":{},",
            "\"record_len\":{},\"reconcile_block_records\":{},\"manifest_sha256\":\"{}\"}},",
            "\"modes\":{{\"scalar\":{{\"wall_ms\":{},\"records_per_sec\":{:.0},\"bytes_per_sec\":{:.0},\"output_sha256\":\"{}\"}},",
            "\"rayon\":{{\"enabled\":{},\"wall_ms\":{},\"records_per_sec\":{:.0},\"bytes_per_sec\":{:.0},\"output_sha256\":\"{}\",\"matches_scalar\":{}}}}},",
            "\"custody\":{{\"posting_last_chain_hash\":\"{}\"}},",
            "\"resources\":{{\"peak_rss_kb\":{},\"temp_disk_bytes\":{}}},",
            "\"host\":{{\"cpu\":{:?},\"arch\":{:?},\"profile\":{:?}}},",
            "\"non_claims\":[\"NEG.SCALE.CUSTOMER_WORKLOAD\",\"NEG.SCALE.PRODUCTION_SLA\",\"NEG.SCALE.AWS_COST\",",
            "\"NEG.SCALE.MAINFRAME_EQUIVALENCE\",\"NEG.SCALE.UNIVERSAL_THROUGHPUT\",\"NEG.SCALE.RANDOM_BYTES_NOT_BUSINESS_CORPUS\"]}}\n"
        ),
        arg == "1g",
        target_bytes, record_count, rl, RECON_RECORDS, corpus_manifest,
        scalar_ms, spr, temp_disk_bytes as f64 / (scalar_ms.max(1) as f64 / 1000.0), scalar_hash,
        rayon_on, rayon_ms, rpr, if rayon_ms > 0 { temp_disk_bytes as f64 / (rayon_ms as f64 / 1000.0) } else { 0.0 }, rayon_hash, rayon_on && rayon_hash == scalar_hash,
        posting_chain.as_deref().unwrap_or(""),
        peak, temp_disk_bytes,
        cpu_model(), std::env::consts::ARCH, if cfg!(debug_assertions) { "debug" } else { "release" },
    );
    let out = format!("reports/SCALE-1-receipt-{arg}.json");
    let _ = std::fs::create_dir_all("reports");
    std::fs::write(&out, &receipt).ok();
    eprintln!("KOBOLD.SCALE.1 [{arg}]: {record_count} rec, scalar {:.0} rec/s ({} ms), rayon {:.0} rec/s ({} ms), peak_rss {} MB, disk {:.2} GB",
              spr, scalar_ms, rpr, rayon_ms, peak / 1024, temp_disk_bytes as f64 / 1e9);
    eprintln!("receipt: {out}");
}
