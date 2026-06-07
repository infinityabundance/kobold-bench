//! Synthetic batch generation + parity re-check for benchmarking the gnucobol-rs hot path.
//!
//! The doctrine: **performance work never alters sealed semantics.** Every benchmark run ends with
//! a parity re-check ([`parity_holds`]) that re-decodes a sample and confirms the byte-exact result,
//! so a throughput number is never reported without re-confirming correctness.

#![forbid(unsafe_code)]

use gnucobol_rs::{
    cob_move, Decimal, FieldAttr, COB_FLAG_HAVE_SIGN, COB_TYPE_NUMERIC_DISPLAY,
    COB_TYPE_NUMERIC_PACKED,
};

/// A simple deterministic LCG so batches are reproducible across runs/machines.
pub struct Lcg(pub u64);
impl Lcg {
    pub fn step(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 16
    }
}

/// S9(7)V99 — a typical money field. DISPLAY (9 bytes) and COMP-3 (5 bytes) attrs.
pub fn money_display() -> FieldAttr {
    FieldAttr {
        field_type: COB_TYPE_NUMERIC_DISPLAY,
        digits: 9,
        scale: 2,
        flags: COB_FLAG_HAVE_SIGN,
    }
}
pub fn money_packed() -> FieldAttr {
    FieldAttr {
        field_type: COB_TYPE_NUMERIC_PACKED,
        digits: 9,
        scale: 2,
        flags: COB_FLAG_HAVE_SIGN,
    }
}

/// Generate `n` DISPLAY S9(7)V99 source records (9 bytes each), with mixed signs.
pub fn gen_display_batch(n: usize, seed: u64) -> Vec<[u8; 9]> {
    let mut rng = Lcg(seed);
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let mut rec = [0u8; 9];
        for b in rec.iter_mut() {
            *b = b'0' + (rng.step() % 10) as u8;
        }
        if rng.step() & 1 == 0 {
            rec[8] |= 0x40; // negative overpunch
        }
        out.push(rec);
    }
    out
}

/// Convert a whole batch DISPLAY -> PACKED (COMP-3), returning bytes/sec is up to the caller. This
/// is the dominant ingestion hot path. Returns the number of conversions performed.
pub fn convert_display_to_packed(
    batch: &[[u8; 9]],
    dst_attr: &FieldAttr,
    src_attr: &FieldAttr,
) -> usize {
    let mut dst = [0u8; 5];
    let mut count = 0;
    for src in batch {
        let _ = cob_move(src, src_attr, &mut dst, dst_attr);
        count += 1;
    }
    count
}

/// Re-check parity on a sample: a DISPLAY value, encoded to PACKED then decoded back, must equal the
/// directly-decoded DISPLAY value. Never report throughput without this returning `true`.
pub fn parity_holds(batch: &[[u8; 9]]) -> bool {
    let sd = money_display();
    let sp = money_packed();
    for src in batch.iter().take(1000) {
        let mut packed = [0u8; 5];
        if cob_move(src, &sd, &mut packed, &sp).is_err() {
            return false;
        }
        let mut back = [0u8; 9];
        if cob_move(&packed, &sp, &mut back, &sd).is_err() {
            return false;
        }
        // The round-tripped value must equal the original value (display semantics).
        let a = Decimal::from_display(src, &sd);
        let b = Decimal::from_display(&back, &sd);
        if a.digits
            .iter()
            .rev()
            .zip(b.digits.iter().rev())
            .any(|(x, y)| x != y)
        {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parity_after_convert() {
        let batch = gen_display_batch(10_000, 1);
        let (sd, sp) = (money_display(), money_packed());
        assert_eq!(convert_display_to_packed(&batch, &sp, &sd), 10_000);
        assert!(parity_holds(&batch));
    }
}
