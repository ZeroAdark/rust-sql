//! Construct C2 (b), the run-ahead tracer's quiet stamp (frame/program.rs `gate_stamp`, program `Pipeline::quiet`): a
//! pipeline whose source PostgreSQL 18.6 may never run reads quiet when neither it nor what feeds it may raise or act,
//! by the PG-quiet reading (effects/visible/hashjoin.rs `side_by`), where a Sort raises nothing when every key orders
//! by an initdb operator whose function PostgreSQL marks leakproof (effects.rs `sort_quiet`). Over the plan fixtures
//! with a Sort PostgreSQL may never run, lowered with the stamps on: a gated pipeline that fills a Sort of int4 / int8
//! keys (its source is the Sort's child and it ends in a Sort or TopK breaker: its sort step compares) reads quiet in
//! some (a merge join's inner, a hash join's inner); one that fills or drains a Sort by an operator PostgreSQL does not
//! mark leakproof (numeric, an array, a record, a range, an enum, jsonb) never does; rangetypes-005, a merge join's inner
//! sorted by numrange, is one.
use gravity_lowerv4_it as common;

use gravity_abi::BreakerKind;
use gravity_decompose::corpus;
use gravity_program::End;
use mcx::MemoryContext;
use types_nodes::plannodes::PlannedStmt;
use types_nodes::NodeTag;

/// `<` / `>` of numeric, anyarray, record, anyrange, anyenum and jsonb: their functions are not leakproof.
const RAISING: [u32; 11] = [1754, 1756, 1072, 1073, 2990, 2991, 3884, 3887, 3518, 3519, 3242];
/// `<` / `>` of int4 and int8: int4lt, int4gt, int8lt, int8gt are leakproof.
const INTS: [u32; 4] = [97, 521, 412, 413];

/// The sort operators of the Sort a pipeline with source plan node `source` drains (the source itself) or fills (its
/// parent), and whether it fills it.
fn sort_ops<'p>(stmt: &'p PlannedStmt<'p>, source: i32) -> Option<(Vec<u32>, bool)> {
    let (n, up) = gravity_decompose::lifetime::node_of(stmt, source)?;
    let (s, fills) = if n.node_tag() == NodeTag::T_Sort { (n, false) } else { (up.filter(|u| u.node_tag() == NodeTag::T_Sort)?, true) };
    s.as_sort().map(|s| (s.sortOperators.to_vec(), fills))
}

/// Some plan Sort of `stmt` is a node PostgreSQL may never run.
fn gated_sort<'p>(stmt: &'p PlannedStmt<'p>) -> bool {
    let scratch = mcx::MemoryContext::new("c2 quiet sort never");
    let never = gravity_decompose::pg_demand::never_run(stmt, scratch.mcx());
    let mut any = false;
    gravity_decompose::rerun::each_plan_node(stmt, &mut |n| {
        any |= n.node_tag() == NodeTag::T_Sort && n.as_plan().is_some_and(|p| never.binary_search(&p.plan_node_id).is_ok());
    });
    any
}

#[test]
fn a_gated_sort_reads_quiet_by_its_operators() {
    gravity_lowerv4::frame::program::set_gate_stamps(true);
    let mut all = common::files(&common::plans_dir(), "plan");
    all.extend(common::files(&common::plans_dir().join("regress-full"), "plan").into_iter().map(|(n, t)| (format!("regress-full/{n}"), t)));
    let (mut lowered, mut quiet, mut raising) = (0, Vec::new(), Vec::new());
    for (name, text) in all.iter().filter(|(_, t)| t.contains("{SORT ")) {
        let ctx = MemoryContext::new("c2 quiet sort");
        let Ok(fx) = corpus::load(ctx.mcx(), text) else { continue };
        if !gated_sort(fx.stmt) {
            continue;
        }
        let Ok(Some(Ok(l))) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| common::lower_fixture(text))) else { continue };
        lowered += 1;
        for p in l.descs.pipelines.iter().filter(|p| p.pg_gate >= 0) {
            let Some((ops, fills)) = sort_ops(fx.stmt, p.pg_gate) else { continue };
            if ops.iter().any(|o| RAISING.contains(o)) {
                assert!(!p.quiet, "{name}: the pipeline of gated node {} reads quiet past a Sort by {ops:?}, whose comparison PostgreSQL does not mark leakproof", p.pg_gate);
                raising.push(format!("{name} {}", p.pg_gate));
            } else if fills && p.quiet && ops.iter().all(|o| INTS.contains(o)) && matches!(p.ends_in, End::Breaker(b) if matches!(b.kind, BreakerKind::Sort | BreakerKind::TopK)) {
                quiet.push(format!("{name} {}", p.pg_gate));
            }
        }
    }
    eprintln!("{lowered} lowered fixtures with a gated Sort: {} quiet int Sort fills, raising {raising:?}", quiet.len());
    assert!(!quiet.is_empty(), "no gated pipeline filling an int4 / int8 Sort reads quiet in {lowered} fixtures: the stamp reads a sort step as raising");
    assert!(raising.iter().any(|x| x.starts_with("regress-full/rangetypes-005 ")), "rangetypes-005's merge inner (a Sort by numrange) is not among the gated raising Sorts: {raising:?}");
}
