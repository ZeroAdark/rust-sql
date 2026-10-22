//! D400: every costsize.c site that GCC 13.3 -O2 (-ffp-contract=fast)
//! contracts into an fmadd/fmsub on aarch64, pinned bit-exactly. Inputs
//! and expected bits come from a C harness that compiles the costsize.c
//! statement shapes verbatim with the PGDG flags (truth) and with
//! -ffp-contract=off; each row was chosen so that unfusing any single
//! site covered by the case changes an output. aarch64 expects the fused
//! bits, every other target the unfused ones.

use super::*;
use types_pathnodes::{
    AggClauseCosts, AppendPath, BitmapAndPath, BitmapOrPath, GatherMergePath, GatherPath,
    IndexPath, Path, PathTarget, RecursiveUnionPath, RelOptInfo,
};

fn install_seams_once() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        if !guc_tables::vars::work_mem.installed() {
            init_small::init_seams();
        }
        guc_tables::vars::parallel_leader_participation.install_if_absent(
            guc_tables::GucVarAccessors {
                get: gucs::parallel_leader_participation_backing,
                set: gucs::set_parallel_leader_participation_backing,
            },
        );
    });
}

/// The harness GUCs: C defaults except where a case needs an inexact
/// page cost or operator cost for its product to round.
fn gucs(seq_page_cost: f64, random_page_cost: f64, cpu_operator_cost: f64) {
    install_seams_once();
    gucs::set_seq_page_cost(seq_page_cost);
    gucs::set_random_page_cost(random_page_cost);
    gucs::set_cpu_operator_cost(cpu_operator_cost);
    gucs::set_cpu_tuple_cost(0.01);
    gucs::set_parallel_tuple_cost(0.1);
    gucs::set_parallel_setup_cost(1000.0);
}

fn check(name: &str, got: &[f64], fused: &[u64], plain: &[u64]) {
    let want = if fp_contract::FUSED { fused } else { plain };
    let got_bits: Vec<u64> = got.iter().map(|x| x.to_bits()).collect();
    assert_eq!(
        got_bits,
        want,
        "{name}: got {got:?}, want {:?}",
        want.iter().map(|b| f64::from_bits(*b)).collect::<Vec<_>>()
    );
}

fn path<'m>(mcx: mcx::Mcx<'m>, rel: RelId, startup: f64, total: f64, rows: f64) -> Path<'m> {
    Path {
        type_: 0,
        pathtype: 0,
        parent: rel,
        pathtarget_id: None,
        param_info: None,
        parallel_aware: false,
        parallel_safe: true,
        parallel_workers: 0,
        rows,
        disabled_nodes: 0,
        startup_cost: startup,
        total_cost: total,
        pathkeys: PgVec::new_in(mcx),
    }
}

fn target<'m>(run: &mut PlannerRun<'m>, startup: f64, per_tuple: f64, width: i32) -> types_pathnodes::PtId {
    let mut t = PathTarget::new(run.mcx);
    t.cost = QualCost { startup, per_tuple };
    t.width = width;
    run.root.alloc_pathtarget(t)
}

fn index_path<'m>(mcx: mcx::Mcx<'m>, rel: RelId, indextotalcost: f64, rows: f64) -> PathNode<'m> {
    PathNode::IndexPath(IndexPath {
        path: path(mcx, rel, 0.0, 0.0, rows),
        indexinfo: None,
        indexclauses: PgVec::new_in(mcx),
        indexorderbys: PgVec::new_in(mcx),
        indexorderbycols: PgVec::new_in(mcx),
        indexscandir: types_pathnodes::ForwardScanDirection,
        indextotalcost,
        indexselectivity: 0.5,
    })
}

fn startup_total(run: &PlannerRun<'_>, id: PathId) -> Vec<f64> {
    let p = run.root.path(id).base();
    vec![p.startup_cost, p.total_cost]
}

// A base scan rel (relid 1) and a path over it carrying the tlist cost.
// in: tuples rows qp_startup qp_pt pt_startup pt_pt (pages separate).
fn scan_fixture<'m>(
    mcx: mcx::Mcx<'m>,
    rtekind: types_pathnodes::RTEKind,
    pages: f64,
    i: &[f64],
) -> (PlannerRun<'m>, RelId, PathId) {
    let mut run = PlannerRun::new(mcx);
    let mut rel = RelOptInfo::new(mcx);
    rel.relid = 1;
    rel.rtekind = rtekind;
    rel.pages = pages as _;
    rel.tuples = i[0];
    rel.rows = i[1];
    rel.baserestrictcost = QualCost { startup: i[2], per_tuple: i[3] };
    let rid = run.root.alloc_rel(rel);
    let pt = target(&mut run, i[4], i[5], 0);
    let mut p = path(mcx, rid, 0.0, 0.0, 0.0);
    p.pathtarget_id = Some(pt);
    let id = run.root.alloc_path(PathNode::Path(p));
    (run, rid, id)
}

// ---- whole-function fixtures ----------------------------------------------

// costsize.c:467. in: sub_startup sub_total rows
fn eval_gather(i: &[f64]) -> Vec<f64> {
    let cx = mcx::MemoryContext::new_bump("d400");
    let mcx = cx.mcx();
    let mut run = PlannerRun::new(mcx);
    let rid = run.root.alloc_rel(RelOptInfo::new(mcx));
    let sub = run.root.alloc_path(PathNode::Path(path(mcx, rid, i[0], i[1], 0.0)));
    let g = run.root.alloc_path(PathNode::GatherPath(GatherPath {
        path: path(mcx, rid, 0.0, 0.0, 0.0),
        subpath: Some(sub),
        single_copy: false,
        num_workers: 2,
    }));
    cost_gather(&mut run, g, rid, Some(i[2]));
    startup_total(&run, g)
}

// costsize.c:524/533. in: num_workers rows input_startup input_total
fn eval_gather_merge(i: &[f64]) -> Vec<f64> {
    let cx = mcx::MemoryContext::new_bump("d400");
    let mcx = cx.mcx();
    let mut run = PlannerRun::new(mcx);
    let rid = run.root.alloc_rel(RelOptInfo::new(mcx));
    let g = run.root.alloc_path(PathNode::GatherMergePath(GatherMergePath {
        path: path(mcx, rid, 0.0, 0.0, 0.0),
        subpath: None,
        num_workers: i[0] as i32,
    }));
    cost_gather_merge(&mut run, g, rid, 0, i[2], i[3], Some(i[1]));
    startup_total(&run, g)
}

// costsize.c:331. in: pages tuples rows qp_startup qp_pt pt_startup pt_pt workers(0)
fn eval_seqscan(i: &[f64]) -> Vec<f64> {
    let cx = mcx::MemoryContext::new_bump("d400");
    let mcx = cx.mcx();
    let (mut run, rid, id) =
        scan_fixture(mcx, RTE_RELATION, i[0], &[i[1], i[2], i[3], i[4], i[5], i[6]]);
    cost_seqscan(&mut run, id, rid).unwrap();
    let mut v = startup_total(&run, id);
    v.push(run.root.path(id).base().rows);
    v
}

// costsize.c:1686. in: tuples rows qp_startup qp_pt pt_startup pt_pt
fn eval_valuesscan(i: &[f64]) -> Vec<f64> {
    let cx = mcx::MemoryContext::new_bump("d400");
    let mcx = cx.mcx();
    let (mut run, rid, id) = scan_fixture(mcx, types_pathnodes::RTE_VALUES, 0.0, i);
    cost_valuesscan(&mut run, id, rid).unwrap();
    startup_total(&run, id)
}

// costsize.c:1734. in: tuples rows qp_startup qp_pt pt_startup pt_pt
fn eval_ctescan(i: &[f64]) -> Vec<f64> {
    let cx = mcx::MemoryContext::new_bump("d400");
    let mcx = cx.mcx();
    let (mut run, rid, id) = scan_fixture(mcx, types_pathnodes::RTE_CTE, 0.0, i);
    cost_ctescan(&mut run, id, rid).unwrap();
    startup_total(&run, id)
}

fn side_path(run: &mut PlannerRun<'_>, rid: RelId, startup: f64, total: f64, rows: f64) -> PathId {
    let pt = target(run, 0.0, 0.0, 8);
    let mut p = path(run.mcx, rid, startup, total, rows);
    p.pathtarget_id = Some(pt);
    run.root.alloc_path(PathNode::Path(p))
}

// costsize.c:1843/1844/1851. in: n_startup n_total n_rows r_total r_rows
fn eval_recursive_union(i: &[f64]) -> Vec<f64> {
    let cx = mcx::MemoryContext::new_bump("d400");
    let mcx = cx.mcx();
    let mut run = PlannerRun::new(mcx);
    let rid = run.root.alloc_rel(RelOptInfo::new(mcx));
    let n = side_path(&mut run, rid, i[0], i[1], i[2]);
    let r = side_path(&mut run, rid, 0.0, i[3], i[4]);
    let pt = target(&mut run, 0.0, 0.0, 0);
    let mut p = path(mcx, rid, 0.0, 0.0, 0.0);
    p.pathtarget_id = Some(pt);
    let u = run.root.alloc_path(PathNode::RecursiveUnionPath(RecursiveUnionPath {
        path: p,
        leftpath: Some(n),
        rightpath: Some(r),
        distinctList: PgVec::new_in(mcx),
        wtParam: 0,
        numGroups: 0.0,
    }));
    cost_recursive_union(&mut run, u, n, r);
    let mut v = startup_total(&run, u);
    v.push(run.root.path(u).base().rows);
    v
}

// costsize.c:1135. in: indextotalcost rows
fn eval_bitmap_tree(i: &[f64]) -> Vec<f64> {
    let cx = mcx::MemoryContext::new_bump("d400");
    let mcx = cx.mcx();
    let mut run = PlannerRun::new(mcx);
    let rid = run.root.alloc_rel(RelOptInfo::new(mcx));
    let ip = run.root.alloc_path(index_path(mcx, rid, i[0], i[1]));
    vec![cost_bitmap_tree_node(&run, ip).0]
}

// costsize.c:1135 x2 + 1194. in: itc1 rows1 itc2 rows2
fn eval_bitmap_and(i: &[f64]) -> Vec<f64> {
    let cx = mcx::MemoryContext::new_bump("d400");
    let mcx = cx.mcx();
    let mut run = PlannerRun::new(mcx);
    let rid = run.root.alloc_rel(RelOptInfo::new(mcx));
    let mut quals = PgVec::new_in(mcx);
    quals.push(run.root.alloc_path(index_path(mcx, rid, i[0], i[1])));
    quals.push(run.root.alloc_path(index_path(mcx, rid, i[2], i[3])));
    let and = run.root.alloc_path(PathNode::BitmapAndPath(BitmapAndPath {
        path: path(mcx, rid, 0.0, 0.0, 0.0),
        bitmapquals: quals,
        bitmapselectivity: 0.0,
    }));
    cost_bitmap_and_node(&mut run, and);
    vec![run.root.path(and).base().total_cost]
}

// costsize.c:1135 + 1241. in: itc1 rows1 and_total (second arm is a BitmapAnd)
fn eval_bitmap_or(i: &[f64]) -> Vec<f64> {
    let cx = mcx::MemoryContext::new_bump("d400");
    let mcx = cx.mcx();
    let mut run = PlannerRun::new(mcx);
    let rid = run.root.alloc_rel(RelOptInfo::new(mcx));
    let mut quals = PgVec::new_in(mcx);
    quals.push(run.root.alloc_path(index_path(mcx, rid, i[0], i[1])));
    quals.push(run.root.alloc_path(PathNode::BitmapAndPath(BitmapAndPath {
        path: path(mcx, rid, i[2], i[2], 0.0),
        bitmapquals: PgVec::new_in(mcx),
        bitmapselectivity: 0.25,
    })));
    let or = run.root.alloc_path(PathNode::BitmapOrPath(BitmapOrPath {
        path: path(mcx, rid, 0.0, 0.0, 0.0),
        bitmapquals: quals,
        bitmapselectivity: 0.0,
    }));
    cost_bitmap_or_node(&mut run, or);
    vec![run.root.path(or).base().total_cost]
}

// costsize.c:1957/1958. in: tuples width comparison_cost sort_mem input_cost
fn eval_sort(i: &[f64]) -> Vec<f64> {
    let (_, startup, total) = cost_sort_shape(0, i[4], i[0], i[1] as i32, i[2], i[3] as i32, -1.0);
    vec![startup, total]
}

// costsize.c:2402 (parallel arm). in: workers, then per subpath: rows total workers
fn eval_append_parallel(i: &[f64]) -> Vec<f64> {
    let cx = mcx::MemoryContext::new_bump("d400");
    let mcx = cx.mcx();
    let mut run = PlannerRun::new(mcx);
    let rid = run.root.alloc_rel(RelOptInfo::new(mcx));
    let mut subs = PgVec::new_in(mcx);
    for k in 0..2 {
        let mut p = path(mcx, rid, 1.0, i[2 + 3 * k], i[1 + 3 * k]);
        p.parallel_workers = i[3 + 3 * k] as i32;
        subs.push(run.root.alloc_path(PathNode::Path(p)));
    }
    let mut p = path(mcx, rid, 0.0, 0.0, 0.0);
    p.parallel_aware = true;
    p.parallel_workers = i[0] as i32;
    let a = run.root.alloc_path(PathNode::AppendPath(AppendPath {
        path: p,
        subpaths: subs,
        first_partial_path: 0,
        limit_tuples: -1.0,
    }));
    cost_append(&mut run, a);
    let p = run.root.path(a).base();
    vec![p.rows, p.total_cost]
}

// costsize.c:2402 (plain arm). in: rows1 total1 rows2 total2
fn eval_append_plain(i: &[f64]) -> Vec<f64> {
    let cx = mcx::MemoryContext::new_bump("d400");
    let mcx = cx.mcx();
    let mut run = PlannerRun::new(mcx);
    let rid = run.root.alloc_rel(RelOptInfo::new(mcx));
    let mut subs = PgVec::new_in(mcx);
    for k in 0..2 {
        subs.push(run.root.alloc_path(PathNode::Path(path(mcx, rid, 0.0, i[2 * k + 1], i[2 * k]))));
    }
    let a = run.root.alloc_path(PathNode::AppendPath(AppendPath {
        path: path(mcx, rid, 0.0, 0.0, 0.0),
        subpaths: subs,
        first_partial_path: 0,
        limit_tuples: -1.0,
    }));
    cost_append(&mut run, a);
    vec![run.root.path(a).base().total_cost]
}

// costsize.c:2463. in: n_streams tuples input_startup input_total
fn eval_merge_append(i: &[f64]) -> Vec<f64> {
    let cx = mcx::MemoryContext::new_bump("d400");
    let mcx = cx.mcx();
    let mut run = PlannerRun::new(mcx);
    let rid = run.root.alloc_rel(RelOptInfo::new(mcx));
    let id = run.root.alloc_path(PathNode::Path(path(mcx, rid, 0.0, 0.0, 0.0)));
    cost_merge_append(&mut run, id, i[0] as usize, 0, i[2], i[3], i[1]);
    startup_total(&run, id)
}

// costsize.c:2507/2519. in: input_startup input_total tuples width
fn eval_material(i: &[f64]) -> Vec<f64> {
    let (_, _, startup, total) = cost_material(0, i[0], i[1], i[2], i[3] as i32);
    vec![startup, total]
}

// costsize.c:2728..2766. in: strategy trans_startup trans_pt final_startup
// final_pt input_startup input_total input_tuples numGroupCols numGroups
fn eval_agg(i: &[f64], strategy: u32) -> Vec<f64> {
    let cx = mcx::MemoryContext::new_bump("d400");
    let mcx = cx.mcx();
    let mut run = PlannerRun::new(mcx);
    let mut costs = AggClauseCosts::default();
    costs.transCost = QualCost { startup: i[1], per_tuple: i[2] };
    costs.finalCost = QualCost { startup: i[3], per_tuple: i[4] };
    let (_, _, startup, total) = cost_agg_shape(
        &mut run, strategy, &costs, i[8] as i32, i[9], &[], 0, i[5], i[6], i[7], 4,
    )
    .unwrap();
    vec![startup, total]
}
fn eval_agg_plain(i: &[f64]) -> Vec<f64> {
    eval_agg(i, types_pathnodes::AGG_PLAIN)
}
fn eval_agg_sorted(i: &[f64]) -> Vec<f64> {
    eval_agg(i, types_pathnodes::AGG_SORTED)
}
fn eval_agg_hashed(i: &[f64]) -> Vec<f64> {
    eval_agg(i, types_pathnodes::AGG_HASHED)
}

// costsize.c:3214. in: input_startup input_total input_tuples numGroupCols
fn eval_group(i: &[f64]) -> Vec<f64> {
    let cx = mcx::MemoryContext::new_bump("d400");
    let mcx = cx.mcx();
    let mut run = PlannerRun::new(mcx);
    let rid = run.root.alloc_rel(RelOptInfo::new(mcx));
    let id = run.root.alloc_path(PathNode::Path(path(mcx, rid, 0.0, 0.0, 0.0)));
    cost_group(&mut run, id, i[3] as i32, 1.0, &[], 0, i[0], i[1], i[2]).unwrap();
    startup_total(&run, id)
}

// costsize.c:3302/3327 (+ cost_rescan 4697 for a Material inner).
// in: o_startup o_total o_rows i_startup i_total i_rows i_material(0/1) i_width
fn eval_nestloop_initial(i: &[f64]) -> Vec<f64> {
    let cx = mcx::MemoryContext::new_bump("d400");
    let mcx = cx.mcx();
    let mut run = PlannerRun::new(mcx);
    let rid = run.root.alloc_rel(RelOptInfo::new(mcx));
    let outer = run.root.alloc_path(PathNode::Path(path(mcx, rid, i[0], i[1], i[2])));
    let pt = target(&mut run, 0.0, 0.0, i[7] as i32);
    let mut ip = path(mcx, rid, i[3], i[4], i[5]);
    ip.pathtarget_id = Some(pt);
    if i[6] != 0.0 {
        ip.pathtype = tag16(NodeTag::T_Material);
    }
    let inner = run.root.alloc_path(PathNode::Path(ip));
    let ws = initial_cost_nestloop(&mut run, types_pathnodes::JOIN_INNER, false, outer, inner).unwrap();
    vec![ws.startup_cost, ws.total_cost, ws.run_cost]
}
fn eval_nestloop_initial_plain(i: &[f64]) -> Vec<f64> {
    eval_nestloop_initial(i)
}

// ---- fused.rs helpers, in the harness's input order ------------------------

fn eval_samplescan(i: &[f64]) -> Vec<f64> {
    vec![fused::samplescan_run_cost(i[0], i[1], i[2], i[3], i[4], i[5])]
}
fn eval_run_from_zero(i: &[f64]) -> Vec<f64> {
    vec![fused::run_from_zero(i[0], i[1], i[2], i[3])]
}
fn eval_run_from_product(i: &[f64]) -> Vec<f64> {
    vec![fused::run_from_product(i[0], i[1], i[2], i[3])]
}
fn eval_index_min_io(i: &[f64]) -> Vec<f64> {
    vec![fused::index_min_io_cost(i[0], i[1], i[2])]
}
fn eval_index_io(i: &[f64]) -> Vec<f64> {
    vec![i[3] + fused::index_io_cost(i[0], i[1], i[2])]
}
fn eval_bitmap_cost_per_page(i: &[f64]) -> Vec<f64> {
    vec![fused::bitmap_cost_per_page(i[0], i[1], i[2], i[3])]
}
fn eval_bitmap_heap_run(i: &[f64]) -> Vec<f64> {
    vec![fused::bitmap_heap_run_cost(i[0], i[1], i[2], i[3], i[4])]
}
fn eval_bitmap_lossy(i: &[f64]) -> Vec<f64> {
    vec![fused::bitmap_lossy_tuples(i[0], i[1], i[2], i[3], i[4])]
}
fn eval_tidscan(i: &[f64]) -> Vec<f64> {
    vec![fused::tidscan_run_cost(i[0], i[1], i[2], i[3], i[4])]
}
fn eval_tidrangescan(i: &[f64]) -> Vec<f64> {
    vec![fused::tidrangescan_run_cost(i[0], i[1], i[2], i[3], i[4], i[5], i[6])]
}
fn eval_incremental_sort(i: &[f64]) -> Vec<f64> {
    vec![fused::incremental_sort_run_cost(i[0], i[1], i[2], i[3], i[4], i[5], gucs::cpu_tuple_cost())]
}
fn eval_memoize_entry(i: &[f64]) -> Vec<f64> {
    vec![fused::memoize_entry_bytes(
        i[0],
        i[1],
        crate::exec_sizing::exec_estimate_cache_entry_overhead_bytes(i[0]),
    )]
}
fn eval_memoize_costs(i: &[f64]) -> Vec<f64> {
    let (startup, total) = fused::memoize_rescan_costs(
        i[0], i[1], i[2], i[3], i[4], i[5], i[6], gucs::cpu_operator_cost(), gucs::cpu_tuple_cost(),
    );
    vec![startup, total]
}
fn eval_agg_spill(i: &[f64]) -> Vec<f64> {
    let pages_written = i[2] * i[3] * 2.0;
    let (startup, total) = fused::agg_spill_costs(
        i[0],
        i[1],
        pages_written,
        pages_written,
        i[3],
        i[4],
        gucs::random_page_cost(),
        gucs::seq_page_cost(),
        gucs::cpu_tuple_cost(),
    );
    vec![startup, total]
}
fn eval_qual(i: &[f64]) -> Vec<f64> {
    vec![i[0] + fused::qual_charge(i[1], QualCost { startup: i[2], per_tuple: i[3] })]
}
fn eval_windowagg_total(i: &[f64]) -> Vec<f64> {
    vec![fused::windowagg_total_cost(
        i[0],
        &[i[1], i[2]],
        i[3],
        i[4],
        gucs::cpu_operator_cost(),
        gucs::cpu_tuple_cost(),
    )]
}
fn eval_windowagg_startup(i: &[f64]) -> Vec<f64> {
    vec![fused::windowagg_startup_cost(i[0], i[1], i[2], i[3], i[4])]
}
fn eval_nestloop_early(i: &[f64], indexed: bool) -> Vec<f64> {
    let (run_cost, ntuples) =
        fused::nestloop_early_stop_costs(i[0], indexed, i[2], i[3], i[4], i[5], i[6], i[7]);
    vec![run_cost, ntuples]
}
fn eval_nestloop_early_idx(i: &[f64]) -> Vec<f64> {
    eval_nestloop_early(i, true)
}
fn eval_nestloop_early_noidx(i: &[f64]) -> Vec<f64> {
    eval_nestloop_early(i, false)
}
fn eval_join_tail(i: &[f64]) -> Vec<f64> {
    vec![fused::join_tail_run_cost(i[0], i[1], i[2], i[3], i[4])]
}
fn eval_merge_startup(i: &[f64]) -> Vec<f64> {
    vec![fused::mergejoin_startup_share(i[0], i[1], i[2])]
}
fn eval_merge_mat(i: &[f64]) -> Vec<f64> {
    vec![fused::mergejoin_mat_inner_cost(i[0], gucs::cpu_operator_cost(), i[1], i[2])]
}
fn eval_merge_quals(i: &[f64]) -> Vec<f64> {
    let (startup, run) = fused::mergejoin_qual_costs(i[0], i[1], i[2], i[3], i[4], i[5], i[6], i[7]);
    vec![startup, run]
}
fn eval_hash_initial(i: &[f64]) -> Vec<f64> {
    let (startup, run) = fused::hashjoin_initial_costs(
        i[0], i[1], gucs::cpu_operator_cost(), i[2], gucs::cpu_tuple_cost(), i[3], i[4],
    );
    vec![startup, run]
}
fn eval_hash_batch(i: &[f64]) -> Vec<f64> {
    let outerpages = page_size(i[2], i[3] as i32);
    let innerpages = page_size(i[4], i[5] as i32);
    let (startup, run) =
        fused::hashjoin_batch_io_costs(i[0], i[1], gucs::seq_page_cost(), innerpages, outerpages);
    vec![startup, run]
}
fn eval_hash_unmatched(i: &[f64]) -> Vec<f64> {
    vec![fused::hashjoin_unmatched_cost(i[0], i[1], i[2], i[3])]
}
fn eval_saop_hash(i: &[f64]) -> Vec<f64> {
    vec![fused::saop_hash_startup(i[0], i[1], i[2])]
}
fn eval_array_coerce(i: &[f64]) -> Vec<f64> {
    vec![fused::array_coerce_per_tuple(i[0], i[1], i[2])]
}
fn eval_anti(i: &[f64]) -> Vec<f64> {
    vec![fused::anti_join_rows(i[0], i[1], i[2], i[3])]
}

// ---- rows (generated from the harness output) -----------------------------

#[test]
fn d400_gather() {
    gucs(1.0, 4.0, 0.0025);
    check(
        "gather",
        &eval_gather(&[53.98174955907853, 310.46652637135236, 297689.0116803364]),
        &[0x409077ed4fc1a46b, 0x40de59d7884e1d17],
        &[0x409077ed4fc1a46b, 0x40de59d7884e1d18],
    );
}

#[test]
fn d400_gather_merge() {
    gucs(1.0, 4.0, 0.0025);
    check(
        "gather_merge",
        &eval_gather_merge(&[2.0, 1909727.1989198176, 6.264679112598296, 18.94942416785857]),
        &[0x408f724ec0bc2112, 0x410b0847043baee5],
        &[0x408f724ec0bc2112, 0x410b0847043baee4],
    );
}

#[test]
fn d400_seqscan() {
    gucs(1.0, 4.0, 0.0025);
    check(
        "seqscan",
        &eval_seqscan(&[731670.0, 13810534.399188014, 3432.6631552836343, 0.0086813492999773, 0.14792465701391677, 0.00030365920489919295, 0.015572158332470758, 0.0]),
        &[0x3f8266bb6d763941, 0x414638f5af812eab, 0x40aad1538916dec2],
        &[0x3f8266bb6d763941, 0x414638f5af812eac, 0x40aad1538916dec2],
    );
}

#[test]
fn d400_samplescan() {
    gucs(1.0, 4.0, 0.0025);
    check(
        "samplescan",
        &eval_samplescan(&[0.5797863697312772, 96400.0, 0.012484910321413849, 5706943.048079114, 0.006669540423044916, 75299578.86117586]),
        &[0x412334d75396c4d3],
        &[0x412334d75396c4d2],
    );
}

#[test]
fn d400_run_from_zero() {
    gucs(1.0, 4.0, 0.0025);
    check(
        "run_from_zero",
        &eval_run_from_zero(&[0.4730461919180512, 2.6155044694129392, 0.0020124064810755292, 3197827.9515201794]),
        &[0x40b92491239918c3],
        &[0x40b92491239918c2],
    );
}

#[test]
fn d400_run_from_product() {
    gucs(1.0, 4.0, 0.0025);
    check(
        "run_from_product",
        &eval_run_from_product(&[0.017408978055894628, 630.446293227522, 0.24021186663064592, 2.975088277000545]),
        &[0x40276151cc3c1975],
        &[0x40276151cc3c1974],
    );
}

#[test]
fn d400_valuesscan() {
    gucs(1.0, 4.0, 0.0025);
    check(
        "valuesscan",
        &eval_valuesscan(&[10.561052787895816, 17939.11838965782, 0.0446742479662783, 7.676187720839862e-05, 0.008097525741788385, 0.0004635762948414949]),
        &[0x3fab04e6e474c428, 0x402100e4ce7b609d],
        &[0x3fab04e6e474c428, 0x402100e4ce7b609e],
    );
}

#[test]
fn d400_ctescan() {
    gucs(1.0, 4.0, 0.0025);
    check(
        "ctescan",
        &eval_ctescan(&[215113.08950698192, 267558.73045156396, 0.001757042560343897, 0.557343495970769, 0.0013400497558832577, 0.08609632846001078]),
        &[0x3f695f12c6bf9948, 0x4101f8efc3b5b5b1],
        &[0x3f695f12c6bf9948, 0x4101f8efc3b5b5b2],
    );
}

#[test]
fn d400_index_min_io() {
    gucs(1.0, 4.0, 0.0025);
    check(
        "index_min_io",
        &eval_index_min_io(&[667095.0, 4.0175102921152845, 0.4706746620655204]),
        &[0x41132a110ac7633a],
        &[0x41132a110ac7633b],
    );
}

#[test]
fn d400_index_io() {
    gucs(1.0, 4.0, 0.0025);
    check(
        "index_io",
        &eval_index_io(&[0.8498666635548066, 549.2127671008111, 9268257.828083934, 2607.0300360534325]),
        &[0x4143a94af9664725],
        &[0x4143a94af9664726],
    );
}

#[test]
fn d400_bitmap_tree() {
    gucs(1.0, 4.0, 0.0025);
    check(
        "bitmap_tree",
        &eval_bitmap_tree(&[581.146186061289, 85088786.39797892]),
        &[0x40d55755f032d429],
        &[0x40d55755f032d42a],
    );
}

#[test]
fn d400_bitmap_and() {
    gucs(1.0, 4.0, 0.0031);
    check(
        "bitmap_and",
        &eval_bitmap_and(&[0.00043518394416338773, 243.78934806801175, 0.00010612700577638966, 99.1641064672982]),
        &[0x3fdaadc87cabbf89],
        &[0x3fdaadc87cabbf8a],
    );
}

#[test]
fn d400_bitmap_or() {
    gucs(1.0, 4.0, 0.0031);
    check(
        "bitmap_or",
        &eval_bitmap_or(&[0.10286233499706592, 33.766248372889876, 0.00034731662738552663]),
        &[0x3fdb1d86ed4bcc01],
        &[0x3fdb1d86ed4bcc02],
    );
}

#[test]
fn d400_bitmap_cost_per_page() {
    gucs(1.0, 4.0, 0.0025);
    check(
        "bitmap_cost_per_page",
        &eval_bitmap_cost_per_page(&[4.187547746292579, 0.5393335004354749, 98270.0, 1098866.0]),
        &[0x4008c5c3213e80d9],
        &[0x4008c5c3213e80da],
    );
}

#[test]
fn d400_bitmap_heap_run() {
    gucs(1.0, 4.0, 0.0025);
    check(
        "bitmap_heap_run",
        &eval_bitmap_heap_run(&[123416.0, 4.842934340687319, 4674.146187251438, 0.043214155844708974, 31289291.7580437]),
        &[0x413dd2ce0fa2f379],
        &[0x413dd2ce0fa2f37a],
    );
}

#[test]
fn d400_bitmap_lossy() {
    gucs(1.0, 4.0, 0.0025);
    check(
        "bitmap_lossy",
        &eval_bitmap_lossy(&[0.012700855418952535, 754609.0, 42844.0, 797453.0, 185325.11390258747]),
        &[0x40c7cc0e9d01e93d],
        &[0x40c7cc0e9d01e93e],
    );
}

#[test]
fn d400_tidscan() {
    gucs(1.0, 4.0, 0.0025);
    check(
        "tidscan",
        &eval_tidscan(&[1.0061630955118133, 165514.0, 0.30150832128954347, 0.4979326741763641, 70803.03936270454]),
        &[0x410eb96896c0ea7f],
        &[0x410eb96896c0ea80],
    );
}

#[test]
fn d400_tidrangescan() {
    gucs(1.0, 4.0, 0.0025);
    check(
        "tidrangescan",
        &eval_tidrangescan(&[2.0128144915136654, 0.1243592664316466, 971018.0, 0.03845844487162975, 3134315.5897943755, 0.12585163895160825, 193796.68433132413]),
        &[0x4110375e87d64419],
        &[0x4110375e87d64418],
    );
}

#[test]
fn d400_recursive_union() {
    gucs(1.0, 4.0, 0.0025);
    check(
        "recursive_union",
        &eval_recursive_union(&[7595.881084854173, 11.600061888876503, 872066.2492944521, 104669.99815286572, 7589928.557668956]),
        &[0x40bdabe18ec6e9ad, 0x413baf99198fcb7f, 0x41924dc05f4dcec1],
        &[0x40bdabe18ec6e9ad, 0x413baf99198fcb7f, 0x41924dc05f4dcec0],
    );
}

#[test]
fn d400_sort() {
    gucs(1.05, 3.9, 0.0025);
    check(
        "sort",
        &eval_sort(&[377237.0391048305, 132.0, 0.0, 7877.0, 73.31127690526219]),
        &[0x40edc769b17ce10c, 0x40ee3d4ca80c7606],
        &[0x40edc769b17ce10d, 0x40ee3d4ca80c7607],
    );
}

#[test]
fn d400_incremental_sort() {
    gucs(1.0, 4.0, 0.0025);
    check(
        "incremental_sort",
        &eval_incremental_sort(&[0.15830212471303834, 0.06723597047949834, 0.005587641209863986, 1536.0, 9644.871079415992, 0.0025805496756545728]),
        &[0x407fae72c63adc99],
        &[0x407fae72c63adc98],
    );
}

#[test]
fn d400_append_parallel() {
    gucs(1.0, 4.0, 0.0025);
    check(
        "append_parallel",
        &eval_append_parallel(&[5.0, 32843.6226959848, 36.5987917450579, 7.0, 5.471551636746143, 20.681229960630358, 4.0]),
        &[0x40e6742000000000, 0x4071f347c4d70e6b],
        &[0x40e6742000000000, 0x4071f347c4d70e6c],
    );
}

#[test]
fn d400_append_plain() {
    gucs(1.0, 4.0, 0.0025);
    check(
        "append_plain",
        &eval_append_plain(&[9592220.919051502, 53.07169648430701, 79.47211757024179, 10.013038453658053]),
        &[0x40e77312c62bbfe9],
        &[0x40e77312c62bbfe8],
    );
}

#[test]
fn d400_merge_append() {
    gucs(1.0, 4.0, 0.0025);
    check(
        "merge_append",
        &eval_merge_append(&[53.0, 2118876.4948947676, 240.83760588191672, 26910.693632215818]),
        &[0x406e4b604b9b78d3, 0x40f7f8e5fc402623],
        &[0x406e4b604b9b78d3, 0x40f7f8e5fc402622],
    );
}

#[test]
fn d400_material() {
    gucs(1.1, 4.0, 0.0025);
    check(
        "material",
        &eval_material(&[145.0373617112136, 46257.98894354237, 7005948.083958927, 168.0]),
        &[0x40622132112ff91f, 0x410ff8af6f560b95],
        &[0x40622132112ff91f, 0x410ff8af6f560b94],
    );
}

#[test]
fn d400_memoize_entry() {
    gucs(1.0, 4.0, 0.0025);
    check(
        "memoize_entry",
        &eval_memoize_entry(&[595.9567440769239, 192.0]),
        &[0x40fe46700b567563],
        &[0x40fe46700b567564],
    );
}

#[test]
fn d400_memoize_costs_1() {
    gucs(1.0, 4.0, 0.0025);
    check(
        "memoize_costs_1",
        &eval_memoize_costs(&[10772.874423383171, 7.0, 1.5482214488326367, 0.7788255073096233, 14.718574314151848, 70.252581281445, 1.6448147395396722]),
        &[0x404b5e07539f8b1b, 0x3ff575993164d89c],
        &[0x404b5e07539f8b1c, 0x3ff575993164d89d],
    );
}

#[test]
fn d400_memoize_costs_2() {
    gucs(1.0, 4.0, 0.0025);
    check(
        "memoize_costs_2",
        &eval_memoize_costs(&[132124.11440746096, 11588.0, 9.527326108118656, 0.9991778282612945, 287.07370394264206, 1.6583341256224693, 255.21239450453595]),
        &[0x3ffaac66d90bd099, 0x406ffaa6bec22a82],
        &[0x3ffaac66d90bd099, 0x406ffaa6bec22a81],
    );
}

#[test]
fn d400_memoize_costs_3() {
    gucs(1.0, 4.0, 0.0025);
    check(
        "memoize_costs_3",
        &eval_memoize_costs(&[1219411.2335270848, 82872.0, 31.726811914624353, 0.9996171588484093, 7.34746798432504, 21.511226977271885, 1.5666419416659618]),
        &[0x4035837818b14654, 0x3ff9bd9438af05c8],
        &[0x4035837818b14654, 0x3ff9bd9438af05c7],
    );
}

#[test]
fn d400_memoize_costs_4() {
    gucs(1.0, 4.0, 0.0025);
    check(
        "memoize_costs_4",
        &eval_memoize_costs(&[36290.62190073256, 32547.0, 3.288867361759939, 0.9998989502147123, 660.5299009138005, 1.9346568018399202, 3.3044272299154347]),
        &[0x3fff1d3b4de1e555, 0x401492c5b4d727dc],
        &[0x3fff1d3b4de1e555, 0x401492c5b4d727db],
    );
}

#[test]
fn d400_memoize_costs_5() {
    gucs(1.0, 4.0, 0.0025);
    check(
        "memoize_costs_5",
        &eval_memoize_costs(&[53472.98585254048, 40883.0, 83.21410061767534, 0.9979645793944262, 389.8224596183791, 3.673918705379728, 2.533813687507959]),
        &[0x400d750f4785a11d, 0x400d03e030957aca],
        &[0x400d750f4785a11d, 0x400d03e030957acb],
    );
}

#[test]
fn d400_memoize_costs_6() {
    gucs(1.0, 4.0, 0.0025);
    check(
        "memoize_costs_6",
        &eval_memoize_costs(&[1889340.5064314315, 626287.0, 42.21740684366621, 0.9999325909577499, 24.96045181256571, 27.44533222070192, 2.711074514601848]),
        &[0x403b743f9905419e, 0x40066aaf77b8f907],
        &[0x403b743f9905419f, 0x40066aaf77b8f907],
    );
}

#[test]
fn d400_agg_plain() {
    gucs(1.0, 4.0, 0.0025);
    check(
        "agg_plain",
        &eval_agg_plain(&[0.0, 0.011886506738895967, 0.002050937551209319, 0.004364589435256983, 0.26575728103871826, 5523.006113167106, 27460.92361111323, 19079978.497156907, 0.0, 30865.385557751833]),
        &[0x40f04210ccc82937, 0x40f04210f5bdebc6],
        &[0x40f04210ccc82936, 0x40f04210f5bdebc5],
    );
}

#[test]
fn d400_agg_sorted() {
    gucs(1.0, 4.0, 0.0025);
    check(
        "agg_sorted",
        &eval_agg_sorted(&[1.0, 0.0017207517567236898, 0.009876319026482621, 0.0003264449329556508, 0.009234446110247036, 825.2768658913183, 123.88504741463157, 4418.464382088627, 1.0, 24485.016563800116]),
        &[0x4089ca370576e4b8, 0x40844c379af3b24f],
        &[0x4089ca370576e4b8, 0x40844c379af3b24e],
    );
}

#[test]
fn d400_agg_hashed() {
    gucs(1.0, 4.0, 0.0025);
    check(
        "agg_hashed",
        &eval_agg_hashed(&[2.0, 0.008157038994510335, 0.3915744917671356, 0.009270941123224628, 0.21784273621440056, 7.092838307632659, 171.10289689259727, 1696.198532907051, 5.0, 78110.0228341047]),
        &[0x408ac4164adf11ac, 0x40d23753fb4d6e21],
        &[0x408ac4164adf11ab, 0x40d23753fb4d6e20],
    );
}

#[test]
fn d400_agg_spill() {
    gucs(1.1, 4.1, 0.0025);
    check(
        "agg_spill",
        &eval_agg_spill(&[59.372723383207315, 4859.035568722361, 5338.941683588387, 6.0, 185194.39366720154]),
        &[0x4111647a85ef62b5, 0x4115fca14ba3303b],
        &[0x4111647a85ef62b4, 0x4115fca14ba3303a],
    );
}

#[test]
fn d400_qual() {
    gucs(1.0, 4.0, 0.0025);
    check(
        "qual",
        &eval_qual(&[96.88911184787105, 1467.3875195636072, 0.00034026882881482457, 0.18516477328370154]),
        &[0x407709911e84ec16],
        &[0x407709911e84ec17],
    );
}

#[test]
fn d400_windowagg_total() {
    gucs(1.0, 4.0, 0.0025);
    check(
        "windowagg_total",
        &eval_windowagg_total(&[3.1339449934193238, 0.003187078912406419, 0.006090868164034708, 4.0, 9814988.584529474]),
        &[0x41118a1766af770f],
        &[0x41118a1766af7710],
    );
}

#[test]
fn d400_windowagg_startup() {
    gucs(1.0, 4.0, 0.0025);
    check(
        "windowagg_startup",
        &eval_windowagg_startup(&[31506.377766569625, 82806749.25041187, 32256295.367615655, 2480228.9943076605, 528712.6526648533]),
        &[0x41649d097b9e03c4],
        &[0x41649d097b9e03c5],
    );
}

#[test]
fn d400_group() {
    gucs(1.0, 4.0, 0.0025);
    check(
        "group",
        &eval_group(&[14.824419666736821, 1.277153800573855, 13439.810448119379, 5.0]),
        &[0x402da61a55a5a0b3, 0x406528cb08a88a65],
        &[0x402da61a55a5a0b3, 0x406528cb08a88a64],
    );
}

#[test]
fn d400_nestloop_initial_1() {
    gucs(1.1, 4.0, 0.0025);
    check(
        "nestloop_initial_1",
        &eval_nestloop_initial(&[57.42618570572136, 35306.87347389775, 195.15508386682137, 5.618073689772861, 7670.948639730723, 1232999.4539278592, 1.0, 264.0]),
        &[0x404f85aa4ab8181e, 0x4162e1a6d45ac71c, 0x4162e19ef2f0346e],
        &[0x404f85aa4ab8181e, 0x4162e1a6d45ac71b, 0x4162e19ef2f0346d],
    );
}

#[test]
fn d400_nestloop_initial_2() {
    gucs(1.1, 4.0, 0.0025);
    check(
        "nestloop_initial_2",
        &eval_nestloop_initial(&[4.361536496252703, 4677.905456142312, 3.1013458538893075, 45.83308028138122, 15211.184155473184, 283496.55920681165, 1.0, 472.0]),
        &[0x404918e933db9866, 0x40edcfdec9eedbb0, 0x40edc9988fa1e4ca],
        &[0x404918e933db9866, 0x40edcfdec9eedbae, 0x40edc9988fa1e4c8],
    );
}

#[test]
fn d400_nestloop_initial_plain() {
    gucs(1.0, 4.0, 0.0025);
    check(
        "nestloop_initial_plain",
        &eval_nestloop_initial_plain(&[760.6350191264296, 90788.21281460648, 739982.9012424682, 1021.3821473370344, 11023.273025118884, 414299.0827079033, 0.0, 53.0]),
        &[0x409bd8119415dcaa, 0x41fe633d6f687333, 0x41fe633d00082ce3],
        &[0x409bd8119415dcaa, 0x41fe633d6f687334, 0x41fe633d00082ce4],
    );
}

#[test]
fn d400_nestloop_early_idx() {
    gucs(1.0, 4.0, 0.0025);
    check(
        "nestloop_early_idx",
        &eval_nestloop_early_idx(&[857.8175371449659, 1.0, 62692.403160681024, 117.34451367360084, 88279.35046667061, 14546.691501733863, 0.014400921669521102, 90.62547697075732]),
        &[0x40b8251cdc393964, 0x4118a1d94a4c4d67],
        &[0x40b8251cdc393963, 0x4118a1d94a4c4d67],
    );
}

#[test]
fn d400_nestloop_early_noidx() {
    gucs(1.0, 4.0, 0.0025);
    check(
        "nestloop_early_noidx",
        &eval_nestloop_early_noidx(&[27.433448905902424, 0.0, 13.429240300598467, 22.80011784194744, 1498.5543787541678, 2547.1917990525735, 0.8341613333575952, 61.20811539545396]),
        &[0x40b9c96a649735f5, 0x412671f2d25c3625],
        &[0x40b9c96a649735f6, 0x412671f2d25c3626],
    );
}

#[test]
fn d400_join_tail() {
    gucs(1.0, 4.0, 0.0025);
    check(
        "join_tail",
        &eval_join_tail(&[20.404913963901137, 0.0019052987714403382, 107630.78103402001, 0.031351017346177956, 8169.8348377949105]),
        &[0x407e19b3943f84e1],
        &[0x407e19b3943f84e0],
    );
}

#[test]
fn d400_merge_startup() {
    gucs(1.0, 4.0, 0.0025);
    check(
        "merge_startup",
        &eval_merge_startup(&[21.94282884671894, 1269173.4274494294, 0.0008419754691759237]),
        &[0x40910a390ee590ff],
        &[0x40910a390ee590fe],
    );
}

#[test]
fn d400_merge_mat() {
    gucs(1.0, 4.0, 0.0025);
    check(
        "merge_mat",
        &eval_merge_mat(&[1.0633947562701331, 773.3746796604255, 6.541665375064647]),
        &[0x402b6c2e4a48b821],
        &[0x402b6c2e4a48b822],
    );
}

#[test]
fn d400_merge_quals() {
    gucs(1.0, 4.0, 0.0025);
    check(
        "merge_quals",
        &eval_merge_quals(&[27.940497567211754, 1.0155336038297833, 0.09323282802713047, 2238.7866312173237, 912.0, 4350487.797875907, 33324.0, 3.9235703014823424]),
        &[0x40c807895d374f9f, 0x4138196737278cab],
        &[0x40c807895d374fa0, 0x4138196737278cac],
    );
}

#[test]
fn d400_hash_initial() {
    gucs(1.0, 4.0, 0.0025);
    check(
        "hash_initial",
        &eval_hash_initial(&[15.973267490811871, 8.680777464675307, 2.0, 4237952.995107412, 324676.2095404193]),
        &[0x40ef0c28950bcc21, 0x4099803f4f18b56b],
        &[0x40ef0c28950bcc22, 0x4099803f4f18b56a],
    );
}

#[test]
fn d400_hash_batch() {
    gucs(1.1, 4.0, 0.0025);
    check(
        "hash_batch",
        &eval_hash_batch(&[590.2250188451779, 15526.809335524593, 2984831.226453306, 439.0, 1793019.8932097554, 398.0]),
        &[0x40f91118667a291e, 0x411de132d65c0d41],
        &[0x40f91118667a291d, 0x411de132d65c0d40],
    );
}

#[test]
fn d400_hash_unmatched() {
    gucs(1.0, 4.0, 0.0025);
    check(
        "hash_unmatched",
        &eval_hash_unmatched(&[214.1843762293765, 0.049292049309575404, 21.4957942091406, 5620.0]),
        &[0x407ffec8db7d72bd],
        &[0x407ffec8db7d72be],
    );
}

#[test]
fn d400_saop_hash() {
    gucs(1.0, 4.0, 0.0025);
    check(
        "saop_hash",
        &eval_saop_hash(&[0.2210964201648526, 683.0, 0.432677318325117]),
        &[0x40727bd5d4bd041d],
        &[0x40727bd5d4bd041c],
    );
}

#[test]
fn d400_array_coerce() {
    gucs(1.0, 4.0, 0.0025);
    check(
        "array_coerce",
        &eval_array_coerce(&[0.111754272492369, 0.6153788766083379, 23532.0]),
        &[0x40cc489a8ea8cf8b],
        &[0x40cc489a8ea8cf8c],
    );
}

#[test]
fn d400_anti() {
    gucs(1.0, 4.0, 0.0025);
    check(
        "anti",
        &eval_anti(&[4996.19553406196, 0.799698244838291, 0.9431659747642321, 0.0016920885191045126]),
        &[0x40009ee607cba831],
        &[0x40009ee607cba832],
    );
}
