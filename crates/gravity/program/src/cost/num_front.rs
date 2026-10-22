//! A-24c: whether a hashed group's front of per-claim i64 partials (lowerv4 ops/agg/numeric/strategies/num_partial.rs,
//! road num.partial) pays, decided at plan time from the group's aggregate list and its group estimate. A row the front
//! takes adds its terms into its key image's block in place of the record fold, so it saves each aggregate's record step
//! (`SAVE`, by the register kind the block closes into) and pays the front's probe (`BASE`); its blocks close once a
//! claim, so no cost is paid a window. A group that finds both of its blocks claimed (two adjacent blocks of the front's,
//! first come) folds its rows in the record as before and pays, on top, the front's probe, its miss (`MISS`) and each
//! aggregate's term the row computed before its probe (`TERM`, by kind); its share of the groups (`missed`) grows with
//! the groups over the blocks, with no step, so a group estimate one past the blocks decides as one at them, and the
//! miss's price is what declines the front as the groups outgrow the blocks.
use cost_profile::anchor::Anchor;
use cost_profile::fit::Fitted;

const MACHINE: &str = "box gencoll: c8g.4xlarge, Graviton 4 (Neoverse V2), 16 vCPU";
/// The dev data (never a benchmark's statements or a bar cell): a synthetic heap table of 6 M rows (47 a page), text
/// group keys of 2 / 4 / 8 / 16 / 32 / 64 / 128 values spread at random over the rows (`abs(hashint8(i + seed)) % G`),
/// money-like numeric(15,2), narrow numeric(9,2) / (6,3) and numeric(20,6) measures. Each statement groups by one key:
/// count(*) and 1 or 4 aggregates of one kind (sums of a column, of a product within 18 digits or past them, min / max,
/// sums of the numeric(20,6) column, count(x) beside a sum, a product past 38 digits beside three). The front planned
/// past this rule as it ships, under a run-time measuring switch the front no longer has, against road num.partial off, one
/// postmaster an arm, a warm run then a timed one, two takes an arm in the order front, off, off, front, their means, W0;
/// 84 statements. The fit: least squares of the front's ms less the road off's over the rows, against `BASE`, `MISS` x
/// `missed`, each kind's count x `missed` x `TERM` and each kind's count x (1 - `missed`) x `SAVE`, a price that came out
/// below zero held at zero. Residual 1.5 ns a row; no statement on the wrong side past 3 % and 10 ms, at W0 or at W8.
/// Taken again on the front without that switch (the same statements, B A A B, and 12 crossing cells of 32 .. 128
/// groups): no statement on the wrong side past 3 % and 10 ms, at W0 or at W8. A fit to that take moved 9 decisions, one
/// past 1.1 % (2.9 %), and planned the front at 4 crossing cells, 3 of which it lost, so these prices stand.
const CORPUS: &str = "A-24c nt_f sweep (6 M heap rows, random text keys of 2 .. 128 values, 1 or 4 aggregates of a kind, the front under its switch, W0)";

// One anchor (update): every price is the hashed path's, so the rule decides alike on every host.
const fn fitted(ns: f64) -> Fitted {
    Fitted { ns, machine: MACHINE, corpus: CORPUS, anchor: Anchor::Update }
}

/// The register kinds an aggregate of the front closes into, the order of `SAVE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// count(x): `[P]`.
    Present = 0,
    /// A class.num_packed sum / avg of a column: `[P, P >> 63, (0,) n]`.
    Sum = 1,
    /// A class.num_packed sum / avg of a num.window tree (a product, a sum of columns).
    Tree = 2,
    /// A class.num_packed min / max of a column: `[P, n]`.
    Ext = 3,
    /// A class.num_wide sum / avg of a column past one word by its typmod: `[P, s, s, s, n]`.
    WideSum = 4,
    /// A class.num_wide sum / avg of a tree past 38 digits.
    WideTree = 5,
}

pub const KINDS: usize = 6;

/// The record fold's cost a row over the front's add, an aggregate of each kind (W0, one participant; W8's own fit:
/// 0.02 / 0.12 / 0.51 / 0.18 / 0.03 / 1.30 against a base of 0.33).
pub const SAVE: [Fitted; KINDS] = [fitted(0.077), fitted(0.968), fitted(4.221), fitted(1.471), fitted(0.556), fitted(10.741)];
/// The front's probe a row (the image's two blocks, the terms' exactness test), every row of a claim.
pub const BASE: Fitted = fitted(2.172);
/// A row of a group with no block: the miss and the claim's test beside the record fold (held at zero: the fit puts a
/// miss's cost in its terms).
pub const MISS: Fitted = fitted(0.0);
/// A row of a group with no block: an aggregate's term, computed before the probe and not used, by kind (W8's own fit:
/// 0 / 0.26 / 0.61 / 0.14 / 0.14 / 1.27).
pub const TERM: [Fitted; KINDS] = [fitted(0.735), fitted(2.246), fitted(7.221), fitted(0.702), fitted(7.134), fitted(0.0)];

/// The share of `groups` groups, each claiming first come its hash's block or the next of `blocks`, that find both
/// claimed: `a^2 / (a^2 + 0.69 a + 2.31)` at `a = groups / blocks`. Simulated (3,000 random placements a point): within
/// 0.01 of the simulation at 64 blocks for 1 .. 256 groups, within 0.021 at 16 and 32 blocks. Counted on the corpus's
/// keys (one placement each), the rows that missed: 3.1 % at 32 groups (0.086 here), 18.7 % at 64 (0.25), 51.6 % at
/// 128 (0.52).
pub fn missed(groups: f64, blocks: f64) -> f64 {
    if blocks <= 0.0 {
        return 1.0;
    }
    let a = groups / blocks;
    a * a / (a * a + 0.69 * a + 2.31)
}

/// Whether the front pays for a group of `groups` estimated groups over `blocks` blocks whose aggregates are `aggs`
/// (each kind's count, `Kind`'s order): the rows of claimed groups save each aggregate's record step, every row pays the
/// probe, a missed group's rows the miss and their terms. An estimate of no groups (none known) keeps the record fold.
pub fn front_pays(groups: u64, blocks: u32, aggs: &[u32; KINDS]) -> bool {
    if groups == 0 {
        return false;
    }
    let m = missed(groups as f64, blocks as f64);
    let of = |price: &[Fitted; KINDS]| -> f64 { aggs.iter().zip(price.iter()).map(|(&n, p)| n as f64 * p.ns).sum() };
    (1.0 - m) * of(&SAVE) > BASE.ns + m * (MISS.ns + of(&TERM))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn of(k: &[(Kind, u32)]) -> [u32; KINDS] {
        let mut a = [0; KINDS];
        for &(kind, n) in k {
            a[kind as usize] += n;
        }
        a
    }

    /// One cheap aggregate (a sum of a column, a min / max, a sum of a numeric(20,6) column) keeps the record fold; four
    /// of them, or one product past 38 digits, take the front.
    #[test]
    fn one_cheap_aggregate_keeps_the_record_fold() {
        for k in [Kind::Sum, Kind::Ext, Kind::WideSum, Kind::Present] {
            assert!(!front_pays(8, 64, &of(&[(k, 1)])), "{k:?}");
        }
        for k in [Kind::Sum, Kind::Ext, Kind::Tree] {
            assert!(front_pays(8, 64, &of(&[(k, 4)])), "{k:?}");
        }
        assert!(front_pays(8, 64, &of(&[(Kind::WideTree, 1)])));
        assert!(!front_pays(0, 64, &of(&[(Kind::Tree, 4)])), "no estimate: no front");
    }

    /// The miss share has no step: one group past the blocks decides as one at them unless the two sides are within a
    /// share point of each other, and the share climbs from none to most of the groups.
    #[test]
    fn the_miss_share_is_continuous_past_the_blocks() {
        assert!(missed(1.0, 64.0) < 0.001 && (missed(64.0, 64.0) - 0.25).abs() < 1e-9 && missed(256.0, 64.0) > 0.75);
        assert!((missed(65.0, 64.0) - missed(64.0, 64.0)).abs() < 0.006);
        let four = of(&[(Kind::Tree, 4)]);
        assert_eq!([63, 64, 65].map(|g| front_pays(g, 64, &four)), [true; 3], "four products: the front at and past the blocks");
        assert!(!front_pays(256, 64, &four), "four times the blocks: most groups miss");
    }

    /// Twice the blocks: half the groups miss and their rows pay their terms for nothing, so four products, three
    /// products beside one past 38 digits, and a list of two products, count(x), min / max and an avg of a numeric(20,6)
    /// column each take the front at the blocks and keep the record fold at twice them.
    #[test]
    fn twice_the_blocks_prices_the_missed_terms() {
        let list = of(&[(Kind::Tree, 2), (Kind::Present, 1), (Kind::Ext, 2), (Kind::WideSum, 1)]);
        for aggs in [list, of(&[(Kind::Tree, 4)]), of(&[(Kind::Tree, 3), (Kind::WideTree, 1)])] {
            assert!(front_pays(64, 64, &aggs) && !front_pays(128, 64, &aggs), "{aggs:?}");
        }
    }
}
