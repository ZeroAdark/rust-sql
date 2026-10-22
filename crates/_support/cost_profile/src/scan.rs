//! The substring scan's unit costs (dict LIKE over a dictionary payload, gravity_dict `like.rs` `build_bits_over`): a
//! rare-byte-pair scan tests each 16-byte block against two of the needle's bytes and verifies every candidate the pair
//! passes with a compare of the whole needle; a candidate the compare rejects is the scan's waste, which a pair of bytes
//! that are rare in the data avoids. Which bytes are rare is only known from the data: counting the bytes of the part of
//! the payload already scanned costs a histogram increment per byte. The re-check rule (mechanism R): at a block that
//! wasted a candidate, the scan counts the bytes it read since its last count and re-ranks its pair from all its counts
//! when the waste the observed rate projects over the rest of the payload exceeds that count's cost, and the new count at
//! least doubles the bytes counted. So a pair that suits the data never pays a count, the counts re-rank at most log2 of
//! the payload times, and the counting stops once it would cost more than twice the projected waste of the whole scan
//! (the per-entry build also stops at a re-rank that keeps the pair).

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Scan {
    pub reject_ns: f64,
    pub count_ns_per_byte: f64,
}

impl Scan {
    pub const REFERENCE: Scan = Scan { reject_ns: 1.525, count_ns_per_byte: 0.499 };

    /// Whether counting the `scanned` bytes read since the last count pays, after `rejected` rejected candidates among
    /// them, with `counted` bytes counted before and `remaining` bytes left to scan: the waste the observed rate projects
    /// over the remaining bytes (`rejected / scanned * remaining` candidates) costs more than counting the `scanned`
    /// bytes, and they at least double the count. Unit costs that are not positive never count.
    pub const fn recount_pays(&self, scanned: u64, rejected: u64, counted: u64, remaining: u64) -> bool {
        if self.reject_ns.is_nan() || self.reject_ns <= 0.0 || self.count_ns_per_byte.is_nan() || self.count_ns_per_byte <= 0.0 {
            return false;
        }
        if scanned == 0 || scanned < counted {
            return false;
        }
        rejected as f64 * self.reject_ns * remaining as f64 > scanned as f64 * scanned as f64 * self.count_ns_per_byte
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_scan_counts_when_the_projected_waste_passes_the_count() {
        let s = Scan { reject_ns: 2.0, count_ns_per_byte: 0.5 };
        assert!(s.recount_pays(100, 1, 0, 10_000), "1 rejected x 2 ns x 10 000 bytes left = 20 000 > 100 x 100 x 0.5 ns = 5 000");
        assert!(!s.recount_pays(100, 1, 0, 2_000), "4 000 < 5 000: too little payload left to pay");
        assert!(!s.recount_pays(100, 1, 200, 10_000), "100 new bytes do not double 200 counted ones");
        assert!(s.recount_pays(200, 2, 200, 10_000));
        assert!(!s.recount_pays(0, 0, 0, 0));
        assert!(!Scan { reject_ns: 0.0, count_ns_per_byte: 0.5 }.recount_pays(1, 1_000, 0, 1 << 40), "no fitted cost: never");
    }
}
