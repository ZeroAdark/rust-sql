//! join.payload_fp's plain text form (lowerv4 ops/hashjoin/strategies/payload_fp.rs `plain`) as the cost home's
//! break-even: a hash build whose payload column is a text value of its scan with no code lane (a merged pgrc scan's
//! datum lane, a heap column) stores the value's Fp128, hashing its bytes once a build row and writing two words
//! (`STORE_*`); each row reaching the group past the probe then keys by those words instead of hashing the bytes
//! (`SAVED_*`: no render, the value is at hand). A merged dimension's build is serial, so at a width the store stays whole
//! while the saving divides: the rule reads the dearer end. Each cost is an Amdahl pair, the share the statement's
//! participants divide (`_SPLIT`, over the width) and the share none divides (`_FLAT`).
use cost_profile::anchor::Anchor;
use cost_profile::fit::Fitted;

const MACHINE: &str = "box gencoll: c8g.4xlarge, Graviton 4 (Neoverse V2), 16 vCPU";
/// The dev data (never a benchmark's statements, the census or a bar cell): a synthetic sweep, a pgrcolumnar fact of 4 M
/// rows (ten parts) joined to one-part pgrcolumnar dimensions of 20 k / 200 k / 1 M rows declared `sorted_by` their key
/// (read through their merge mark on the build, so the grouped text is a plain value: scan.merged), the text 21 bytes of
/// 1,000 values; `GROUP BY` that text over 40 k .. 4 M joined rows (a filter on the fact); the words against the render
/// (`gravity_road_off = 'join.payload_fp'`) on one server with the rule's permille forced to 0 (an experiment build),
/// arms alternated per cell, W0 and W8, the middle of three hot runs. The fit: per width, the least-squares `rows at the
/// group x saved - build rows x store` over the render's excess. At W0 the saving a row is the cache-resident build's
/// (38.3 ns over the 20 k cells; a build past the caches saves more, ~65 ns, its render reading the value's bytes cold)
/// and the store the 1 M cells' (82.5 ns: the SipHash, two stores and the wider record); at W8 the store stays on the
/// serial merged build (41.4 ns a build row, flat) while the saving divides (4.2 ns a row).
const CORPUS: &str = "F1 plain payload sweep (4 M fact rows, merged 20 k / 200 k / 1 M dimensions, 21 B text, 40 k .. 4 M group rows, W0 + W8, middle of 3)";

/// A build row's store of a plain value's words (its bytes' SipHash, two stores, a wider record), divided among the
/// participants (W0 82.5, W8 41.4 ns a build row).
pub const STORE_SPLIT: Fitted = Fitted { ns: 47.0, machine: MACHINE, corpus: CORPUS, anchor: Anchor::Update };
/// A build row's store that the participants do not divide (a merged dimension's build runs serially).
pub const STORE_FLAT: Fitted = Fitted { ns: 35.5, machine: MACHINE, corpus: CORPUS, anchor: Anchor::Update };
/// What a row reaching the group saves (its bytes' SipHash against two loads), divided among the participants (W0 38.3,
/// W8 4.2 ns measured).
pub const SAVED_SPLIT: Fitted = Fitted { ns: 38.3, machine: MACHINE, corpus: CORPUS, anchor: Anchor::Update };
/// What a row reaching the group saves that the participants do not divide.
pub const SAVED_FLAT: Fitted = Fitted { ns: 0.0, machine: MACHINE, corpus: CORPUS, anchor: Anchor::Update };

fn at(split: Fitted, flat: Fitted, width: u16) -> f64 {
    split.ns / width.max(1) as f64 + flat.ns
}

/// The store's permille of the saving at the dearer of W0 and `widest` (`Facts::width`), rounded up: the words pay where
/// the rows at the group times 1000 reach the build's rows times it (payload_fp.rs `pays`).
pub fn permille(widest: u16) -> u64 {
    let r = [0, widest].iter().map(|&w| at(STORE_SPLIT, STORE_FLAT, w).max(0.0) / at(SAVED_SPLIT, SAVED_FLAT, w)).fold(0.0f64, f64::max);
    (r * 1000.0).ceil() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pays(group: u64, build: u64, widest: u16) -> bool {
        group * 1000 >= build * permille(widest)
    }

    /// The sweep's cells fall on the measured side at the dearer end: a 20 k build under 400 k and 4 M group rows pays at
    /// W8 (+1.4, +18.9 ms), a 200 k build under 400 k rows and a 1 M build under 2 M or 4 M rows do not (-13.1, -35.8,
    /// -19.3 ms at W8).
    #[test]
    fn the_sweeps_cells_fall_on_their_measured_side() {
        assert!(pays(400_000, 20_000, 8) && pays(4_000_000, 20_000, 8));
        assert!(!pays(400_000, 200_000, 8) && !pays(2_000_000, 1_000_000, 8) && !pays(4_000_000, 1_000_000, 8));
        assert!(permille(8) >= permille(0));
    }
}
