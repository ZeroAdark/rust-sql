//! D400: aarch64 PostgreSQL fuses these cost products (GCC
//! -ffp-contract=fast). Each input set is one where the fused and unfused
//! results differ. The expected bits come from the exact C expressions,
//! built with the box-23 GCC 13.3 -O2: once with the default contraction
//! (fmadd) and once with -ffp-contract=off. Each test fails if its site
//! is left unfused on aarch64, or fused on any other target.

use mcx::{MemoryContext, PgVec};
use types_nodes::NodeTag;
use types_pathnodes::{Path, PathId, PathNode, PathTarget, RelId, RelOptInfo};

const FUSED: bool = cfg!(target_arch = "aarch64");

fn pick(fused: u64, unfused: u64) -> f64 {
    f64::from_bits(if FUSED { fused } else { unfused })
}

fn base_path<'mcx>(mcx: mcx::Mcx<'mcx>, rel: RelId, rows: f64, startup: f64, total: f64) -> Path<'mcx> {
    Path {
        type_: types_pathnodes::tag16(NodeTag::T_Path),
        pathtype: types_pathnodes::tag16(NodeTag::T_SeqScan),
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

fn sub_path<'mcx>(
    run: &mut crate::run::PlannerRun<'mcx>,
    rel: RelId,
    rows: f64,
    total: f64,
) -> PathId {
    let pt = run.root.alloc_pathtarget(PathTarget::new(run.mcx));
    let mut p = base_path(run.mcx, rel, rows, 0.0, total);
    p.pathtarget_id = Some(pt);
    run.root.alloc_path(PathNode::Path(p))
}

fn default_costs() {
    assert_eq!(crate::gucs::cpu_operator_cost(), 0.0025);
    assert_eq!(crate::gucs::cpu_tuple_cost(), 0.01);
}

// pathnode.c:144/146 (compare_fractional_path_costs). Unfused, the two
// costs tie (0); fused, path1 costs one ulp more (+1).
#[test]
fn d400_compare_fractional_path_costs_fuses() {
    let cx = MemoryContext::new("t");
    let mcx = cx.mcx();
    let rel = RelId::default();
    let p1 = base_path(mcx, rel, 10.0, 523.66, 1265.17);
    let p2 = base_path(mcx, rel, 10.0, 0.0, 5978.109999999999);
    let got = crate::pathnode::compare_fractional_path_costs(&p1, &p2, 0.1);
    assert_eq!(got, if FUSED { 1 } else { 0 });
}

// pathnode.c:3985 (create_lockrows_path).
#[test]
fn d400_lockrows_total_fuses() {
    default_costs();
    let cx = MemoryContext::new("t");
    let mcx = cx.mcx();
    let mut run = crate::run::PlannerRun::new(mcx);
    let rel = run.root.alloc_rel(RelOptInfo::new(mcx));
    let sub = sub_path(&mut run, rel, 18791.0, 4.57);
    let id = crate::pathnode::create_lockrows_path(&mut run, rel, sub, PgVec::new_in(mcx), 0);
    let got = run.root.path(id).base().total_cost;
    // C: 0x1.80f5c28f5c290p+7 fused, 0x1.80f5c28f5c28fp+7 unfused.
    assert_eq!(got.to_bits(), pick(0x4068_0F5C_28F5_C290, 0x4068_0F5C_28F5_C28F).to_bits());
}

// pathnode.c:3357 (create_upper_unique_path).
#[test]
fn d400_upper_unique_total_fuses() {
    default_costs();
    let cx = MemoryContext::new("t");
    let mcx = cx.mcx();
    let mut run = crate::run::PlannerRun::new(mcx);
    let rel = run.root.alloc_rel(RelOptInfo::new(mcx));
    let sub = sub_path(&mut run, rel, 69058.0, 61.19);
    let id = crate::pathnode::create_upper_unique_path(&mut run, rel, sub, 5, 100.0);
    let got = run.root.path(id).base().total_cost;
    // C: 0x1.ce351eb851eb9p+9 fused, ...eb8p+9 unfused.
    assert_eq!(got.to_bits(), pick(0x408C_E351_EB85_1EB9, 0x408C_E351_EB85_1EB8).to_bits());
}

fn setop(strategy: types_pathnodes::SetOpStrategy) -> (f64, f64) {
    crate::tests::install_fixtures();
    // The hashed arm reads hash_mem through the work_mem-backed globals.
    if !guc_tables::vars::work_mem.installed() {
        init_small::init_seams();
    }
    let cx = MemoryContext::new("t");
    let mcx = cx.mcx();
    let mut run = crate::run::PlannerRun::new(mcx);
    let rel = run.root.alloc_rel(RelOptInfo::new(mcx));
    let l = sub_path(&mut run, rel, 65360.0, 929.02);
    let r = sub_path(&mut run, rel, 23681.0, 21.78);
    let mut groups = PgVec::new_in(mcx);
    for _ in 0..3 {
        groups.push(types_pathnodes::NodeId::default());
    }
    let id = crate::pathnode::create_setop_path(
        &mut run,
        rel,
        l,
        r,
        types_pathnodes::SETOPCMD_EXCEPT,
        strategy,
        groups,
        1.0,
        71602.0,
    );
    let p = run.root.path(id).base();
    (p.startup_cost, p.total_cost)
}

// pathnode.c:3839 + 3847 (sorted): both products are fused; each one alone
// moves the total.
#[test]
fn d400_setop_sorted_total_fuses() {
    let _guc = crate::tests::GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    default_costs();
    let (_, total) = setop(types_pathnodes::SETOP_SORTED);
    // C: 0x1.c167333333333p+10 fused, 0x1.c167333333334p+10 unfused.
    assert_eq!(total.to_bits(), pick(0x409C_1673_3333_3333, 0x409C_1673_3333_3334).to_bits());
}

// pathnode.c:3859 + 3868 (hashed).
#[test]
fn d400_setop_hashed_costs_fuse() {
    let _guc = crate::tests::GUC_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    default_costs();
    let (startup, total) = setop(types_pathnodes::SETOP_HASHED);
    // C: startup 0x1.94a6e147ae147p+10 fused, ...148p+10 unfused.
    assert_eq!(startup.to_bits(), pick(0x4099_4A6E_147A_E147, 0x4099_4A6E_147A_E148).to_bits());
    assert_eq!(total.to_bits(), pick(0x409C_1673_3333_3333, 0x409C_1673_3333_3334).to_bits());
}
