//! M5 L0-d (M5-plan §1.3, §9 Wave 1a row 1a-6; hazard H9a, R8-L04): an SRF step has a say in its pipeline's parallelism. A
//! pipeline turned serial only through its source's `pipeline_desc` (a LIMIT, a pool-unsafe host site) and its end's, so a
//! ProjectSet over an eligible heap scan stayed `Parallel::Eligible` whatever the function: an fmgr / PL / volatile set
//! would run on pool threads. hir.rs `srf_serial` (asked by source/heap.rs) now keeps it on the leader unless srf.parallel
//! serves it and every function is a set the iterator emits with no host (M5-SERIES R2 narrowed it from any library set),
//! parallel safe and not volatile; the road off is the serial form.
use gravity_lowerv4_it as common;

use gravity_decompose::ops::shared::srf::SrfKind;
use gravity_decompose::Op;
use gravity_lowerv4::face::kinds::KindId;
use gravity_lowerv4::face::strategy::{StrategySet, SRF_PARALLEL, set_off};
use gravity_lowerv4::frame::frame::SourceMaker;
use gravity_lowerv4::frame::px::Px;
use gravity_lowerv4::graph::Gx;
use gravity_program::{Arena, Parallel, Reason as Why};

/// The heap source's pipeline descriptor for `rel`'s heap scan pipeline, beside decompose's own, with srf.parallel
/// applying (it applies to every SRF statement). `set`: the SRF step's first function made `(funcid, kind)` (a set the
/// census has no heap-scan fixture of; the rule reads only the function and its kind).
fn descs(rel: &str, set: Option<(u32, SrfKind)>) -> (Parallel, Parallel) {
    let rig = mcx::MemoryContext::new("GravityPlanRig");
    common::RigCatalog::install_seams();
    let text = std::fs::read_to_string(common::plans_dir().join(rel)).expect("plan fixture");
    let ctx = mcx::MemoryContext::new("m5 l0 parallel");
    let fx = gravity_decompose::corpus::load(ctx.mcx(), &text).unwrap_or_else(|e| panic!("fixture: {e}"));
    let arena = Arena::new(1 << 20);
    let a = arena.handle();
    let mut g = gravity_decompose::decompose_in(a, fx.stmt, &fx.facts, fx.mcx).expect("decomposes");
    for d in g.pipelines.iter_mut().flat_map(|p| p.ops.iter_mut()).filter_map(|op| if let Op::Srf(d) = op { Some(d) } else { None }) {
        if let Some((funcid, kind)) = set {
            (d.functions[0].funcid, d.functions[0].kind) = (funcid, kind);
        }
    }
    let k = g.pipelines.iter().position(|p| matches!(p.source, gravity_decompose::Source::Range { .. })).expect("a heap scan pipeline");
    let gx = Gx { graph: &g, facts: &fx.facts, catalog: common::RigCatalog::get(), over: None, mcx: rig.mcx() };
    let mut words = gravity_lower::Sequential::new(a);
    let mut px = Px::new(a, &gx, &mut words);
    px.strategies.add(SRF_PARALLEL);
    let maker = gravity_lowerv4::ops::kind_as::<SourceMaker>(KindId::SrcHeap).expect("src.heap registers its maker");
    gravity_expr::nodes::func::acl_defer_begin();
    let d = gravity_lowerv4::catch(0, || {
        let src = (maker.0)(&mut px, k);
        src.pipeline_desc(&px, k)
    })
    .unwrap_or_else(|r| panic!("{rel}: the heap source refused: {r:?}"));
    gravity_expr::nodes::func::acl_defer_end();
    (g.pipelines[k].desc.parallel, d.parallel)
}

fn serial_by_the_veto(rel: &str, set: Option<(u32, SrfKind)>) {
    let (planned, got) = descs(rel, set);
    assert_eq!(planned, Parallel::Eligible, "{rel}: decompose plans the ProjectSet's heap pipeline eligible");
    assert!(matches!(got, Parallel::Ineligible { reason: Why::Host, .. }), "{rel} {set:?}: the SRF step keeps it on the leader: {got:?}");
}

/// P1 (bench-089, `SELECT count(*) FROM (SELECT generate_series(1, a % 3) FROM g) s`): decompose plans its heap scan
/// eligible (W8 / W16 are P1's cells), and srf.parallel serves it: generate_series over int4 is the iterator's, with no
/// host, parallel safe and immutable.
#[test]
fn p1_stays_parallel_where_srf_parallel_serves_it() {
    let (planned, got) = descs("bench-089.plan", None);
    assert_eq!((planned, got), (Parallel::Eligible, Parallel::Eligible));
}

/// The road's off twin (`gravity_road_off=srf.parallel`) is the serial form.
#[test]
fn the_road_off_twin_is_serial() {
    let mut off = StrategySet::EMPTY;
    off.add(SRF_PARALLEL);
    set_off(off);
    let r = std::panic::catch_unwind(|| serial_by_the_veto("bench-089.plan", None));
    set_off(StrategySet::EMPTY);
    r.unwrap_or_else(|e| std::panic::resume_unwind(e));
}

/// Does not apply (G4; main 09-26): a library entry or an array walk over an eligible heap scan stays on the leader,
/// parallel safe and immutable as they are: their per-open contexts are the leader thread's slot bank (M5-HOSTS H-a /
/// H-f). Before this rule any set the library serves (`gravity_fn::srf::serves`) ran on the pool.
#[test]
fn a_library_or_array_set_over_a_heap_scan_stays_on_the_leader() {
    for set in [(2331, SrfKind::Unnest), (1191, SrfKind::Subscripts), (3219, SrfKind::JsonbArrayElements { text: false }), (3260, SrfKind::SeriesNumeric { step: false })] {
        serial_by_the_veto("bench-089.plan", Some(set));
    }
}

/// Does not apply (G4): a heap pipeline without an SRF step keeps decompose's descriptor (bench-001, Q1: `SELECT id, a + 1
/// FROM g WHERE a > 15`, eligible).
#[test]
fn a_heap_pipeline_without_a_set_function_is_left_alone() {
    let (planned, got) = descs("bench-001.plan", None);
    assert_eq!(planned, Parallel::Eligible);
    assert_eq!(got, planned, "no SRF step: the heap rule's descriptor is decompose's");
}
