// memoize item 5 (the v4 row MEMO-RERUN-R2's captured-plan test, ported): `JoinDesc::Memo.purge` over captured plans.
// PostgreSQL 18.6's ExecReScanMemoize purges the whole cache when the child's changed params hold one outside
// `keyparamids`; decompose resolves that statically (the child's extParam holds a param no key reads and the statement
// does not fix). Every Memoize of every captured plan is checked against an independent reading of the plan text, and two
// plans are pinned: regress memoize#57's EXISTS SubPlan purges (the child reads `t0.ten`); subselect-079's lateral
// recursive CTE does not (its CteScan's allParam holds its own CTE InitPlan's output, which ExecReScan re-derives inside
// the child and never puts in the memo's chgParam). The rest (memoize.sql's joins and LATERALs, every other test's) never
// purge.
use std::fs;
use std::path::{Path, PathBuf};

use gravity_decompose::graph::{Op, PipelineGraph, PipelineNode};
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::subplan::SubForm;
use gravity_decompose::{corpus, decompose_in};
use gravity_program::Arena;
use mcx::MemoryContext;

mod harness;

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

/// The graph's Memo ops' purge facts, in pipeline order.
fn purges(g: &PipelineGraph<'_>) -> Vec<bool> {
    let mut ps = Vec::new();
    for p in g.pipelines.iter() {
        all_pipes(p, &mut ps);
    }
    ps.iter().flat_map(|p| p.ops.iter()).filter_map(|op| if let Op::Join(JoinDesc::Memo { purge, .. }) = op { Some(*purge) } else { None }).collect()
}

/// The ids of the bitmapset field `field` (any prefix: `plan.`, `scan.plan.`, `join.plan.`) of the node opening at `t`'s
/// first `{`, its own fields only (a nested node's are skipped).
fn own_bms(t: &str, field: &str) -> Vec<i32> {
    let (b, mut depth, mut i) = (t.as_bytes(), 0usize, 0usize);
    while i < b.len() {
        match b[i] {
            b'\\' => i += 1,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            b':' if depth == 1 && i > 0 && b[i - 1].is_ascii_whitespace() => {
                let end = t[i..].find(char::is_whitespace).map_or(t.len(), |e| i + e);
                let name = &t[i + 1..end];
                if name == field || name.ends_with(&format!(".{field}")) {
                    let body = t[end..].trim_start().strip_prefix("(b").expect("a bitmapset");
                    return body[..body.find(')').unwrap()].split_whitespace().map(|x| x.parse().unwrap()).collect();
                }
            }
            _ => {}
        }
        i += 1;
    }
    panic!("no {field}")
}

/// The plan text's own answer per Memoize (before statement-fixed params): the child's extParam holds a non-key param.
fn reading(text: &str) -> Vec<bool> {
    text.match_indices("{MEMOIZE").map(|(i, _)| {
        let t = &text[i..];
        let keys = own_bms(t, "keyparamids");
        let child = &t[t.find(":plan.lefttree {").expect("a child") + ":plan.lefttree ".len()..];
        own_bms(child, "extParam").iter().any(|x| !keys.contains(x))
    }).collect()
}

fn plan_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            plan_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "plan") && harness::selected(&p) {
            out.push(p);
        }
    }
}

#[test]
fn a_memo_purges_only_where_its_child_reads_a_param_outside_its_keys() {
    let mut files = Vec::new();
    plan_files(&harness::plans_root(), &mut files);
    files.sort();
    let (mut seen, mut decomposed, mut purging, mut names) = (0, 0, Vec::new(), Vec::new());
    for path in files {
        let text = fs::read_to_string(&path).unwrap();
        if !text.contains("{MEMOIZE") {
            continue;
        }
        seen += 1;
        let name = harness::fixture_name(&path);
        let want = reading(&text);
        let ctx = MemoryContext::new("memo_purge");
        let fx = corpus::load(ctx.mcx(), &text).unwrap_or_else(|e| panic!("{name}: {e}"));
        let arena = Arena::new(1 << 20);
        let Ok(g) = decompose_in(arena.handle(), fx.stmt, &fx.facts, fx.mcx) else { continue };
        let got = purges(&g);
        if got.is_empty() {
            continue; // a Memoize decompose folds away (none today): nothing to compare
        }
        decomposed += 1;
        names.push(name.clone());
        // the plan's Memoize nodes in tree order may differ from the pipelines' order: compare the multisets
        let (mut a, mut b) = (got.clone(), want.clone());
        a.sort();
        b.sort();
        assert_eq!(a, b, "{name}: decompose's purge facts {got:?}, the plan's reading {want:?}");
        if got.iter().any(|&p| p) {
            purging.push(name);
        }
    }
    assert!(seen >= 70, "captured plans holding a Memoize: {seen}");
    assert!(decomposed >= 1, "no captured Memoize decomposed");
    assert_eq!(purging, ["regress-full/memoize-012"], "only memoize#56 / #57's plan purges (the EXISTS SubPlan's child reads t0.ten)");
    assert!(names.iter().any(|n| n == "regress-full/subselect-079"), "subselect-079 (a CteScan child holding its own CTE InitPlan) decomposed and never purges");
}
