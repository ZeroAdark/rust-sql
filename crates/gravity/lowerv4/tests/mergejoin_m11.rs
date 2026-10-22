//! M11's rig (the merged M9's merge-join part; owner M11-MERGE). MERGE-1: a plan Material under a merge join's inner
//! (graph.rs `pipe_kind`, main 09-27 17:59Z) is merge.join's in admission (`pipeline_kinds`) and in resolve (`step_need`)
//! alike, over the regress plan fixtures holding a MergeJoin and a Material; the statements' rows are the e2e files'
//! (e2e/gravity/v4/m11_merge_*.sql, judged against PostgreSQL 18.6).
use gravity_lowerv4_it as common;

use gravity_abi::breaker::BreakerKind;
use gravity_decompose::join::JoinDesc;
use gravity_decompose::Op;
use gravity_program::End;
use gravity_lowerv4::face::kinds::KindId;
use gravity_lowerv4::graph::Gx;

/// `f` over the graph of a plan fixture's text; `None` when decompose refuses it.
fn with_graph<R>(name: &str, text: &str, f: impl FnOnce(&Gx<'_, '_>) -> R) -> Option<R> {
    let rig = mcx::MemoryContext::new("GravityPlanRig");
    common::RigCatalog::install_seams();
    let ctx = mcx::MemoryContext::new("m11 fixture");
    let fx = gravity_decompose::corpus::load(ctx.mcx(), text).unwrap_or_else(|e| panic!("{name}: {e}"));
    let arena = gravity_program::Arena::new(1 << 20);
    let graph = gravity_decompose::decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).ok()?;
    Some(f(&Gx { graph: &graph, facts: &fx.facts, catalog: common::RigCatalog::get(), over: None, mcx: rig.mcx() }))
}

#[test]
fn a_material_under_a_merge_inner_is_merge_joins_in_admission_and_resolve() {
    let (mut merged, mut other, mut graphs) = (0, 0, 0);
    // every regress plan fixture holding both a MergeJoin and a Material (a Material under a merge inner among them)
    let fixtures = common::files(&common::plans_dir().join("regress-full"), "plan");
    for (name, text) in fixtures.iter().filter(|(_, t)| t.contains("{MERGEJOIN") && t.contains("{MATERIAL")) {
        let Some(()) = with_graph(name, text, |gx| {
            for k in 0..gx.pipelines().len() {
                let mut listed = mcx::PgVec::new_in(gx.mcx);
                gx.pipeline_kinds(k, &mut listed);
                let end = gx.end_need(k);
                for (j, op) in gx.pipelines()[k].ops.iter().enumerate() {
                    let need = gx.step_need(k, j);
                    assert!(listed.iter().any(|&(x, _)| x == need), "{name} p{k} op {j}: resolve's {need:?} is not in admission's {listed:?}");
                    if matches!(op, Op::Join(JoinDesc::Spool)) {
                        let want = if end == KindId::MergeJoin { KindId::MergeJoin } else { KindId::NestSpool };
                        assert_eq!(need, want, "{name} p{k} op {j}: a spool op under end {end:?}");
                        if want == KindId::MergeJoin {
                            merged += 1;
                            assert!(!listed.iter().any(|&(x, _)| x == KindId::NestSpool), "{name} p{k}: a merge inner's spool still needs nest.spool: {listed:?}");
                        } else {
                            other += 1;
                        }
                    }
                }
            }
        }) else {
            continue;
        };
        graphs += 1;
    }
    assert!(graphs > 0 && merged > 0, "no fixture decomposed a Material under a merge inner ({graphs} graphs, {merged} spools)");
    eprintln!("m11: {graphs} graphs, {merged} merge-inner spool ops, {other} other spool ops");
}

/// main 18:12Z (4): a Sort's end is merge.join's exactly when a merge join reads its slot (graph.rs end_need), or (D2975) when
/// it is an earlier arm of a union whose run a merge join reads; every other
/// Sort keeps runs.sort's (or its group / merge-append form's) end, over the regress plan fixtures holding a Sort.
#[test]
fn only_a_sort_a_merge_join_reads_changes_its_end() {
    let (mut merge, mut kept) = (0, 0);
    let fixtures = common::files(&common::plans_dir().join("regress-full"), "plan");
    for (name, text) in fixtures.iter().filter(|(_, t)| t.contains("{SORT")).take(600) {
        with_graph(name, text, |gx| {
            for k in 0..gx.pipelines().len() {
                let End::Breaker(b) = gx.pipelines()[k].desc.ends_in else { continue };
                if b.kind != BreakerKind::Sort {
                    continue;
                }
                let reads = |k: usize| gx.pipelines().iter().enumerate().any(|(j, q)| q.ops.iter().any(|op| matches!(op, Op::Join(JoinDesc::Merge { slot, .. }) if gx.life_of(j, *slot).is_some_and(|l| l.0 as usize == k))));
                // D2975: an earlier arm of a union whose last arm's run a merge join reads fills that run too
                let read = reads(k) || gx.run_arms(k).into_iter().any(reads);
                let end = gx.end_need(k);
                assert_eq!(end == KindId::MergeJoin, read, "{name} p{k}: a Sort's end {end:?}, read by a merge join: {read}");
                if read { merge += 1 } else { kept += 1 }
            }
        });
    }
    assert!(merge > 0 && kept > 0, "{merge} merge-inner Sorts, {kept} others");
    eprintln!("m11: {merge} merge-inner Sorts, {kept} other Sorts kept");
}
