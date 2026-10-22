//! The route of a grouped count(DISTINCT)'s pairs (STEP-19, gap-optoverfit mechanism R). It replaces `GROUPED_MIN` = 1 << 16,
//! carried from lowerv2's grouped count(DISTINCT) kernel, of which lowerv4 count_cut.rs and kernels t3.rs each held a copy.
//!
//! By the group words, a group's pairs meet in one owner partition and its count is final when that partition's fold ends:
//! one round (hosts distinct_fold). By the whole pair, each partition dedups its pairs into per-group accumulators that a
//! second round re-sinks by the group (exec owner.rs `distinct_fns`). One round does less work, since nothing is re-sunk.
//! - One owner (a serial producer: the width-1 arm folds every partition on one owner, driver section.rs `own_all`) pays
//!   nothing for it, so its pairs always route by the group; its owner is exec's two-round one over those partitions (exec
//!   owner.rs `owner_fns_at`: round 1 sees each group in one partition, round 2 re-sinks one accumulator per group).
//! - Parallel owners each take a range of the partitions, so the partitions are only as even as the groups hash, and the
//!   owner that takes the fullest finishes last. G groups of like size over P partitions leave the fullest about
//!   `1 + sqrt(2 P ln P / G)` times the mean (the maximum load of G balls in P bins). The second round re-sinks one
//!   accumulator per group a partition saw: about `min(G P, D)` of the D deduped pairs, at `RESINK` (`RESINK_X10` / 10) of a
//!   pair's first-round cost. The route by the group pays when the excess is under that share:
//!   `sqrt(2 P ln P / G) <= RESINK min(1, G P / N)`, with P the partitions the planned groups are cut into (`spill_bits`)
//!   and N the producer's rows (D <= N, so the share is a lower bound; no estimate reads as the sparse case, one pair per
//!   group and partition).
//!
//! The run time re-checks it: a partitioned owner phase whose partition does not fit its owner's region is redone on one
//! owner (driver section.rs `own_single`, D533), so a group the estimate called small costs time, never the statement.
//!
//! The rule lives beside `params::spill_bits`, whose partitions it reads, because the kernels decide it too (t3.rs `grouped`)
//! and the kernel boundary (docs/gravity/abi-v2.md §0, lint kernel_boundary.rs) keeps the cost crates out of the kernels;
//! lowerv4 count_cut.rs `pair_partition` calls it here, so it has one home.

/// The second round's cost per re-sunk accumulator over the first round's per deduped pair. FITTED (STEP-19, c8g box 57
/// (Graviton 4), 2026-10-03) on the dev table `tpd` (8,000,000 rows from `generate_series`, groups `g % 2^10 .. g % 2^20`, a 25- or 8-valued
/// argument; no benchmark statement): same binary, one round against two, arms interleaved (topn STATUS "STEP-19 ABAB").
/// Per accumulator, the two-round arm's extra time over the groups it re-sinks; per pair, the one-round arm's time over its
/// deduped pairs past the 2^10-group statement's; the least ratio of the 2^16, 2^18 and 2^20 statements (8.0, 26.8, 6.6).
/// In tenths, so the rule stays integer arithmetic (abi-v2 §0).
pub const RESINK_X10: u64 = 66;

/// `ln 2^b` for b = 0..=20, scaled by 2^16 (`round(b ln 2 * 2^16)`): the rule's one transcendental, tabulated (abi-v2 §0).
const LN_P_Q16: [u64; 21] = [
    0, 45426, 90852, 136278, 181704, 227130, 272557, 317983, 363409, 408835, 454261, 499687, 545113, 590539, 635965, 681391,
    726817, 772244, 817670, 863096, 908522,
];

/// Whether `groups` planned groups route by their group words (one round) over the `2^bits` partitions their pair table is cut
/// into (the caller's `params::spill_bits(groups, record)`), the producer's `rows` estimate (0:
/// none) and whether its owner phase can run on parallel owners (module doc).
pub fn group_routed(groups: u64, bits: u8, rows: u64, parallel: bool) -> bool {
    if groups == 0 {
        return false;
    }
    if !parallel {
        return true;
    }
    let b = bits.min(20);
    let (p, g, n, ln) = (1u128 << b, groups as u128, rows as u128, LN_P_Q16[b as usize] as u128);
    let r2 = (RESINK_X10 * RESINK_X10) as u128;
    // `2 P ln P / G <= (RESINK s)^2`, s = min(1, G P / N), both sides times 100 G 2^16 (s = 1) or 100 G N^2 2^16 / P (s < 1)
    if rows == 0 || g * p >= n {
        200 * p * ln <= (r2 * g) << 16
    } else {
        (200 * ln).saturating_mul(n.saturating_mul(n)) <= (r2 << 16).saturating_mul(g.saturating_mul(g)).saturating_mul(g).saturating_mul(p)
    }
}

/// D3784: `group_routed` when the largest group holds `top` (2^-16) of the producer's rows (0: unknown, the rule above).
/// The fullest partition holds at least that group, so the excess it leaves over the partitions' mean, `top P - 1` means
/// when the group alone passes one, must also stay under the second round's share: `top P - 1 <= RESINK min(1, G P / N)`
/// (the module doc's rule with the group's load in place of the even spread's). A grouped count(DISTINCT) whose one group
/// holds most rows routed by the group leaves one owner to dedup them alone.
pub fn group_routed_skewed(groups: u64, bits: u8, rows: u64, parallel: bool, top: u32) -> bool {
    if !group_routed(groups, bits, rows, parallel) {
        return false;
    }
    if !parallel || top == 0 {
        return true;
    }
    let p = 1u128 << bits.min(20);
    let load = (top as u128).min(1 << 16) * p;
    if load <= 1 << 16 {
        return true;
    }
    // 10 (load - 1) N <= RESINK_X10 min(N, G P), both sides times 2^16 (min = N: no rows estimate, or G P >= N)
    let (excess, g, n) = (load - (1 << 16), groups as u128, rows as u128);
    let resink = (RESINK_X10 as u128) << 16;
    if rows == 0 || g * p >= n {
        10 * excess <= resink
    } else {
        (10 * excess).saturating_mul(n) <= resink.saturating_mul(g).saturating_mul(p)
    }
}

/// The groups the fullest of `owners` owners holds when `groups` groups hash over them (KEYED-28: a two-round owner's second
/// round re-sinks every group by its hash): the mean plus the module's maximum-load excess, `sqrt(2 (G / W) ln W)`, with `ln W`
/// taken at the next power of two (an upper bound). Never more than `groups`; one owner holds them all.
pub fn owner_share(groups: u64, owners: u32) -> u64 {
    if owners <= 1 {
        return groups;
    }
    let mean = groups.div_ceil(owners as u64);
    let ln = LN_P_Q16[(owners.next_power_of_two().trailing_zeros() as usize).min(20)] as u128;
    let excess = ((2 * mean as u128 * ln) >> 16).isqrt() as u64 + 1;
    (mean + excess).min(groups)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_owner_share_is_the_mean_and_its_excess() {
        assert_eq!(owner_share(200_000, 1), 200_000);
        assert_eq!(owner_share(0, 8), 0);
        for (g, w) in [(64u64, 2u32), (2000, 8), (200_000, 8), (1 << 20, 6), (7, 16)] {
            let s = owner_share(g, w);
            let real = g as f64 / w as f64 + (2.0 * (g as f64 / w as f64) * (w.next_power_of_two() as f64).ln()).sqrt();
            assert!(s <= g && s as f64 >= real.min(g as f64) && (s as f64) < real + 3.0, "groups {g} owners {w}: {s} vs {real}");
        }
        assert!(owner_share(200_000, 8) < 200_000 / 7 && owner_share(2000, 8) < 2000 / 6);
    }

    #[test]
    fn one_owner_always_routes_by_the_group() {
        assert!(group_routed(1, 3, 1_000_000, false) && group_routed(1 << 20, 6, 0, false));
        assert!(!group_routed(0, 3, 0, false) && !group_routed(0, 3, 0, true));
    }

    #[test]
    fn parallel_owners_route_by_the_group_past_the_balance_bound() {
        // sparse (no rows, or one pair per group and partition) over the least partitions: any group
        assert!(group_routed(1, 3, 0, true) && group_routed(1 << 16, 3, 1 << 17, true) && group_routed(1 << 20, 6, 8_000_000, true));
        // dense: a few groups over many rows re-sink little, so they keep the pair route
        assert!(!group_routed(1024, 3, 8_000_000, true) && !group_routed(64, 3, 1 << 20, true));
        // more partitions (wider records or more groups) want more groups
        assert!(group_routed(512, 3, 0, true) && !group_routed(512, 11, 0, true));
    }

    #[test]
    fn a_group_holding_most_rows_keeps_the_pair_route() {
        // unknown skew or one owner: the even-spread rule alone
        for (g, bits, n) in [(7140u64, 3u8, 5_396_529u64), (1 << 16, 3, 1 << 17), (512, 3, 0)] {
            assert_eq!(group_routed_skewed(g, bits, n, true, 0), group_routed(g, bits, n, true));
            assert!(group_routed_skewed(g, bits, n, false, 1 << 16));
        }
        // 7140 planned groups over 5.40 M rows route by the group when they spread evenly; one group holding 79 % of the
        // rows (one dominant key pair, hosts facts.rs `top_share`) keeps the pair route at any rows estimate near it
        // (the even rule flips between 5.40 M and 5.56 M)
        assert!(group_routed(7140, 3, 5_396_529, true) && !group_routed(7140, 3, 5_556_528, true));
        for n in [5_000_000u64, 5_396_529, 5_556_528, 6_000_000] {
            assert!(!group_routed_skewed(7140, 3, n, true, 51_707), "rows {n}");
        }
        // a largest group within a partition's mean changes nothing
        assert!(group_routed_skewed(7140, 3, 5_396_529, true, 65536 / 8));
        // the real-valued rule: top P - 1 <= RESINK min(1, G P / N)
        let real = |g: u64, bits: u8, n: u64, top: u32| {
            let (p, t) = ((1u64 << bits) as f64, top as f64 / 65536.0);
            let share = if n == 0 { 1.0 } else { (g as f64 * p / n as f64).min(1.0) } * (RESINK_X10 as f64 / 10.0);
            t * p <= 1.0 || t * p - 1.0 <= share
        };
        for bits in [1u8, 3, 6, 10] {
            for g in [1u64, 100, 7140, 1 << 16, 1 << 20] {
                for n in [0u64, 1 << 10, 5_396_529, 1 << 30] {
                    for top in [1u32, 4096, 16384, 60_948, 65536] {
                        let want = group_routed(g, bits, n, true) && real(g, bits, n, top);
                        assert_eq!(group_routed_skewed(g, bits, n, true, top), want, "groups {g} bits {bits} rows {n} top {top}");
                    }
                }
            }
        }
    }

    #[test]
    fn integer_rule_is_the_real_valued_rule() {
        for (b, &l) in LN_P_Q16.iter().enumerate() {
            assert!((l as f64 - b as f64 * core::f64::consts::LN_2 * 65536.0).abs() <= 0.5);
        }
        let real = |g: u64, bits: u8, n: u64| {
            let p = (1u64 << bits) as f64;
            let share = if n == 0 { 1.0 } else { (g as f64 * p / n as f64).min(1.0) } * (RESINK_X10 as f64 / 10.0);
            2.0 * p * p.ln() / g as f64 <= share * share
        };
        for bits in 0..=20u8 {
            for g in (0..=40).flat_map(|e| [1u64, 3, 5, 7].map(|m| m << e)) {
                for n in core::iter::once(0).chain((0..48).step_by(2).flat_map(|e| [1u64, 3].map(|m| m << e))) {
                    assert_eq!(group_routed(g, bits, n, true), real(g, bits, n), "groups {g} bits {bits} rows {n}");
                }
            }
        }
    }
}
