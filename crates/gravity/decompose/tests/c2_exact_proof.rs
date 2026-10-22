//! Construct C2 (D1): a structural proof is built only by the licence home, each constructor checking its evidence
//! against the statement (decompose effects/licence.rs `Proof`), so a call site cannot assert one. Read over captured
//! regression plans: FirstFetch agrees with the walk `PipelineGraph::first_cte` reads, PgReadsWhole takes a Sort, a Hash
//! and a plain or hashed Agg only, PgFailsToo a Hash Join or a FULL Merge Join only, PgRunsFirst the first-fetch chain,
//! and a SubPlan the first row of a Values first fetch evaluates first (`subplan_first`).
use std::fs;

use gravity_decompose::effects::{Effects, Proof, ProofKind};
use gravity_decompose::ops::shared::agg::{AGG_HASHED, AGG_PLAIN};
use gravity_decompose::ops::shared::subplan::SubLink;
use gravity_decompose::lifetime::node_of;
use gravity_decompose::{corpus, decompose_in};
use gravity_program::Arena;
use mcx::MemoryContext;
use types_nodes::jointype::JoinType;
use types_nodes::NodeTag;

const FIXTURES: [&str; 6] = ["numeric-013", "numeric-021", "jsonb_jsonpath-002", "join-025", "aggregates-030", "aggregates-001"];

fn text(name: &str) -> String {
    fs::read_to_string(manifest_dir().join("tests/plans/regress-full").join(format!("{name}.plan"))).unwrap()
}

#[test]
fn first_fetch_is_the_first_cte_walk() {
    let mut seen = 0;
    for name in FIXTURES {
        let ctx = MemoryContext::new("c2-exact");
        let fx = corpus::load(ctx.mcx(), &text(name)).unwrap();
        let arena = Arena::new(1 << 20);
        let g = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).expect("decomposes");
        for i in g.init_plans.iter().filter(|i| matches!(i.link, SubLink::Cte)) {
            let p = Proof::first_fetch(fx.stmt, i.plan_id);
            let carried = g.first_cte.filter(|(id, _)| *id == i.plan_id).map(|(_, l)| l);
            assert_eq!(p.is_some() && !Effects::of_init_plan(i).acts, carried.is_some(), "{name}: CTE {}: the proof and first_cte disagree", i.plan_id);
            assert!(carried.is_none_or(|l| l.exact() == p), "{name}: CTE {}: first_cte carries the FirstFetch proof's licence", i.plan_id);
            if let Some(p) = p {
                seen += 1;
                let (n, _) = node_of(fx.stmt, p.node()).expect("the cited node is the statement's");
                assert!(p.kind() == ProofKind::FirstFetch && n.as_cte_scan().is_some_and(|c| c.ctePlanId == i.plan_id), "{name}: the CteScan is cited");
            }
        }
        assert!(Proof::first_fetch(fx.stmt, 9999).is_none(), "{name}: no such CTE");
    }
    assert!(seen > 0, "a fixture whose CTE is the first fetch (numeric-013's nest outer)");
}

#[test]
fn node_evidence_is_checked_by_kind() {
    let (mut whole, mut fails, mut refused) = (0, 0, 0);
    for name in FIXTURES {
        let ctx = MemoryContext::new("c2-exact");
        let fx = corpus::load(ctx.mcx(), &text(name)).unwrap();
        for id in 0..256 {
            let Some((n, _)) = node_of(fx.stmt, id) else { continue };
            let reads_whole = match n.node_tag() {
                NodeTag::T_Sort | NodeTag::T_Hash => true,
                NodeTag::T_Agg => n.as_agg().is_some_and(|a| a.aggstrategy == AGG_PLAIN || a.aggstrategy == AGG_HASHED),
                _ => false,
            };
            let covers = n.as_hash_join().is_some() || n.as_merge_join().is_some_and(|m| m.join.jointype == JoinType::JOIN_FULL);
            let w = Proof::pg_reads_whole(fx.stmt, id);
            let f = Proof::pg_fails_too(fx.stmt, id);
            assert_eq!(w.is_some(), reads_whole, "{name}: node {id} {:?}: PgReadsWhole", n.node_tag());
            assert_eq!(f.is_some(), covers, "{name}: node {id} {:?}: PgFailsToo", n.node_tag());
            assert!(w.is_none_or(|p| p.kind() == ProofKind::PgReadsWhole && p.node() == id));
            assert!(f.is_none_or(|p| p.kind() == ProofKind::PgFailsToo && p.node() == id));
            whole += usize::from(reads_whole);
            fails += usize::from(covers);
            refused += usize::from(!reads_whole && !covers);
        }
        assert!(Proof::pg_reads_whole(fx.stmt, -5).is_none() && Proof::pg_fails_too(fx.stmt, -5).is_none(), "{name}: no such node");
    }
    assert!(whole > 0 && fails > 0 && refused > 0, "each evidence check met both answers: {whole} {fails} {refused}");
}

#[test]
fn runs_first_is_the_first_fetch_chain() {
    let ctx = MemoryContext::new("c2-exact");
    let fx = corpus::load(ctx.mcx(), &text("numeric-013")).unwrap();
    let root = fx.stmt.planTree.and_then(|n| n.as_plan()).map(|p| p.plan_node_id).expect("a plan");
    assert!(Proof::pg_runs_first(fx.stmt, root).is_some_and(|p| p.kind() == ProofKind::PgRunsFirst), "the root is on the chain");
    let cte = (0..256).find(|&id| node_of(fx.stmt, id).is_some_and(|(n, _)| n.as_cte_scan().is_some() && Proof::pg_runs_first(fx.stmt, id).is_some()));
    assert!(cte.is_some(), "the nest's outer CteScan, which ExecNestLoop fetches first");
    let off = (0..256).filter(|&id| node_of(fx.stmt, id).is_some() && Proof::pg_runs_first(fx.stmt, id).is_none()).count();
    assert!(off > 0, "the nest's inner is off the chain");
}

/// D1 site 5: a shared CTE's PgFailsToo names the CTE its Hash Join builds from, and holds
/// only where the join's outer first-fetch chain reads the same CTE: with-025's self join of `t` (a Sort over a Hash Join
/// of two CteScans of CTE 1). Another CTE id, or a node that is no Hash Join, holds none.
#[test]
fn shared_cte_proof_needs_an_outer_reader() {
    let ctx = MemoryContext::new("c2-exact");
    let fx = corpus::load(ctx.mcx(), &text("with-025")).unwrap();
    let joins: Vec<i32> = (0..256).filter(|&id| node_of(fx.stmt, id).is_some_and(|(n, _)| n.as_hash_join().is_some())).collect();
    assert!(
        joins.iter().any(|&id| Proof::pg_fails_too_shared(fx.stmt, id, 1, ctx.mcx()).is_some_and(|p| p.kind() == ProofKind::PgFailsToo && p.node() == id)),
        "the self join of CTE 1: {joins:?}"
    );
    assert!(joins.iter().all(|&id| Proof::pg_fails_too_shared(fx.stmt, id, 9999, ctx.mcx()).is_none()), "no such CTE");
    let others = (0..256).filter(|&id| node_of(fx.stmt, id).is_some_and(|(n, _)| n.as_hash_join().is_none()));
    assert!(others.into_iter().all(|id| Proof::pg_fails_too_shared(fx.stmt, id, 1, ctx.mcx()).is_none()), "no Hash Join, no proof");
}

/// D2117 (b) (lowerv4 subplan/per_row.rs `certain_first`): `Proof::subplan_first` holds for a SubPlan in the target list
/// of the Values Scan the statement's first fetch reaches: groupingsets-047 (a Sort over VALUES (false), (true), its entries
/// `v.c` then the correlated count SubPlan) and create_index_spgist-013 (three constant rows, the ORDER BY .. LIMIT 1
/// SubPlan alone). It cites the SubPlan's plan root. None for another plan id, a first fetch that is no Values Scan
/// (numeric-013's CTE nest), a qual on the scan, an entry before the SubPlan that may raise (1 / 0 in place of `v.c`), a
/// LIMIT 0 over the scan, and a correlated SubPlan over more than one row whose held pipelines its rerun set runs again
/// per row (PostgreSQL interleaves the acts with the rows there). `exact_first` grants code that acts under this proof alone.
#[test]
fn subplan_first_is_the_values_rows_first_code() {
    use gravity_decompose::effects::{exact_first, Cx, Effects};
    let mut sets = 0;
    for name in ["groupingsets-047", "create_index_spgist-013"] {
        let ctx = MemoryContext::new("c2-exact");
        let fx = corpus::load(ctx.mcx(), &text(name)).unwrap();
        let arena = Arena::new(1 << 20);
        let g = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).expect("decomposes");
        let cx = Cx::new(fx.stmt, &fx.facts, arena.handle());
        let p = Proof::subplan_first(&cx, &g, 1, &[]).unwrap_or_else(|| panic!("{name}: SubPlan 1 runs first"));
        let root = fx.stmt.subplans.iter().next().flatten().and_then(|r| r.as_plan()).map(|p| p.plan_node_id);
        assert_eq!((p.kind(), Some(p.node())), (ProofKind::PgRunsFirst, root), "{name}: the SubPlan's plan root is cited");
        assert!(Proof::subplan_first(&cx, &g, 9999, &[]).is_none(), "{name}: no such SubPlan");
        // a SubPlan whose plan is one pipeline has no rerun set (create_index_spgist-013's): groupingsets-047's has one
        let rerun: Vec<usize> = g.reruns.iter().flat_map(|r| r.pipelines.iter().map(|&q| usize::from(q))).collect();
        if let Some(&q) = rerun.first() {
            assert!(Proof::subplan_first(&cx, &g, 1, &[q]).is_none(), "{name}: a pipeline re-run per row held");
            sets += 1;
        }
        let kept: Vec<usize> = (0..g.pipelines.len()).filter(|q| !rerun.contains(q)).collect();
        assert!(Proof::subplan_first(&cx, &g, 1, &kept).is_some(), "{name}: pipelines outside every rerun set held");
        assert!(exact_first(Effects::ACTS, p).is_ok(), "{name}: acting code under the SubPlan-first proof");
        let chain = fx.stmt.planTree.and_then(|n| n.as_plan()).map(|p| p.plan_node_id).and_then(|id| Proof::pg_runs_first(fx.stmt, id)).expect("the root is on the chain");
        assert!(exact_first(Effects::ACTS, chain).is_err() && exact_first(Effects::RAISES, chain).is_ok(), "{name}: any other proof is exact's");
    }
    assert!(sets > 0, "a fixture whose correlated SubPlan has a rerun set (main 00:29:27Z (1))");
    let ctx = MemoryContext::new("c2-exact");
    let fx = corpus::load(ctx.mcx(), &text("numeric-013")).unwrap();
    let arena = Arena::new(1 << 20);
    let g = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).expect("decomposes");
    let cx = Cx::new(fx.stmt, &fx.facts, arena.handle());
    assert!((0..8).all(|id| Proof::subplan_first(&cx, &g, id, &[]).is_none()), "numeric-013: its first fetch is a CteScan");
    // the same statement with one thing changed: each change is a reason PostgreSQL may run other code first, or none
    let base = text("groupingsets-047");
    let at = base.find("{VALUESSCAN").expect("the Values Scan");
    let (head, scan) = base.split_at(at);
    let tl = "{TARGETENTRY :expr {VAR :varno 2 :varattno 1 :vartype 16 :vartypmod -1 :varcollid 0 :varnullingrels (b) :varlevelsup 0 :varreturningtype 0 :varnosyn 2 :varattnosyn 1 :location -1} :resno 1";
    let int4 = |v: u8| format!("{{CONST :consttype 23 :consttypmod -1 :constcollid 0 :constlen 4 :constbyval true :constisnull false :location -1 :constvalue 4 [ {v} 0 0 0 0 0 0 0 ]}}");
    let div = format!("{{TARGETENTRY :expr {{OPEXPR :opno 528 :opfuncid 154 :opresulttype 23 :opretset false :opcollid 0 :inputcollid 0 :args ({} {}) :location -1}} :resno 1", int4(1), int4(0));
    let qual = ":scan.plan.qual ({CONST :consttype 16 :consttypmod -1 :constcollid 0 :constlen 1 :constbyval true :constisnull false :location -1 :constvalue 1 [ 1 0 0 0 0 0 0 0 ]}) :scan.plan.lefttree";
    let root_at = base.find(":planTree {").expect("a plan tree") + ":planTree ".len();
    let root_end = close(&base, root_at);
    let limit0 = format!(
        "{}{{LIMIT :plan.disabled_nodes 0 :plan.startup_cost 0 :plan.total_cost 0 :plan.plan_rows 0 :plan.plan_width 0 :plan.parallel_aware false :plan.parallel_safe false :plan.async_capable false :plan.plan_node_id 90 :plan.targetlist <> :plan.qual <> :plan.lefttree {} :plan.righttree <> :plan.initPlan <> :plan.extParam (b) :plan.allParam (b) :limitOffset <> :limitCount {{CONST :consttype 20 :consttypmod -1 :constcollid 0 :constlen 8 :constbyval true :constisnull false :location -1 :constvalue 8 [ 0 0 0 0 0 0 0 0 ]}} :limitOption 0 :uniqNumCols 0 :uniqColIdx <> :uniqOperators <> :uniqCollations <>}}{}",
        &base[..root_at],
        &base[root_at..=root_end],
        &base[root_end + 1..]
    );
    let changed = [
        ("a qual on the Values Scan", format!("{head}{}", scan.replacen(":scan.plan.qual <> :scan.plan.lefttree", qual, 1))),
        ("an entry before it that may raise", format!("{head}{}", scan.replacen(tl, &div, 1))),
        ("a LIMIT 0 over the scan", limit0),
    ];
    let ctx0 = MemoryContext::new("c2-exact");
    let g0 = corpus::load(ctx0.mcx(), &base).unwrap();
    let arena0 = Arena::new(1 << 20);
    let graph = decompose_in(arena0.handle(), g0.stmt, &g0.facts, g0.mcx).expect("decomposes");
    for (why, t) in changed {
        assert_ne!(t, base, "{why}: the change applies");
        let ctx = MemoryContext::new("c2-exact");
        let fx = corpus::load(ctx.mcx(), &t).unwrap_or_else(|e| panic!("{why}: {e:?}"));
        let arena = Arena::new(1 << 20);
        let cx = Cx::new(fx.stmt, &fx.facts, arena.handle());
        assert!(Proof::subplan_first(&cx, &graph, 1, &[]).is_none(), "{why}: no proof");
    }
}

/// The offset of the `}` closing the node that opens at `open` (a `{`) in a plan's text.
fn close(t: &str, open: usize) -> usize {
    let mut depth = 0usize;
    for (i, c) in t[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return open + i;
                }
            }
            _ => {}
        }
    }
    panic!("an unclosed node at {open}")
}

/// D793-b: the checkout under test, read at run time.
fn manifest_dir() -> &'static std::path::Path {
    static DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::var_os("CARGO_MANIFEST_DIR").map(Into::into).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)"))
}
