//! D1478 (M8-SUB): an EvalPlanQual recheck substituted every positioned site of the stopped node, a SubPlan's own scan too,
//! so a SubPlan below LockRows read the substituted position (its window's base, no row) and came back empty: NULL, EXISTS
//! false (isolation overlay epq-subq-subplan). PostgreSQL's PlanRowMarks name the top-level query's relations alone, and
//! ExecScanFetch runs any other scan again under the snapshot. nest.subplan now names its inner's scan site, and the
//! pipelines its inner's slots come from, as `ProtoKind::Rescan`; exec epq.rs leaves those (with an InitPlan's and a hashed
//! set's pipelines, and their producers) unsubstituted.
use gravity_lowerv4_it as common;

use gravity_program::protos::ProtoKind;

/// The Rescan kind by its name, so the test compiles at the parent (its proof fails there on the assertions).
const RESCAN: &str = "nest.subplan.rescan";

fn lowered(rel: &str) -> common::LoweredIn {
    let p = common::plans_dir().join(rel);
    let text = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    common::lower_fixture(&text).expect("decomposes").unwrap_or_else(|r| panic!("{rel} refused: {r:?}"))
}

/// The Rescan protos' sites, and the pipelines they name (site -1).
fn rescans(l: &common::LoweredIn) -> (Vec<usize>, Vec<usize>) {
    let ps = l.descs.v4.protos.iter().filter(|p| p.kind.name() == RESCAN);
    (ps.clone().filter(|p| p.site >= 0).map(|p| p.site as usize).collect(), ps.filter(|p| p.site < 0).map(|p| p.pipeline as usize).collect())
}

/// cte-013 `SELECT a FROM t WHERE a < 100 AND b < (SELECT y FROM u WHERE u.x = t.a)` and cte-015 `SELECT a, EXISTS (SELECT 1
/// FROM u WHERE u.x = t.a) FROM t WHERE a < 3`: the inner's scan of u is named, the outer's scan of t is not.
#[test]
fn a_subplans_inner_scan_is_named_to_the_recheck() {
    for rel in ["cte-013.plan", "cte-015.plan"] {
        let l = lowered(rel);
        let (sites, pipes) = rescans(&l);
        let rel_of = |s: usize| l.descs.sites[s].relation;
        assert_eq!(sites.len(), 1, "{rel}: one inner site named: {:?}", l.descs.v4.protos);
        assert!(pipes.is_empty(), "{rel}: no pipeline named: {pipes:?}");
        assert_eq!(rel_of(sites[0]), 16405, "{rel}: the named site scans u");
        let outer = l.descs.pipelines[0].driving_site;
        assert!(outer >= 0 && rel_of(outer as usize) == 16395 && !sites.contains(&(outer as usize)), "{rel}: t's scan stays a recheck's substitution");
    }
}

/// m8x-018 `SELECT x, (SELECT count(*) FROM generate_series(1, x) y) FROM generate_series(1, 5) x`: the inner reads its
/// rerun set's aggregate, so the set's pipeline is named (its scan and its producers re-run under a recheck).
#[test]
fn a_subplans_rerun_set_is_named_to_the_recheck() {
    let text = std::fs::read_to_string(common::milestone_dir(8).join("m8x-018.plan")).expect("fixture");
    let l = common::lower_fixture(&text).expect("decomposes").unwrap_or_else(|r| panic!("m8x-018 refused: {r:?}"));
    let (_, pipes) = rescans(&l);
    let members: Vec<usize> = l.descs.v4.protos.iter().filter(|p| p.kind == ProtoKind::Rerun).map(|p| p.pipeline as usize).collect();
    assert!(!pipes.is_empty() && pipes.iter().all(|q| members.contains(q)), "the rerun set's pipeline named: {pipes:?} vs members {members:?}");
}
