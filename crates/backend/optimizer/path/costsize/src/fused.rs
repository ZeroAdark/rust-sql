//! costsize.c arithmetic whose rounding depends on FMA contraction (D400).
//!
//! PostgreSQL on aarch64 is built by GCC with `-ffp-contract=fast`: a
//! multiply whose every use is an add/subtract in the same basic block is
//! fused into it. Which product fuses is decided per site by the PGDG 18.6
//! aarch64 disassembly (the D400 site table); `fp_contract` makes that
//! fusion happen on aarch64 only. Each helper is the exact statement cluster
//! of the cited C lines, pulled out so the association is unit-tested
//! against GCC's own output (see `fma_tests`).

use fp_contract::{fmadd, fmsub};
use types_pathnodes::QualCost;

/// `x = 0; x += a * b; x += c * d;` (costsize.c:1581/1585 and kin). The first
/// `+=` fuses with 0.0, which is just `a * b` rounded; the second fuses.
#[inline]
pub(crate) fn run_from_zero(a: f64, b: f64, c: f64, d: f64) -> f64 {
    fmadd(c, d, a * b)
}

/// `x = a * b; x += c * d;` (costsize.c:331/333, 1520/1524): GCC fuses the
/// FIRST product into the add and rounds the second.
#[inline]
pub(crate) fn run_from_product(a: f64, b: f64, c: f64, d: f64) -> f64 {
    fmadd(a, b, c * d)
}

/// cost_samplescan run cost (costsize.c:411/425/428).
pub(crate) fn samplescan_run_cost(
    spc_page_cost: f64,
    pages: f64,
    cpu_per_tuple: f64,
    tuples: f64,
    tlist_per_tuple: f64,
    rows: f64,
) -> f64 {
    let run_cost = fmadd(cpu_per_tuple, tuples, spc_page_cost * pages);
    fmadd(tlist_per_tuple, rows, run_cost)
}

/// cost_tidscan run cost (costsize.c:1328/1337/1341).
pub(crate) fn tidscan_run_cost(
    spc_random_page_cost: f64,
    ntuples: f64,
    cpu_per_tuple: f64,
    tlist_per_tuple: f64,
    rows: f64,
) -> f64 {
    let run_cost = fmadd(cpu_per_tuple, ntuples, spc_random_page_cost * ntuples);
    fmadd(tlist_per_tuple, rows, run_cost)
}

/// cost_tidrangescan run cost (costsize.c:1420/1435/1439).
#[allow(clippy::too_many_arguments)]
pub(crate) fn tidrangescan_run_cost(
    spc_random_page_cost: f64,
    spc_seq_page_cost: f64,
    nseqpages: f64,
    cpu_per_tuple: f64,
    ntuples: f64,
    tlist_per_tuple: f64,
    rows: f64,
) -> f64 {
    let run_cost = fmadd(spc_seq_page_cost, nseqpages, spc_random_page_cost);
    let run_cost = fmadd(cpu_per_tuple, ntuples, run_cost);
    fmadd(tlist_per_tuple, rows, run_cost)
}

/// cost_index min_IO_cost for a single scan (costsize.c:747-756).
pub(crate) fn index_min_io_cost(pages_fetched: f64, spc_random: f64, spc_seq: f64) -> f64 {
    if pages_fetched > 0.0 {
        let mut m = spc_random;
        if pages_fetched > 1.0 {
            m = fmadd(pages_fetched - 1.0, spc_seq, m); // costsize.c:753
        }
        m
    } else {
        0.0
    }
}

/// cost_index heap I/O interpolation (costsize.c:795-797):
/// `max_IO_cost + csquared * (min_IO_cost - max_IO_cost)`.
pub(crate) fn index_io_cost(index_correlation: f64, min_io_cost: f64, max_io_cost: f64) -> f64 {
    let csquared = index_correlation * index_correlation;
    fmadd(csquared, min_io_cost - max_io_cost, max_io_cost)
}

/// cost_bitmap_heap_scan per-page cost (costsize.c:1070-1073).
pub(crate) fn bitmap_cost_per_page(spc_random: f64, spc_seq: f64, pages_fetched: f64, t: f64) -> f64 {
    if pages_fetched >= 2.0 {
        fmsub(spc_random - spc_seq, (pages_fetched / t).sqrt(), spc_random)
    } else {
        spc_random
    }
}

/// cost_bitmap_heap_scan run cost (costsize.c:1077/1105/1110).
pub(crate) fn bitmap_heap_run_cost(
    pages_fetched: f64,
    cost_per_page: f64,
    cpu_run_cost: f64,
    tlist_per_tuple: f64,
    rows: f64,
) -> f64 {
    let run_cost = pages_fetched * cost_per_page + cpu_run_cost;
    fmadd(tlist_per_tuple, rows, run_cost)
}

/// compute_bitmap_pages lossy tuple estimate before clamp_row_est
/// (costsize.c:6599-6601): the first product fuses, the second rounds.
pub(crate) fn bitmap_lossy_tuples(
    index_selectivity: f64,
    exact_pages: f64,
    lossy_pages: f64,
    heap_pages: f64,
    tuples: f64,
) -> f64 {
    fmadd(
        index_selectivity * (exact_pages / heap_pages),
        tuples,
        (lossy_pages / heap_pages) * tuples,
    )
}

/// cost_incremental_sort run cost (costsize.c:2105-2119).
pub(crate) fn incremental_sort_run_cost(
    group_startup_cost: f64,
    group_run_cost: f64,
    group_input_run_cost: f64,
    input_groups: f64,
    input_tuples: f64,
    comparison_cost: f64,
    cpu_tuple_cost: f64,
) -> f64 {
    let mut run_cost = fmadd(
        group_run_cost + group_startup_cost,
        input_groups - 1.0,
        group_run_cost,
    );
    run_cost = fmadd(group_input_run_cost, input_groups - 1.0, run_cost);
    run_cost = fmadd(cpu_tuple_cost + comparison_cost, input_tuples, run_cost);
    fmadd(2.0 * cpu_tuple_cost, input_groups, run_cost)
}

/// cost_memoize_rescan's cache entry size before the param_exprs widths
/// (costsize.c:2570): relation_byte_size's multiply (costsize.c:6455) fuses
/// into the add of ExecEstimateCacheEntryOverheadBytes.
pub(crate) fn memoize_entry_bytes(tuples: f64, tuple_bytes: f64, overhead_bytes: f64) -> f64 {
    fmadd(tuples, tuple_bytes, overhead_bytes)
}

/// cost_memoize_rescan's (startup, total) from the ratios
/// (costsize.c:2621-2664). hit_ratio is never rounded: GCC fuses its multiply
/// into the one shared `1.0 - hit_ratio`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn memoize_rescan_costs(
    calls: f64,
    ndistinct: f64,
    est_cache_entries: f64,
    evict_ratio: f64,
    tuples: f64,
    input_startup_cost: f64,
    input_total_cost: f64,
    cpu_operator_cost: f64,
    cpu_tuple_cost: f64,
) -> (f64, f64) {
    let miss_ratio = fmsub(
        (calls - ndistinct) / calls,
        est_cache_entries / ndistinct.max(est_cache_entries),
        1.0,
    );
    let mut total_cost = fmadd(input_total_cost, miss_ratio, cpu_operator_cost); // :2631
    total_cost = fmadd(cpu_tuple_cost, evict_ratio, total_cost); // :2636
    total_cost = fmadd(cpu_operator_cost / 10.0 * evict_ratio, tuples, total_cost); // :2643
    total_cost += fmadd(cpu_operator_cost, tuples, cpu_tuple_cost); // :2652
    let startup_cost = fmadd(input_startup_cost, miss_ratio, cpu_tuple_cost); // :2658+2664
    (startup_cost, total_cost)
}

/// cost_agg hash-spill charge (costsize.c:2834-2841); spill_cost's last
/// multiply fuses into both of its uses.
#[allow(clippy::too_many_arguments)]
pub(crate) fn agg_spill_costs(
    startup_cost: f64,
    total_cost: f64,
    pages_written: f64,
    pages_read: f64,
    depth: f64,
    input_tuples: f64,
    random_page_cost: f64,
    seq_page_cost: f64,
    cpu_tuple_cost: f64,
) -> (f64, f64) {
    let startup_cost = fmadd(pages_written, random_page_cost, startup_cost);
    let mut total_cost = fmadd(pages_written, random_page_cost, total_cost);
    total_cost = fmadd(pages_read, seq_page_cost, total_cost);
    let spill_base = depth * input_tuples * 2.0;
    (
        fmadd(spill_base, cpu_tuple_cost, startup_cost),
        fmadd(spill_base, cpu_tuple_cost, total_cost),
    )
}

/// `qual_cost.startup + output_tuples * qual_cost.per_tuple`
/// (costsize.c:2854 cost_agg, 3226 cost_group).
pub(crate) fn qual_charge(output_tuples: f64, qual_cost: QualCost) -> f64 {
    fmadd(output_tuples, qual_cost.per_tuple, qual_cost.startup)
}

/// cost_windowagg total (costsize.c:3151/3162/3163).
#[cfg(test)]
pub(crate) fn windowagg_total_cost(
    input_total_cost: f64,
    wfunccosts: &[f64],
    num_cols: f64,
    input_tuples: f64,
    cpu_operator_cost: f64,
    cpu_tuple_cost: f64,
) -> f64 {
    let mut total_cost = input_total_cost;
    for &wfunccost in wfunccosts {
        total_cost = windowagg_add_func(total_cost, wfunccost, input_tuples);
    }
    windowagg_tail(total_cost, num_cols, input_tuples, cpu_operator_cost, cpu_tuple_cost)
}

/// One window function's per-tuple charge (costsize.c:3151).
pub(crate) fn windowagg_add_func(total_cost: f64, wfunccost: f64, input_tuples: f64) -> f64 {
    fmadd(wfunccost, input_tuples, total_cost)
}

/// Per-column compare and per-tuple charges (costsize.c:3162/3163).
pub(crate) fn windowagg_tail(
    total_cost: f64,
    num_cols: f64,
    input_tuples: f64,
    cpu_operator_cost: f64,
    cpu_tuple_cost: f64,
) -> f64 {
    let total_cost = fmadd(cpu_operator_cost * num_cols, input_tuples, total_cost);
    fmadd(cpu_tuple_cost, input_tuples, total_cost)
}

/// cost_windowagg startup bump for startup_tuples > 1 (costsize.c:3182).
pub(crate) fn windowagg_startup_cost(
    path_startup_cost: f64,
    total_cost: f64,
    startup_cost: f64,
    input_tuples: f64,
    startup_tuples: f64,
) -> f64 {
    fmadd(
        (total_cost - startup_cost) / input_tuples,
        startup_tuples - 1.0,
        path_startup_cost,
    )
}

/// final_cost_nestloop SEMI/ANTI/inner-unique run cost and ntuples
/// (costsize.c:3410-3495). Returns (run_cost, ntuples).
#[allow(clippy::too_many_arguments)]
pub(crate) fn nestloop_early_stop_costs(
    mut run_cost: f64,
    indexed_join_quals: bool,
    inner_run_cost: f64,
    inner_rescan_run_cost: f64,
    outer_path_rows: f64,
    inner_path_rows: f64,
    outer_match_frac: f64,
    match_count: f64,
) -> (f64, f64) {
    let mut outer_matched_rows = (outer_path_rows * outer_match_frac).round_ties_even();
    let mut outer_unmatched_rows = outer_path_rows - outer_matched_rows;
    let inner_scan_frac = 2.0 / (match_count + 1.0);
    let mut ntuples = outer_matched_rows * inner_path_rows * inner_scan_frac;
    if indexed_join_quals {
        run_cost = fmadd(inner_run_cost, inner_scan_frac, run_cost); // :3445
        if outer_matched_rows > 1.0 {
            run_cost = fmadd(
                (outer_matched_rows - 1.0) * inner_rescan_run_cost,
                inner_scan_frac,
                run_cost,
            ); // :3447
        }
        run_cost += outer_unmatched_rows * inner_rescan_run_cost / inner_path_rows;
    } else {
        ntuples = fmadd(outer_unmatched_rows, inner_path_rows, ntuples); // :3479
        run_cost += inner_run_cost;
        if outer_unmatched_rows >= 1.0 {
            outer_unmatched_rows -= 1.0;
        } else {
            outer_matched_rows -= 1.0;
        }
        if outer_matched_rows > 0.0 {
            run_cost = fmadd(
                outer_matched_rows * inner_rescan_run_cost,
                inner_scan_frac,
                run_cost,
            ); // :3490
        }
        if outer_unmatched_rows > 0.0 {
            run_cost = fmadd(outer_unmatched_rows, inner_rescan_run_cost, run_cost); // :3494
        }
    }
    (run_cost, ntuples)
}

/// The per-tuple tail every join costing ends with: qual eval over the
/// join tuples, then the tlist over the output rows (costsize.c:3509/3513
/// nestloop, 4067/4071 mergejoin, 4515/4519 hashjoin).
pub(crate) fn join_tail_run_cost(
    run_cost: f64,
    cpu_per_tuple: f64,
    ntuples: f64,
    tlist_per_tuple: f64,
    rows: f64,
) -> f64 {
    fmadd(tlist_per_tuple, rows, fmadd(cpu_per_tuple, ntuples, run_cost))
}

/// initial_cost_mergejoin startup share of one input's run cost
/// (costsize.c:3730/3739 outer, 3771/3780 inner). The run-cost shares at
/// 3732/3741 fuse with a 0.0 addend, so they round like plain code.
pub(crate) fn mergejoin_startup_share(startup_cost: f64, input_run_cost: f64, startsel: f64) -> f64 {
    fmadd(input_run_cost, startsel, startup_cost)
}

/// final_cost_mergejoin materialized-inner cost (costsize.c:3981).
pub(crate) fn mergejoin_mat_inner_cost(
    inner_run_cost: f64,
    cpu_operator_cost: f64,
    inner_rows: f64,
    rescanratio: f64,
) -> f64 {
    fmadd(cpu_operator_cost * inner_rows, rescanratio, inner_run_cost)
}

/// final_cost_mergejoin merge-qual charges (costsize.c:4050-4054): the
/// inner products fuse, then the outer ones. Returns (startup, run).
#[allow(clippy::too_many_arguments)]
pub(crate) fn mergejoin_qual_costs(
    startup_cost: f64,
    run_cost: f64,
    merge_qual_per_tuple: f64,
    outer_rows: f64,
    outer_skip_rows: f64,
    inner_rows: f64,
    inner_skip_rows: f64,
    rescanratio: f64,
) -> (f64, f64) {
    (
        fmadd(
            merge_qual_per_tuple,
            fmadd(inner_skip_rows, rescanratio, outer_skip_rows),
            startup_cost,
        ),
        fmadd(
            merge_qual_per_tuple,
            fmadd(inner_rows - inner_skip_rows, rescanratio, outer_rows - outer_skip_rows),
            run_cost,
        ),
    )
}

/// initial_cost_hashjoin hash build/probe charges (costsize.c:4199/4201):
/// cop * num_hashclauses is shared (CSE) so it rounds; each outer multiply
/// fuses. Returns (startup, run).
pub(crate) fn hashjoin_initial_costs(
    startup_cost: f64,
    run_cost: f64,
    cpu_operator_cost: f64,
    num_hashclauses: f64,
    cpu_tuple_cost: f64,
    inner_path_rows: f64,
    outer_path_rows: f64,
) -> (f64, f64) {
    let per_clause = cpu_operator_cost * num_hashclauses;
    (
        fmadd(per_clause + cpu_tuple_cost, inner_path_rows, startup_cost),
        fmadd(per_clause, outer_path_rows, run_cost),
    )
}

/// initial_cost_hashjoin batch I/O (costsize.c:4246/4247); `2 * outerpages`
/// is exact. Returns (startup, run).
pub(crate) fn hashjoin_batch_io_costs(
    startup_cost: f64,
    run_cost: f64,
    seq_page_cost: f64,
    innerpages: f64,
    outerpages: f64,
) -> (f64, f64) {
    (
        fmadd(seq_page_cost, innerpages, startup_cost),
        fmadd(seq_page_cost, innerpages + 2.0 * outerpages, run_cost),
    )
}

/// final_cost_hashjoin unmatched-outer bucket charge (costsize.c:4473).
pub(crate) fn hashjoin_unmatched_cost(
    run_cost: f64,
    hash_qual_per_tuple: f64,
    outer_unmatched_rows: f64,
    bucket_rows: f64,
) -> f64 {
    fmadd(hash_qual_per_tuple * outer_unmatched_rows * bucket_rows, 0.05, run_cost)
}

/// cost_qual_eval_walker hashed ScalarArrayOpExpr build charge (costsize.c:4904).
pub(crate) fn saop_hash_startup(startup: f64, estarraylen: f64, hash_per_tuple: f64) -> f64 {
    fmadd(estarraylen, hash_per_tuple, startup)
}

/// cost_qual_eval_walker ArrayCoerceExpr per-element charge (costsize.c:4976).
pub(crate) fn array_coerce_per_tuple(per_tuple: f64, elem_per_tuple: f64, nelems: f64) -> f64 {
    fmadd(elem_per_tuple, nelems, per_tuple)
}

/// calc_joinrel_size_estimate JOIN_ANTI rows (costsize.c:5622-5623).
pub(crate) fn anti_join_rows(outer_rows: f64, fkselec: f64, jselec: f64, pselec: f64) -> f64 {
    outer_rows * fmsub(fkselec, jselec, 1.0) * pselec
}
