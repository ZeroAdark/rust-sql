// facts-r6-2 (main 08:49Z; M8-SUB's nest.subplan e2e on box 72):
//   - D1045: `expr::type_of` / `expr::collation_of` of a SubPlan are PostgreSQL's exprType / exprCollation per subLinkType:
//     EXPR the first column's type, ARRAY its promoted array type, MULTIEXPR record, EXISTS / ANY / ALL / ROWCOMPARE / CTE
//     boolean; the first column's collation for EXPR / ARRAY only. Checked on every SubPlan node of every fixture (the plan
//     trees' target lists, quals and InitPlan lists), each subLinkType met at least once;
//   - a SubPlan a ProjectSet's plain target entry reads is placed before the Srf op (M8-SUB 09-27);
//   - D1047 (INTERIM, main 09:55Z): a pipeline that reads a placed SubPlan with no op of it before the read refuses by name;
use std::collections::BTreeMap;
use std::fs;

use gravity_decompose::catalog::expr_type::{collation_of, type_of};
use gravity_decompose::effects::builtin_promoted_array;
use gravity_decompose::{corpus, rerun as rescan};
use mcx::MemoryContext;
use types_nodes::primnodes::SubLinkType;
use types_nodes::Node;

mod harness;

/// PostgreSQL 18.6 nodeFuncs.c exprType / exprCollation of a SubPlan, from its fields (`None`: an ARRAY over a type with
/// no builtin array type).
fn pg(s: &types_nodes::primnodes::SubPlan<'_>) -> (Option<u32>, u32) {
    match s.subLinkType {
        SubLinkType::EXPR_SUBLINK => (Some(s.firstColType), s.firstColCollation),
        SubLinkType::ARRAY_SUBLINK => (builtin_promoted_array(s.firstColType), s.firstColCollation),
        SubLinkType::MULTIEXPR_SUBLINK => (Some(2249), 0),
        _ => (Some(16), 0),
    }
}

fn subplans<'p>(n: Node<'p>, f: &mut dyn FnMut(Node<'p>)) {
    gravity_decompose::expr::walk(n, &mut |x| {
        if x.as_sub_plan().is_some() {
            f(x);
        }
        true
    });
}

#[test]
fn a_subplans_type_and_collation_are_postgresqls_per_sublink_type() {
    let mut files = harness::fixtures_in(&harness::plans_root());
    files.extend(harness::fixtures_in(&harness::plans_root().join("regress-full")));
    let rows = harness::par_map(&files, |path| {
        let text = fs::read_to_string(path).unwrap();
        let ctx = MemoryContext::new("subplan_types");
        let Ok(fx) = corpus::load(ctx.mcx(), &text) else { return (BTreeMap::new(), Vec::new()) };
        let (mut seen, mut bad) = (BTreeMap::new(), Vec::new());
        let mut check = |x: Node<'_>| {
            let s = x.as_sub_plan().unwrap();
            let (ty, coll) = pg(s);
            *seen.entry(format!("{:?}", s.subLinkType)).or_insert(0usize) += 1;
            // pins independent of the table: an ARRAY over int4 / text is int4[] / text[], a test sublink is bool
            let pin = match (s.subLinkType, s.firstColType) {
                (SubLinkType::ARRAY_SUBLINK, 23) => Some(1007),
                (SubLinkType::ARRAY_SUBLINK, 25) => Some(1009),
                (SubLinkType::EXISTS_SUBLINK | SubLinkType::ANY_SUBLINK | SubLinkType::ALL_SUBLINK, _) => Some(16),
                _ => None,
            };
            if pin.is_some_and(|p| type_of(x) != Some(p)) {
                bad.push(format!("{}: subplan {} {:?} over {}: type {:?}, pinned {pin:?}", path.display(), s.plan_id, s.subLinkType, s.firstColType, type_of(x)));
            }
            if type_of(x) != ty || collation_of(x) != coll {
                bad.push(format!("{}: subplan {} {:?}: type {:?} coll {}, PostgreSQL {ty:?} {coll}", path.display(), s.plan_id, s.subLinkType, type_of(x), collation_of(x)));
            }
        };
        rescan::each_plan_node(fx.stmt, &mut |n| {
            let Some(p) = n.as_plan() else { return };
            for x in p.initPlan.iter() {
                check(x);
            }
            for e in p.targetlist.iter().chain(p.qual.iter()) {
                subplans(e, &mut check);
            }
        });
        (seen, bad)
    });
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    for (s, _) in rows.iter() {
        for (k, v) in s {
            *seen.entry(k.clone()).or_insert(0) += v;
        }
    }
    eprintln!("SubPlans by subLinkType: {seen:?}");
    for t in ["EXISTS_SUBLINK", "ALL_SUBLINK", "ANY_SUBLINK", "ROWCOMPARE_SUBLINK", "EXPR_SUBLINK", "MULTIEXPR_SUBLINK", "ARRAY_SUBLINK", "CTE_SUBLINK"] {
        assert!(seen.get(t).is_some_and(|&n| n > 0), "no fixture holds a {t} SubPlan: {seen:?}");
    }
    let bad: Vec<&String> = rows.iter().flat_map(|r| r.1.iter()).collect();
    assert!(bad.is_empty(), "{bad:#?}");
}

/// M8-SUB 09-27: a SubPlan a ProjectSet's plain target entry reads is placed before the Srf op (lowerv4's srf.projectset
/// evaluates those entries per output row and in the call that finds every set ended, before the projection). Over every
/// fixture, and at least one fixture has the shape.
#[test]
fn a_projectsets_subplan_op_comes_before_its_srf_op() {
    use gravity_decompose::graph::Op;
    let mut files = harness::fixtures_in(&harness::plans_root());
    files.extend(harness::fixtures_in(&harness::plans_root().join("regress-full")));
    let rows = harness::par_map(&files, |path| {
        let text = fs::read_to_string(path).unwrap();
        let ctx = MemoryContext::new("subplan_types");
        let Ok(fx) = corpus::load(ctx.mcx(), &text) else { return (0, Vec::new()) };
        let arena = gravity_program::Arena::new(1 << 20);
        let Ok(g) = gravity_decompose::decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx) else { return (0, Vec::new()) };
        let (mut n, mut bad) = (0, Vec::new());
        for (k, p) in g.pipelines.iter().enumerate() {
            for (i, op) in p.ops.iter().enumerate() {
                let Op::Srf(s) = op else { continue };
                if s.place != gravity_decompose::ops::shared::srf::SrfPlace::TargetList {
                    continue;
                }
                let mut ids = Vec::new();
                for e in s.tlist.iter() {
                    subplans(e, &mut |x| ids.push(x.as_sub_plan().unwrap().plan_id));
                }
                for id in ids {
                    n += 1;
                    if !p.ops[..i].iter().any(|o| matches!(o, Op::SubPlan(d) if d.plan_id == id)) {
                        bad.push(format!("{}: p{k} Srf op {i} reads subplan {id} with no op before it", path.display()));
                    }
                }
            }
        }
        (n, bad)
    });
    let n: usize = rows.iter().map(|r| r.0).sum();
    assert!(n > 0, "no fixture has a SubPlan in a ProjectSet's target list");
    let bad: Vec<&String> = rows.iter().flat_map(|r| r.1.iter()).collect();
    assert!(bad.is_empty(), "{bad:#?}");
}

/// D1047: the fixtures a pipeline reads a placed SubPlan in with no op of it before the read refuse by name, and none
/// does: a grouping set's arm places its own qual / target-list SubPlans since item 11 (d), and a draining hash join's
/// probe qual SubPlans go on its probe (cte-049, the last one, served since). A reader inside an aggregate's arguments is
/// no read on the emit side (the op sits on the input side). The check stays the structural guard.
#[test]
fn a_subplan_read_with_no_op_of_it_before_the_read_refuses_by_name() {
    let mut files = harness::fixtures_in(&harness::plans_root());
    files.extend(harness::fixtures_in(&harness::plans_root().join("regress-full")));
    let rows = harness::par_map(&files, |path| {
        let text = fs::read_to_string(path).unwrap();
        let ctx = MemoryContext::new("subplan_types");
        let Ok(fx) = corpus::load(ctx.mcx(), &text) else { return None };
        let arena = gravity_program::Arena::new(1 << 20);
        let r = gravity_decompose::decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).err()?;
        r.to_string().contains("a SubPlan read in a pipeline with no op of it before the read").then(|| harness::fixture_name(path))
    });
    let mut got: Vec<String> = rows.into_iter().flatten().collect();
    got.sort();
    assert!(got.is_empty(), "{got:?}");
}
