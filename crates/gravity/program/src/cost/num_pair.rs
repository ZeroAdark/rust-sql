//! F4: whether num.partial's pair placement (lowerv4 ops/agg/numeric/strategies/num_partial.rs, road num.pair) pays for an
//! image with a key past one byte, from the group estimate. A row whose image holds its pair block adds there with no
//! hash (`HIT` saved); a row whose pair block another image holds pays the pair test, its branch the other way and the
//! hashed path (`DISPLACED`). Placed by the keys' bytes, the images of keys past one byte share blocks about as a random
//! placement does (codes that differ in one byte do not, which this rule does not see), so the displaced share is the
//! random placement's: `1 - (blocks / G) (1 - (1 - 1 / blocks)^G)` of `G` groups. One-byte keys keep P2's bound (the
//! groups within the blocks): their bytes place them as a bijection.
use cost_profile::anchor::Anchor;
use cost_profile::fit::Fitted;

const MACHINE: &str = "box gencoll: c8g.4xlarge, Graviton 4 (Neoverse V2), 16 vCPU";
/// The dev data (never a benchmark's statements): synthetic heap tables of 6 M rows (nt_q, nt_px, nt_pw), text group
/// keys of 6 .. 64 values (char(1) .. char(8), varchar(8), short texts, 7-byte codes), money-like numeric(15,2) measures;
/// count(*) and 4 or 8 sums / avgs of columns and products. The pair against road num.pair off, one postmaster an arm, a
/// warm run then a timed one, pair / off / off / pair, their means, W0. The two prices solve nt_q's two cells whose
/// images were displaced (counted from their bytes' placement): 64 codes, 5 of 64 displaced, -1.6 %; three keys of 24
/// groups, 4.2 of 24 displaced, +3.8 %. nt_px's and nt_pw's cells check the rule's side (none past 3 % and 10 ms).
const CORPUS: &str = "F4 pair cells (nt_q / nt_px / nt_pw: 6 M heap rows, text keys of 6 .. 64 values, pair against num.pair off, W0)";

// One anchor (update): both prices are the hashed path's, so the rule's 15 groups hold on every host.
const fn fitted(ns: f64) -> Fitted {
    Fitted { ns, machine: MACHINE, corpus: CORPUS, anchor: Anchor::Update }
}

/// A row whose image holds its pair block: the hashed path's cost it skips (a hashed probe of the L1-resident blocks).
pub const HIT: Fitted = fitted(6.2);
/// A row whose pair block another image holds: the pair test, its branch taken the other way, then the hashed path.
pub const DISPLACED: Fitted = fitted(52.7);

/// The share of `groups` images, placed at random over `blocks` blocks, whose block an earlier image holds.
pub fn displaced(groups: f64, blocks: f64) -> f64 {
    if groups <= 0.0 || blocks <= 0.0 {
        return 0.0;
    }
    1.0 - blocks / groups * (1.0 - (1.0 - 1.0 / blocks).powf(groups))
}

/// Whether the pair pays for an image with a key past one byte under an estimate of `groups` groups over `blocks`
/// blocks: the displaced rows' cost below the hit rows' saving. An estimate of no groups (none known): no pair.
pub fn pair_pays(groups: u64, blocks: u32) -> bool {
    if groups == 0 {
        return false;
    }
    let d = displaced(groups as f64, blocks as f64);
    (1.0 - d) * HIT.ns > d * DISPLACED.ns
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The displaced share climbs with the groups, with no step: none for one group, about a twentieth at 8 of 64 blocks,
    /// over a third at 64.
    #[test]
    fn the_displaced_share_climbs_with_the_groups() {
        assert!(displaced(1.0, 64.0).abs() < 1e-12);
        assert!((displaced(8.0, 64.0) - 0.053).abs() < 0.001 && (displaced(64.0, 64.0) - 0.365).abs() < 0.001);
        assert!((1..128).all(|g| displaced(g as f64 + 1.0, 64.0) > displaced(g as f64, 64.0)));
    }

    /// Keys past one byte take the pair up to about a quarter of the blocks (15 of 64), not at 16 or more, nor with no
    /// estimate.
    #[test]
    fn the_pair_pays_up_to_a_quarter_of_the_blocks() {
        assert!(pair_pays(8, 64) && pair_pays(15, 64));
        assert!(!pair_pays(16, 64) && !pair_pays(24, 64) && !pair_pays(64, 64) && !pair_pays(0, 64));
    }
}
