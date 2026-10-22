//! D4469 road 2: the cost of a pgrcolumnar sorted path, whose scan k-way merges the parts' sorted streams (hosts
//! `MergedRangeSource`). An estimate only: the scan merges whatever parts its snapshot sees, so a plan stays right at
//! any part count. Pessimistic until the part-level key ranges feed it: every row is its own run (no part is assumed
//! disjoint from another), so the merged path wins where startup decides (an early stop, a merge join's inner) and a
//! full ordered read of many parts keeps its Sort.

/// The most streams the planner merges (its estimate, the committed part count); past it the plan sorts.
pub const K_PLAN: u64 = 256;

/// The merge's `(startup, run)` cost over the seqscan path's own for `parts` streams (0 or 1: none, the
/// part's own order) and `rows` rows, `granule_cost` the seqscan's cost of one granule: each stream's first granule is
/// read before the first row, and each row costs a comparison per heap level and a run's window.
pub fn merge_cost(parts: u64, rows: f64, granule_cost: f64, cpu_operator_cost: f64, cpu_tuple_cost: f64) -> (f64, f64) {
    if parts <= 1 {
        return (0.0, 0.0);
    }
    let levels = (64 - (parts - 1).leading_zeros()) as f64;
    let startup = parts as f64 * granule_cost;
    (startup, rows.max(0.0) * (cpu_operator_cost * levels + cpu_tuple_cost))
}

#[cfg(test)]
mod tests {
    use super::merge_cost;

    #[test]
    fn one_part_costs_nothing_and_more_parts_cost_more() {
        assert_eq!(merge_cost(0, 1e6, 10.0, 0.0025, 0.01), (0.0, 0.0));
        assert_eq!(merge_cost(1, 1e6, 10.0, 0.0025, 0.01), (0.0, 0.0));
        let (s2, r2) = merge_cost(2, 1e6, 10.0, 0.0025, 0.01);
        assert_eq!(s2, 20.0);
        assert!((r2 - 1e6 * (0.0025 + 0.01)).abs() < 1e-6, "{r2}");
        let (s17, r17) = merge_cost(17, 1e6, 10.0, 0.0025, 0.01);
        assert_eq!(s17, 170.0);
        assert!((r17 - 1e6 * (0.0025 * 5.0 + 0.01)).abs() < 1e-6, "five heap levels over 17 streams: {r17}");
    }
}
