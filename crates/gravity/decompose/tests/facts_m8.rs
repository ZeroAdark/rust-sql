// FACTS-1a (M8-plan §7.1; main 09-27): the facts M8's nest kinds read, each with a holds / does-not-hold pair over captured
// plans (tests/plans/cte.sql's FACTS-1a section, scripts/gravity-capture-plans.sh cte):
//   - an InitPlan binds EVERY exec parameter of its first row (`InitPlan::params` = the plan's setParam, in column order):
//     a two-column ROWCOMPARE and an uncorrelated MULTIEXPR UPDATE carry two, a scalar one;
//   - `SubPlanDesc::conditional`: PostgreSQL may skip the SubPlan for a row that reaches its op (past the first WHEN test,
//     the first COALESCE / AND / OR argument, a row comparison's first column, a FILTERed aggregate's arguments, a join
//     qual past its first conjunct), never for the first WHEN test or a plan qual's conjunct (its own filter op, the
//     SubPlan op right before its reader);
//   - a plan's qual conjuncts keep their order as filter ops, and a conjunct with a SubPlan is always one (main 09-27
//     condition (a): why a qual conjunct's SubPlan is unconditional; e2e/gravity/v4/m8_lazy.sql shows PG's call counts);
//   - F3: a nest carries its NestLoop's inner_unique;
//   - Decision 16 (closed: no Gather in Gravity plans): decompose refuses Gather and Gather Merge by kind.
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;

use gravity_decompose::graph::{Op, PipelineGraph, PipelineNode};
use gravity_decompose::join::JoinDesc;
use gravity_decompose::admit::Refusal;
use gravity_decompose::ops::shared::subplan::{SubForm, SubLink};
use gravity_decompose::{corpus, decompose_in, ledger, Kind};
use gravity_program::Arena;
use mcx::MemoryContext;
use types_nodes::Node;

mod harness;

/// The cte fixture whose `-- sql:` statement is `sql` (exactly one).
fn fixture(sql: &str) -> PathBuf {
    let hits: Vec<PathBuf> = harness::fixtures_in(&harness::plans_root())
        .into_iter()
        .filter(|p| p.file_name().and_then(|f| f.to_str()).is_some_and(|f| f.starts_with("cte-")))
        .filter(|p| fs::read_to_string(p).unwrap().lines().any(|l| l.strip_prefix("-- sql: ").is_some_and(|q| q.trim().trim_end_matches(';') == sql)))
        .collect();
    assert_eq!(hits.len(), 1, "cte fixtures of {sql}: {hits:?} (re-capture: scripts/gravity-capture-plans.sh cte)");
    hits.into_iter().next().unwrap()
}

/// Decompose fixture `path` and hand the graph (or the refusal) and the statement to `f`.
fn with_graph<R>(path: &PathBuf, f: impl FnOnce(Result<&PipelineGraph<'_>, &Refusal>, &types_nodes::plannodes::PlannedStmt<'_>) -> R) -> R {
    let text = fs::read_to_string(path).unwrap();
    let ctx = MemoryContext::new("facts_m8");
    let fx = corpus::load(ctx.mcx(), &text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let arena = Arena::new(1 << 20);
    let g = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx);
    f(g.as_ref(), fx.stmt)
}

/// Every pipeline, embedded inners included.
fn all_pipes<'a, 'p>(p: &'a PipelineNode<'p>, out: &mut Vec<&'a PipelineNode<'p>>) {
    out.push(p);
    for op in p.ops.iter() {
        match op {
            Op::Join(JoinDesc::Nest { inner, .. }) => all_pipes(inner, out),
            Op::SubPlan(d) => {
                if let SubForm::Nested { inner } = &d.form {
                    all_pipes(inner, out);
                }
            }
            _ => {}
        }
    }
}

fn pipes<'a, 'p>(g: &'a PipelineGraph<'p>) -> Vec<&'a PipelineNode<'p>> {
    let mut out = Vec::new();
    for p in g.pipelines.iter() {
        all_pipes(p, &mut out);
    }
    out
}

#[test]
fn an_init_plan_carries_every_set_param() {
    for (sql, link, n) in [
        ("SELECT a FROM t WHERE (a, b) = (SELECT x, y FROM u WHERE x = 7)", SubLink::RowCompare, 2),
        ("UPDATE u SET (y, z) = (SELECT max(b), min(c) FROM t) WHERE x = 6", SubLink::MultiExpr, 2),
        ("SELECT a FROM t WHERE a < 3 AND (SELECT count(*) FROM u) > 10", SubLink::Expr, 1),
    ] {
        with_graph(&fixture(sql), |g, stmt| {
            let g = g.unwrap_or_else(|r| panic!("{sql}: refused {r}"));
            assert_eq!(g.init_plans.len(), 1, "{sql}");
            let ip = &g.init_plans[0];
            let set = stmt.subplans.iter().flatten().count(); // the statement's subplans, for the message
            let params: Vec<i32> = ip.params.iter().collect();
            assert_eq!((ip.link, params.len()), (link, n), "{sql}: {params:?} ({set} subplans)");
            assert_eq!(ip.param, params[0], "{sql}: `param` is the first of `params`");
            let mut distinct = params.clone();
            distinct.dedup();
            assert_eq!(distinct, params, "{sql}: one exec parameter per column");
        });
    }
}

/// Each statement's SubPlan ops by plan id: (conditional, whether a filter op runs before it in its pipeline).
fn subplans(sql: &str) -> BTreeMap<i32, (bool, bool)> {
    with_graph(&fixture(sql), |g, _| {
        let g = g.unwrap_or_else(|r| panic!("{sql}: refused {r}"));
        let mut out = BTreeMap::new();
        for p in pipes(g) {
            for (i, op) in p.ops.iter().enumerate() {
                if let Op::SubPlan(d) = op {
                    let filtered = p.ops[..i].iter().any(|o| matches!(o, Op::Filter(_)));
                    out.insert(d.plan_id, (d.conditional, filtered));
                }
            }
        }
        out
    })
}

#[test]
fn a_conditional_subplan_is_one_postgres_may_skip() {
    // (statement, its SubPlans: plan id -> conditional)
    let cases: [(&str, &[(i32, bool)]); 8] = [
        ("SELECT a, CASE WHEN a > 1 THEN (SELECT count(*) FROM u WHERE u.y = t.b) ELSE 0 END FROM t WHERE a < 4", &[(1, true)]),
        ("SELECT a, CASE WHEN (SELECT count(*) FROM u WHERE u.y = t.b) > 30 THEN 1 ELSE 0 END FROM t WHERE a < 4", &[(1, false)]),
        ("SELECT a, COALESCE(NULLIF(a, 2), (SELECT max(x) FROM u WHERE u.y = t.b)) FROM t WHERE a < 4", &[(2, true)]),
        ("SELECT a FROM t WHERE a < 4 AND (b = 1 OR EXISTS (SELECT 1 FROM u WHERE u.y = t.b AND u.x > 1990))", &[(2, true)]),
        ("SELECT a FROM t WHERE a < 4 AND ROW(a, (SELECT min(x) FROM u WHERE u.y = t.b)) < ROW(3, 5)", &[(2, true)]),
        ("SELECT b, sum((SELECT count(*) FROM u WHERE u.y = t.b)) FILTER (WHERE a > 2) FROM t WHERE a < 6 GROUP BY b", &[(1, true)]),
        // a join qual past its first conjunct: a nested loop's join qual (cte.sql's facts-r6-3 statement; the hash right
        // join form below is placed on its probe)
        ("SELECT t.a, u.x FROM t JOIN u ON u.x + 0 = t.a + 0 AND u.z <> t.c AND (t.b = 1 OR EXISTS (SELECT 1 FROM u u2 WHERE u2.y = t.b AND u2.z = u.z))", &[(2, true)]),
        ("SELECT a FROM t WHERE a < 3 AND b < (SELECT y FROM u WHERE u.x = t.a) AND c <> 'x'", &[(1, false)]),
    ];
    let mut bad = Vec::new();
    for (sql, want) in cases {
        let got = subplans(sql);
        let got_c: Vec<(i32, bool)> = got.iter().map(|(k, v)| (*k, v.0)).collect();
        println!("facts_m8 conditional\t{sql}\t{got:?}");
        if got_c != want {
            bad.push(format!("{sql}: SubPlans (plan id, conditional) {got_c:?}, want {want:?}"));
        }
    }
    // D1047 (the placement gap, its INTERIM refusal closed): a right join's probe qual SubPlan goes on the probe pipeline,
    // before the probe that reads it per candidate pair (join.rs map_hash_join's draining form), conditional past the
    // qual's first conjunct
    let rj = "SELECT t.a, u.x FROM t LEFT JOIN u ON u.x = t.a AND u.z <> t.c AND EXISTS (SELECT 1 FROM u u2 WHERE u2.y = t.b AND u2.z = u.z) WHERE t.a < 4";
    let got = subplans(rj);
    if got.is_empty() || !got.values().all(|v| v.0) {
        bad.push(format!("{rj}: SubPlans (plan id, (conditional, filtered)) {got:?}, want each placed and conditional"));
    }
    // a qual conjunct's SubPlan runs after the conjuncts before it: its op follows their filter ops
    let q = subplans("SELECT a FROM t WHERE a < 3 AND b < (SELECT y FROM u WHERE u.x = t.a) AND c <> 'x'");
    if !q.get(&1).is_some_and(|v| v.1) {
        bad.push(format!("the qual's SubPlan op does not follow the cheaper conjunct's filter: {q:?}"));
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

/// Every plan node of `n`'s tree (children, Append / MergeAppend / BitmapAnd / BitmapOr members, a SubqueryScan's subplan).
fn plan_nodes<'p>(n: Node<'p>, out: &mut Vec<Node<'p>>) {
    let Some(p) = n.as_plan() else { return };
    out.push(n);
    for c in [p.lefttree, p.righttree].into_iter().flatten() {
        plan_nodes(c, out);
    }
    let lists = [n.as_append().map(|a| &a.appendplans), n.as_merge_append().map(|m| &m.mergeplans), n.as_bitmap_and().map(|b| &b.bitmapplans), n.as_bitmap_or().map(|b| &b.bitmapplans)];
    for l in lists.into_iter().flatten() {
        for c in l.iter() {
            plan_nodes(c, out);
        }
    }
    if let Some(s) = n.as_subquery_scan().and_then(|s| s.subplan) {
        plan_nodes(s, out);
    }
}

/// Index-only scan `n`'s qual conjunct `q` as its heap range form reads it (decompose closure.rs `index_only_full`): each
/// `INDEX_VAR` column `k` as the index target list's entry `k`, a column of the relation. `None` on another node, or when
/// an entry is no plain column.
fn heap_form<'p>(mcx: mcx::Mcx<'p>, n: Node<'p>, q: Node<'p>) -> Option<Node<'p>> {
    fn rw<'p>(mcx: mcx::Mcx<'p>, x: Node<'p>, cols: &[&'p types_nodes::primnodes::Var]) -> types_error::PgResult<Option<Node<'p>>> {
        if let Some(v) = x.as_var().filter(|v| v.varno == types_nodes::primnodes::INDEX_VAR) {
            let Some(c) = usize::try_from(v.varattno).ok().and_then(|a| a.checked_sub(1)).and_then(|i| cols.get(i)) else { return Ok(None) };
            return Node::mk(mcx, types_nodes::primnodes::Var { varnullingrels: c.varnullingrels.clone_in(mcx)?, ..**c }).map(Some);
        }
        nodes_core::expression_tree_mutator_dyn(mcx, x, &mut |y| rw(mcx, y, cols))
    }
    let ios = n.as_index_only_scan()?;
    let cols: Vec<&'p types_nodes::primnodes::Var> = ios.indextlist.iter().map(|t| t.as_target_entry().and_then(|t| t.expr.as_var())).collect::<Option<_>>()?;
    rw(mcx, q, &cols).ok().flatten()
}

/// main 09-27 condition (a) on `SubPlanDesc::conditional`: decompose never reorders a plan's qual conjuncts. Over every
/// captured fixture, the conjuncts of a plan node's qual that become filter ops sit in one pipeline in the qual list's
/// order, and a conjunct holding a SubPlan always becomes a filter op (never folded into a source, where the SubPlan op
/// could not sit between it and the conjuncts before it).
#[test]
fn a_plans_qual_conjuncts_keep_their_order() {
    let mut files = harness::fixtures_in(&harness::plans_root());
    files.extend(harness::fixtures_in(&harness::plans_root().join("regress-full")));
    let rows = harness::par_map(&files, |path| {
        let text = fs::read_to_string(path).unwrap();
        let ctx = MemoryContext::new("facts_m8");
        let Ok(fx) = corpus::load(ctx.mcx(), &text) else { return (0, 0, Vec::new()) };
        let arena = Arena::new(1 << 20);
        let Ok(g) = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx) else { return (0, 0, Vec::new()) };
        let ps = pipes(&g);
        // a conjunct normalize rewrote is its canonical form in the graph (equal, not the same node); two conjuncts can
        // share a canonical form (`1 < a AND a > 1`), so each filter op is claimed once, the first unclaimed one
        let claimed = std::cell::RefCell::new(BTreeSet::new());
        let at = |q: Node<'_>| {
            let c = gravity_decompose::normalize::expr(fx.mcx, &fx.facts, q).unwrap();
            let mut claimed = claimed.borrow_mut();
            let hit = ps.iter().enumerate().find_map(|(k, p)| {
                p.ops.iter().enumerate().position(|(i, o)| !claimed.contains(&(k, i)) && matches!(o, Op::Filter(n) if n.ptr_eq(q) || types_nodes::equal(*n, c))).map(|i| (k, i))
            });
            if let Some(h) = hit {
                claimed.insert(h);
            }
            hit
        };
        let mut nodes = Vec::new();
        if let Some(t) = fx.stmt.planTree {
            plan_nodes(t, &mut nodes);
        }
        for t in fx.stmt.subplans.iter().flatten() {
            plan_nodes(t, &mut nodes);
        }
        let (mut lists, mut subs, mut bad) = (0, 0, Vec::new());
        for n in nodes {
            let qual = &n.as_plan().unwrap().qual;
            // an index-only scan read as the heap range filters its conjuncts over the relation's columns
            let found: Vec<(Node<'_>, Option<(usize, usize)>)> = qual.iter().map(|q| (q, at(q).or_else(|| heap_form(fx.mcx, n, q).and_then(|h| at(h))))).collect();
            for (q, pos) in found.iter() {
                if gravity_decompose::expr::has_subplan(*q) {
                    subs += 1;
                    if pos.is_none() {
                        bad.push(format!("{}: node {}: a qual conjunct with a SubPlan is no filter op", path.display(), n.as_plan().unwrap().plan_node_id));
                    }
                }
            }
            let placed: Vec<(usize, usize)> = found.iter().filter_map(|f| f.1).collect();
            if placed.len() >= 2 {
                lists += 1;
                if placed.windows(2).any(|w| w[0].0 != w[1].0 || w[0].1 >= w[1].1) {
                    bad.push(format!("{}: node {}: qual conjuncts as filter ops at {placed:?}, not in the qual's order", path.display(), n.as_plan().unwrap().plan_node_id));
                }
            }
        }
        (lists, subs, bad)
    });
    let (lists, subs): (usize, usize) = rows.iter().fold((0, 0), |a, r| (a.0 + r.0, a.1 + r.1));
    let bad: Vec<&String> = rows.iter().flat_map(|r| r.2.iter()).collect();
    eprintln!("facts_m8 qual order: {lists} multi-conjunct quals as filter ops, {subs} conjuncts with a SubPlan");
    assert!(bad.is_empty(), "{} quals:\n{bad:#?}", bad.len());
    assert!(harness::filter().is_some() || (lists > 0 && subs > 0), "no qual to check ({lists} / {subs})");
}

/// Every NestLoop of `n` (the plan tree below it): inner root's node id -> inner_unique.
fn nest_loops(n: Node<'_>, out: &mut BTreeMap<i32, bool>) {
    let Some(p) = n.as_plan() else { return };
    if let Some(nl) = n.as_nest_loop() {
        if let Some(inner) = p.righttree.and_then(|r| r.as_plan()) {
            out.insert(inner.plan_node_id, nl.join.inner_unique);
        }
    }
    for c in [p.lefttree, p.righttree].into_iter().flatten() {
        nest_loops(c, out);
    }
    let lists = [n.as_append().map(|a| &a.appendplans), n.as_merge_append().map(|m| &m.mergeplans), n.as_bitmap_and().map(|b| &b.bitmapplans), n.as_bitmap_or().map(|b| &b.bitmapplans)];
    for l in lists.into_iter().flatten() {
        for c in l.iter() {
            nest_loops(c, out);
        }
    }
    if let Some(s) = n.as_subquery_scan().and_then(|s| s.subplan) {
        nest_loops(s, out);
    }
}

/// F3 (M8-plan §7.1, built): every nest op carries the plan's `inner_unique` of its NestLoop (nest.loop's single match),
/// over every captured fixture; both values occur.
#[test]
fn a_nest_carries_its_nest_loops_inner_unique() {
    let mut files = harness::fixtures_in(&harness::plans_root());
    files.extend(harness::fixtures_in(&harness::plans_root().join("regress-full")));
    let rows = harness::par_map(&files, |path| {
        let text = fs::read_to_string(path).unwrap();
        let ctx = MemoryContext::new("facts_m8");
        let Ok(fx) = corpus::load(ctx.mcx(), &text) else { return (0, 0, Vec::new()) };
        let arena = Arena::new(1 << 20);
        let Ok(g) = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx) else { return (0, 0, Vec::new()) };
        let mut want = BTreeMap::new();
        if let Some(t) = fx.stmt.planTree {
            nest_loops(t, &mut want);
        }
        for t in fx.stmt.subplans.iter().flatten() {
            nest_loops(t, &mut want);
        }
        let (mut yes, mut no, mut bad) = (0, 0, Vec::new());
        for p in pipes(&g) {
            for op in p.ops.iter() {
                if let Op::Join(JoinDesc::Nest { unique, inner_node, .. }) = op {
                    match want.get(inner_node) {
                        Some(u) if u == unique => {
                            yes += *unique as usize;
                            no += !*unique as usize;
                        }
                        other => bad.push(format!("{}: nest inner=n{inner_node} unique={unique}, the plan's NestLoop {other:?}", path.display())),
                    }
                }
            }
        }
        (yes, no, bad)
    });
    let (yes, no): (usize, usize) = rows.iter().fold((0, 0), |a, r| (a.0 + r.0, a.1 + r.1));
    let bad: Vec<&String> = rows.iter().flat_map(|r| r.2.iter()).collect();
    eprintln!("facts_m8 F3: {yes} unique nests, {no} not");
    assert!(bad.is_empty(), "{bad:#?}");
    assert!(harness::filter().is_some() || (yes > 0 && no > 0), "F3 needs both an inner_unique nest and one that is not ({yes} / {no})");
}

/// The index of the `}` closing the node whose `{` is at `open` (outfuncs escapes a brace inside a token with `\\`).
fn closing(s: &str, open: usize) -> usize {
    let (b, mut depth, mut i) = (s.as_bytes(), 0usize, open);
    loop {
        match b[i] {
            b'\\' => i += 1,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return i;
                }
            }
            _ => {}
        }
        i += 1;
    }
}

/// Decision 16 (closed, main 09-27): Gravity plans have no Gather. pgrust's planner never builds one (planner lib.rs
/// `run.assess_parallel = false`, decision 6): the two fixtures were captured with every parallel cost at zero and
/// decompose them plain. Their root's input wrapped in a Gather / Gather Merge node (outfuncs' fields, stmt_plan.rs) is a
/// plan decompose refuses by the node's kind (walk.rs), the ledger's Excluded row (tests/ledger.rs), never a Shape.
#[test]
fn gather_and_gather_merge_refuse_by_kind() {
    let base = " :plan.disabled_nodes 0 :plan.startup_cost 0 :plan.total_cost 1 :plan.plan_rows 1 :plan.plan_width 0 :plan.parallel_aware false :plan.parallel_safe false :plan.async_capable false :plan.plan_node_id 90 :plan.targetlist <> :plan.qual <> :plan.lefttree ";
    let rest = " :plan.righttree <> :plan.initPlan <> :plan.extParam (b) :plan.allParam (b)";
    for (sql, kind, head, tail) in [
        ("SELECT count(*) FROM t", Kind::Gather, "{GATHER", " :num_workers 2 :rescan_param -1 :single_copy false :invisible false :initParam (b)}"),
        ("SELECT a FROM t WHERE c > 'c1' ORDER BY b LIMIT 5", Kind::GatherMerge, "{GATHERMERGE", " :num_workers 2 :rescan_param -1 :numCols 0 :sortColIdx () :sortOperators () :collations () :nullsFirst () :initParam (b)}"),
    ] {
        let path = fixture(sql);
        with_graph(&path, |g, _| assert!(g.is_ok(), "{sql}: the captured plan (no Gather) refuses: {:?}", g.err().map(|r| r.to_string())));
        let text = fs::read_to_string(&path).unwrap();
        let at = text.find(":plan.lefttree {").expect("the root's input") + ":plan.lefttree ".len();
        let end = closing(&text, at);
        let wrapped = format!("{}{head}{base}{}{rest}{tail}{}", &text[..at], &text[at..=end], &text[end + 1..]);
        let ctx = MemoryContext::new("facts_m8");
        let fx = corpus::load(ctx.mcx(), &wrapped).unwrap_or_else(|e| panic!("{sql}: the wrapped plan does not read: {e}"));
        let arena = Arena::new(1 << 20);
        match decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx) {
            Ok(_) => panic!("{sql}: decomposed under a {kind:?}"),
            Err(r) => {
                assert_eq!(r.kind, kind, "{sql}: refused {r}");
                assert_eq!(r.reason, ledger::reason(kind), "{sql}: a refusal by kind, not a shape: {r}");
            }
        };
    }
}
