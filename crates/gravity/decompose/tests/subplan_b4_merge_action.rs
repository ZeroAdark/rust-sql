//! MAIN 09:14Z (modifytable's mergecmd B4): a SubPlan in a MERGE action's WHEN qual or target list is conditional
//! (`SubPlanDesc::conditional`, ops/subplan `attach`): PostgreSQL 18.6 reaches an action's lists only for a row of its kind
//! that no earlier action took (ExecMergeMatched / ExecMergeNotMatched), its target list only once its qual held, so a
//! step evaluating the SubPlan for every row reaching the write would call it for rows PG never reads it on. Positives
//! (captured over tests/plans/cte.sql's t / u by scripts/gravity-capture-plans.sh, tests/subplan_b4/*.plan): an EXISTS
//! past a WHEN MATCHED qual's first conjunct (merge-when), an UPDATE SET value (merge-set), a NOT MATCHED INSERT value
//! (merge-insert). Negatives: RETURNING's SubPlan, read for every written row (merge-returning), beside an action's
//! conditional one in one statement (merge-when-returning); nest.subplan keeps B4 for it (lowerv4 per_row.rs).
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use gravity_decompose::graph::{Op, PipelineNode};
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::subplan::SubForm;
use gravity_decompose::{corpus, decompose_in};
use gravity_program::Arena;
use mcx::MemoryContext;

fn manifest_dir() -> PathBuf {
    std::env::var_os("CARGO_MANIFEST_DIR").map(Into::into).expect("CARGO_MANIFEST_DIR unset: run under cargo test (D793-b)")
}

/// Every pipeline, embedded inners included (facts_m8.rs `all_pipes`).
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

/// Fixture `name`'s SubPlan ops: plan id -> conditional.
fn conditional(name: &str) -> BTreeMap<i32, bool> {
    let path = manifest_dir().join("tests/subplan_b4").join(format!("{name}.plan"));
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let ctx = MemoryContext::new("subplan_b4");
    let fx = corpus::load(ctx.mcx(), &text).unwrap_or_else(|e| panic!("{name}: {e}"));
    let arena = Arena::new(1 << 20);
    let g = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx).unwrap_or_else(|r| panic!("{name}: refused {r}"));
    let mut pipes = Vec::new();
    for p in g.pipelines.iter() {
        all_pipes(p, &mut pipes);
    }
    let mut out = BTreeMap::new();
    for p in pipes {
        for op in p.ops.iter() {
            if let Op::SubPlan(d) = op {
                out.insert(d.plan_id, d.conditional);
            }
        }
    }
    out
}

#[test]
fn a_merge_actions_subplan_is_conditional() {
    let cases: [(&str, &[(i32, bool)]); 5] = [
        ("merge-when", &[(1, true)]),
        ("merge-set", &[(1, true)]),
        ("merge-insert", &[(1, true)]),
        ("merge-returning", &[(1, false)]),
        ("merge-when-returning", &[(1, false), (2, true)]),
    ];
    let mut bad = Vec::new();
    for (name, want) in cases {
        let got: Vec<(i32, bool)> = conditional(name).into_iter().collect();
        println!("subplan_b4 conditional\t{name}\t{got:?}");
        if got != want {
            bad.push(format!("{name}: SubPlans (plan id, conditional) {got:?}, want {want:?}"));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}
