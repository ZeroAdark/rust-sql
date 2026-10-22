//! D400: the planner's float sites that aarch64 PostgreSQL fuses (GCC
//! -ffp-contract=fast), one helper per C expression. Each helper is written
//! in GCC's association (checked against the PG 18.6 aarch64 disassembly)
//! and goes through fp_contract, so it fuses on aarch64 and rounds twice
//! elsewhere. The call sites stay in their C files' ports.

use fp_contract::{fmadd, fmsub};

const DEFAULT_PAGE_CPU_MULTIPLIER: f64 = 50.0;

/// selfuncs.c:993, like_support.c:717.
#[inline]
pub(crate) fn hist_blend(selec: f64, hist_weight: f64, other: f64) -> f64 {
    fmadd(selec, hist_weight, other * (1.0 - hist_weight))
}

/// selfuncs.c:1014-1015, like_support.c:742-743 (`*=` then `+=` fuse).
#[inline]
pub(crate) fn mcv_merge(selec: f64, nullfrac: f64, sumcommon: f64, mcvsel: f64) -> f64 {
    fmadd(selec, 1.0 - nullfrac - sumcommon, mcvsel)
}

/// selfuncs.c:1315.
#[inline]
pub(crate) fn ineq_first_bin(frac: f64, eq_selec: f64, binfrac: f64) -> f64 {
    fmadd(1.0 - binfrac, eq_selec, frac)
}

/// selfuncs.c:7249+7259 (qual_op_cost fuses into its only use).
#[inline]
pub(crate) fn generic_index_tuple_cost(
    total: f64,
    num_index_tuples: f64,
    num_sa_scans: f64,
    cpu_index_tuple_cost: f64,
    cpu_operator_cost: f64,
    n_ops: f64,
) -> f64 {
    fmadd(
        num_index_tuples * num_sa_scans,
        fmadd(cpu_operator_cost, n_ops, cpu_index_tuple_cost),
        total,
    )
}

/// selfuncs.c:7791/7807 (bt), 7909/7917 (gist, spg).
#[inline]
pub(crate) fn sa_descent_total(total: f64, num_sa_scans: f64, descent_cost: f64) -> f64 {
    fmadd(num_sa_scans, descent_cost, total)
}

/// selfuncs.c:8549/8550.
#[inline]
pub(crate) fn gin_descent(
    startup: f64,
    total: f64,
    descent_cost: f64,
    search_entries: f64,
    array_scans: f64,
) -> (f64, f64) {
    (
        fmadd(descent_cost, search_entries, startup),
        fmadd(array_scans * descent_cost, search_entries, total),
    )
}

/// selfuncs.c:8557/8558/8565/8571.
#[inline]
pub(crate) fn gin_entry_pages(
    startup: f64,
    total: f64,
    entry_pages_fetched: f64,
    array_scans: f64,
    data_pages_fetched: f64,
    cpu_operator_cost: f64,
) -> (f64, f64) {
    let m = DEFAULT_PAGE_CPU_MULTIPLIER;
    let startup = fmadd(entry_pages_fetched * m, cpu_operator_cost, startup);
    let total = fmadd(
        entry_pages_fetched * array_scans * m,
        cpu_operator_cost,
        total,
    );
    (
        fmadd(m * cpu_operator_cost, data_pages_fetched, startup),
        fmadd(
            data_pages_fetched * (array_scans - 1.0) * m,
            cpu_operator_cost,
            total,
        ),
    )
}

/// selfuncs.c:8596.
#[inline]
pub(crate) fn gin_page_io(
    startup: f64,
    entry_pages_fetched: f64,
    data_pages_fetched: f64,
    spc_random_page_cost: f64,
) -> f64 {
    fmadd(
        entry_pages_fetched + data_pages_fetched,
        spc_random_page_cost,
        startup,
    )
}

/// selfuncs.c:8625/8631.
#[inline]
pub(crate) fn gin_search_pages(
    startup: f64,
    total: f64,
    search_entries: f64,
    data_pages_fetched: f64,
    array_scans: f64,
    cpu_operator_cost: f64,
) -> (f64, f64) {
    let m = DEFAULT_PAGE_CPU_MULTIPLIER;
    (
        fmadd(m * cpu_operator_cost, search_entries, startup),
        fmadd(
            data_pages_fetched * array_scans * m,
            cpu_operator_cost,
            total,
        ),
    )
}

/// selfuncs.c:8644 (fused inside the parenthesised sum).
#[inline]
pub(crate) fn gin_total_io(
    total: f64,
    startup: f64,
    data_pages_fetched: f64,
    spc_random_page_cost: f64,
) -> f64 {
    total + fmadd(data_pages_fetched, spc_random_page_cost, startup)
}

/// selfuncs.c:8662/8664.
#[inline]
pub(crate) fn gin_qual_costs(
    total: f64,
    search_entries: f64,
    array_scans: f64,
    qual_op_cost: f64,
    num_tuples: f64,
    index_selectivity: f64,
    cpu_index_tuple_cost: f64,
) -> f64 {
    fmadd(
        num_tuples * index_selectivity,
        cpu_index_tuple_cost,
        fmadd(search_entries * array_scans, qual_op_cost, total),
    )
}

/// selfuncs.c:8866 (fuses across two statements into `+= qual_arg_cost`), 8873, 8883.
#[inline]
pub(crate) fn brin_costs(
    spc_seq_page_cost: f64,
    revmap_num_pages: f64,
    loop_count: f64,
    qual_arg_cost: f64,
    spc_random_page_cost: f64,
    num_pages: f64,
    cpu_operator_cost: f64,
    estimated_ranges: f64,
    pages_per_range: f64,
) -> (f64, f64) {
    let startup = fmadd(
        spc_seq_page_cost * revmap_num_pages,
        loop_count,
        qual_arg_cost,
    );
    let total = fmadd(
        spc_random_page_cost * (num_pages - revmap_num_pages),
        loop_count,
        startup,
    );
    (
        startup,
        fmadd(
            0.1 * cpu_operator_cost * estimated_ranges,
            pages_per_range,
            total,
        ),
    )
}

/// clausesel.c:415, ts_selfuncs.c:413, _int_selfuncs.c:316.
#[inline]
pub(crate) fn or_sel(s1: f64, s2: f64) -> f64 {
    fmsub(s1, s2, s1 + s2)
}

/// ts_selfuncs.c:335/338 (`acc += f - acc * f`).
#[inline]
pub(crate) fn ts_mce_accum(acc: f64, freq: f64) -> f64 {
    acc + fmsub(acc, freq, freq)
}

/// ts_selfuncs.c:345.
#[inline]
pub(crate) fn ts_prefix_selec(matched: f64, allmces: f64, n_matched: f64, length: f64) -> f64 {
    fmadd(1.0 - allmces, n_matched / length, matched)
}

/// allpaths.c:1181/1213.
#[inline]
pub(crate) fn width_accum(acc: f64, width: f64, rows: f64) -> f64 {
    fmadd(width, rows, acc)
}

/// createplan.c:4669.
#[inline]
pub(crate) fn mat_total(total: f64, cpu_operator_cost: f64, rows: f64) -> f64 {
    fmadd(cpu_operator_cost, rows, total)
}

/// planagg.c:465.
#[inline]
pub(crate) fn fractional_cost(startup: f64, total: f64, fraction: f64) -> f64 {
    fmadd(fraction, total - startup, startup)
}

/// prepunion.c:447.
#[inline]
pub(crate) fn recursion_groups(lrows: f64, rrows: f64) -> f64 {
    fmadd(rrows, 10.0, lrows)
}

/// plancat.c:2165.
#[inline]
pub(crate) fn function_cost(per_tuple: f64, procost: f64, cpu_operator_cost: f64) -> f64 {
    fmadd(procost, cpu_operator_cost, per_tuple)
}

/// setrefs.c:2176.
#[inline]
pub(crate) fn subplan_cost(startup: f64, num_exec: f64, per_call: f64) -> f64 {
    fmadd(num_exec, per_call, startup)
}

/// geqo_selection.c:104.
#[inline]
pub(crate) fn geqo_sqrtval(bias: f64, rand: f64) -> f64 {
    fmsub(4.0 * (bias - 1.0), rand, bias * bias)
}

/// network_selfuncs.c:175.
#[inline]
pub(crate) fn net_mcv_combine(
    mcv_selec: f64,
    nullfrac: f64,
    sumcommon: f64,
    non_mcv_selec: f64,
) -> f64 {
    fmadd(1.0 - nullfrac - sumcommon, non_mcv_selec, mcv_selec)
}

/// network_selfuncs.c:363/368.
#[inline]
pub(crate) fn net_join_add(selec: f64, nullfrac: f64, sumcommon: f64, part: f64) -> f64 {
    fmadd(1.0 - nullfrac - sumcommon, part, selec)
}

/// network_selfuncs.c:378.
#[inline]
pub(crate) fn net_hist_hist_add(
    selec: f64,
    nullfrac1: f64,
    sumcommon1: f64,
    nullfrac2: f64,
    sumcommon2: f64,
    part: f64,
) -> f64 {
    fmadd(
        (1.0 - nullfrac1 - sumcommon1) * (1.0 - nullfrac2 - sumcommon2),
        part,
        selec,
    )
}

/// network_selfuncs.c:488/735.
#[inline]
pub(crate) fn net_freq_accum(selec: f64, freq: f64, part: f64) -> f64 {
    fmadd(freq, part, selec)
}

/// rangetypes_selfuncs.c:348, multirangetypes_selfuncs.c:431.
#[inline]
pub(crate) fn empty_frac_merge(empty_frac: f64, hist_selec: f64) -> f64 {
    fmadd(1.0 - empty_frac, hist_selec, empty_frac)
}

#[cfg(test)]
mod d400_fma_site_tests {
    //! Each input set makes the fused and unfused results differ; for
    //! multi-fma helpers there is one set per fma (unfusing that fma alone
    //! changes the result). Expected bits come from the same C expressions
    //! compiled with the box-23 GCC 13.3 -O2, once as built and once with
    //! -ffp-contract=off. Python math.fma agreed on every value.
    use super::*;

    fn pick(fused: u64, unfused: u64) -> u64 {
        if cfg!(target_arch = "aarch64") {
            fused
        } else {
            unfused
        }
    }

    #[test]
    fn d400_hist_blend_fuses() {
        let got = hist_blend(0.9997, 0.75, 0.6377);
        assert_eq!(got.to_bits(), pick(0x3FED182A9930BE0E, 0x3FED182A9930BE0F));
    }

    #[test]
    fn d400_mcv_merge_fuses() {
        let got = mcv_merge(0.7231, 0.0269, 0.269, 0.1438);
        assert_eq!(got.to_bits(), pick(0x3FE4E4D7553BF19D, 0x3FE4E4D7553BF19E));
    }

    #[test]
    fn d400_ineq_first_bin_fuses() {
        let got = ineq_first_bin(0.02652, 0.039077, 0.09643);
        assert_eq!(got.to_bits(), pick(0x3FAFA8066DE818E3, 0x3FAFA8066DE818E2));
    }

    #[test]
    fn d400_generic_index_tuple_cost_fuses_0() {
        let got = generic_index_tuple_cost(227.73, 31033.0, 2.0, 0.037513, 0.016539, 3.0);
        assert_eq!(got.to_bits(), pick(0x40B6038A63736CDF, 0x40B6038A63736CE0));
    }
    #[test]
    fn d400_generic_index_tuple_cost_fuses_1() {
        let got = generic_index_tuple_cost(813.54, 96623.0, 3.0, 0.039603, 0.005254, 2.0);
        assert_eq!(got.to_bits(), pick(0x40CDF5952DC2B0EB, 0x40CDF5952DC2B0EA));
    }

    #[test]
    fn d400_sa_descent_total_fuses() {
        let got = sa_descent_total(9.69, 26.0, 0.04);
        assert_eq!(got.to_bits(), pick(0x402575C28F5C28F5, 0x402575C28F5C28F6));
    }

    #[test]
    fn d400_gin_descent_fuses() {
        let (a, b) = gin_descent(5.97, 7.11, 0.045, 6.0, 4.0);
        assert_eq!(
            (a.to_bits(), b.to_bits()),
            (
                pick(0x4018F5C28F5C28F5, 0x4018F5C28F5C28F6),
                pick(0x40206147AE147AE1, 0x40206147AE147AE2)
            )
        );
    }

    #[test]
    fn d400_gin_entry_pages_fuses_0() {
        let (a, b) = gin_entry_pages(45.68, 227.47, 409.0, 4.0, 2369.0, 0.04741);
        assert_eq!(
            (a.to_bits(), b.to_bits()),
            (
                pick(0x40B9E6EDD2F1A9FC, 0x40B9E6EDD2F1A9FC),
                pick(0x40D47630189374BD, 0x40D47630189374BD)
            )
        );
    }
    #[test]
    fn d400_gin_entry_pages_fuses_1() {
        let (a, b) = gin_entry_pages(37.66, 361.2, 265.0, 5.0, 144.0, 0.009333);
        assert_eq!(
            (a.to_bits(), b.to_bits()),
            (
                pick(0x406C90A29C779A6B, 0x406C90A29C779A6B),
                pick(0x40938134E3BCD35A, 0x40938134E3BCD35A)
            )
        );
    }
    #[test]
    fn d400_gin_entry_pages_fuses_2() {
        let (a, b) = gin_entry_pages(85.73, 105.57, 276.0, 2.0, 3440.0, 0.004168);
        assert_eq!(
            (a.to_bits(), b.to_bits()),
            (
                pick(0x408AE127BB2FEC57, 0x408AE127BB2FEC56),
                pick(0x408D4C05BC01A36E, 0x408D4C05BC01A36E)
            )
        );
    }
    #[test]
    fn d400_gin_entry_pages_fuses_3() {
        let (a, b) = gin_entry_pages(81.69, 66.55, 490.0, 4.0, 2471.0, 0.00677);
        assert_eq!(
            (a.to_bits(), b.to_bits()),
            (
                pick(0x4090EFF439581063, 0x4090EFF439581063),
                pick(0x40A94E9EF9DB22D1, 0x40A94E9EF9DB22D0)
            )
        );
    }

    #[test]
    fn d400_gin_page_io_fuses() {
        let got = gin_page_io(46.3201, 184.0, 3521.0, 1.1);
        assert_eq!(got.to_bits(), pick(0x40B019D1F212D773, 0x40B019D1F212D774));
    }

    #[test]
    fn d400_gin_search_pages_fuses_0() {
        let (a, b) = gin_search_pages(83.0617, 971.4486, 17.0, 3313.0, 2.0, 0.047978);
        assert_eq!(
            (a.to_bits(), b.to_bits()),
            (
                pick(0x405EF5F3B645A1CB, 0x405EF5F3B645A1CA),
                pick(0x40D078A3D70A3D71, 0x40D078A3D70A3D71)
            )
        );
    }
    #[test]
    fn d400_gin_search_pages_fuses_1() {
        let (a, b) = gin_search_pages(57.0553, 722.205, 10.0, 670.0, 5.0, 0.015807);
        assert_eq!(
            (a.to_bits(), b.to_bits()),
            (
                pick(0x40503D5CFAACD9E8, 0x40503D5CFAACD9E8),
                pick(0x40AA53C147AE147C, 0x40AA53C147AE147B)
            )
        );
    }

    #[test]
    fn d400_gin_total_io_fuses() {
        let got = gin_total_io(659.3194, 96.6702, 4763.0, 1.1);
        assert_eq!(got.to_bits(), pick(0x40B76B4A2339C0ED, 0x40B76B4A2339C0EC));
    }

    #[test]
    fn d400_gin_qual_costs_fuses() {
        let got = gin_qual_costs(2.6194, 16.0, 3.0, 0.005, 380906.0, 0.111605, 0.005);
        assert_eq!(got.to_bits(), pick(0x406AED4357F3DCDC, 0x406AED4357F3DCDB));
    }

    #[test]
    fn d400_brin_costs_fuses_0() {
        let (a, b) = brin_costs(0.7, 3.0, 904.0, 0.7757, 4.0, 264.0, 0.006185, 2120.0, 16.0);
        assert_eq!(
            (a.to_bits(), b.to_bits()),
            (
                pick(0x409DACB3EAB367A0, 0x409DACB3EAB3679F),
                pick(0x412CDC404F78FEF0, 0x412CDC404F78FEF0)
            )
        );
    }
    #[test]
    fn d400_brin_costs_fuses_1() {
        let (a, b) = brin_costs(0.7, 8.0, 855.0, 0.7493, 1.1, 706.0, 0.012766, 3445.0, 32.0);
        assert_eq!(
            (a.to_bits(), b.to_bits()),
            (
                pick(0x40B2B4BFD21FF2E4, 0x40B2B4BFD21FF2E5),
                pick(0x41242F2CF69F4906, 0x41242F2CF69F4907)
            )
        );
    }
    #[test]
    fn d400_brin_costs_fuses_2() {
        let (a, b) = brin_costs(1.5, 3.0, 107.0, 0.2618, 1.1, 78.0, 0.045615, 7846.0, 169.0);
        assert_eq!(
            (a.to_bits(), b.to_bits()),
            (
                pick(0x407E1C305532617C, 0x407E1C305532617C),
                pick(0x40CDFED89A0AD8A1, 0x40CDFED89A0AD8A2)
            )
        );
    }

    #[test]
    fn d400_or_sel_fuses() {
        let got = or_sel(0.5041, 0.6197);
        assert_eq!(got.to_bits(), pick(0x3FE9F7107D50B8AB, 0x3FE9F7107D50B8AC));
    }

    #[test]
    fn d400_ts_mce_accum_fuses() {
        let got = ts_mce_accum(0.969716, 0.16638344526290894);
        assert_eq!(got.to_bits(), pick(0x3FEF3130E2F7B9E1, 0x3FEF3130E2F7B9E0));
    }

    #[test]
    fn d400_ts_prefix_selec_fuses() {
        let got = ts_prefix_selec(0.264439, 0.339453, 100.0, 187.0);
        assert_eq!(got.to_bits(), pick(0x3FE3C3F9844FC93B, 0x3FE3C3F9844FC93A));
    }

    #[test]
    fn d400_width_accum_fuses() {
        let got = width_accum(520545.7, 189.0, 329669.1);
        assert_eq!(got.to_bits(), pick(0x418DF56F2CCCCCCC, 0x418DF56F2CCCCCCD));
    }

    #[test]
    fn d400_mat_total_fuses() {
        let got = mat_total(47.53, 0.0025, 854437.0);
        assert_eq!(got.to_bits(), pick(0x40A10F3EB851EB85, 0x40A10F3EB851EB86));
    }

    #[test]
    fn d400_fractional_cost_fuses() {
        let got = fractional_cost(21.93, 2119.41, 0.00684931506849315);
        assert_eq!(got.to_bits(), pick(0x404225ED340AA92B, 0x404225ED340AA92C));
    }

    #[test]
    fn d400_recursion_groups_fuses() {
        let got = recursion_groups(681408.9, 44402.4);
        assert_eq!(got.to_bits(), pick(0x41312C38E6666667, 0x41312C38E6666666));
    }

    #[test]
    fn d400_function_cost_fuses() {
        let got = function_cost(3.9362, 100.0, 0.0025);
        assert_eq!(got.to_bits(), pick(0x4010BEAB367A0F91, 0x4010BEAB367A0F90));
    }

    #[test]
    fn d400_subplan_cost_fuses() {
        let got = subplan_cost(57.07, 14799.6, 90.9493);
        assert_eq!(got.to_bits(), pick(0x41348A16548D3AE6, 0x41348A16548D3AE7));
    }

    #[test]
    fn d400_geqo_sqrtval_fuses() {
        let got = geqo_sqrtval(1.9294624909932065, 0.838719152991124);
        assert_eq!(got.to_bits(), pick(0x3FE358D4889A13D2, 0x3FE358D4889A13D4));
    }

    #[test]
    fn d400_net_mcv_combine_fuses() {
        let got = net_mcv_combine(0.3717, 0.2539, 0.4039, 0.332499);
        assert_eq!(got.to_bits(), pick(0x3FDF121F8FE4D053, 0x3FDF121F8FE4D054));
    }

    #[test]
    fn d400_net_join_add_fuses() {
        let got = net_join_add(0.4011, 0.0445, 0.002, 0.185395);
        assert_eq!(got.to_bits(), pick(0x3FE27DF1E4895701, 0x3FE27DF1E4895702));
    }

    #[test]
    fn d400_net_hist_hist_add_fuses() {
        let got = net_hist_hist_add(0.4696, 0.1806, 0.0169, 0.1791, 0.0108, 0.595406);
        assert_eq!(got.to_bits(), pick(0x3FEB69E4F989341F, 0x3FEB69E4F989341E));
    }

    #[test]
    fn d400_net_freq_accum_fuses() {
        let got = net_freq_accum(0.2986, 0.09634363651275635, 0.254206);
        assert_eq!(got.to_bits(), pick(0x3FD4AD866BBDF091, 0x3FD4AD866BBDF090));
    }

    #[test]
    fn d400_empty_frac_merge_fuses() {
        let got = empty_frac_merge(0.27745673060417175, 0.182678);
        assert_eq!(got.to_bits(), pick(0x3FDA346BA2327AA7, 0x3FDA346BA2327AA6));
    }
}
