//! D779 (main 09-26, CENSUS gap found on regress insert_conflict#13): props walked an ON CONFLICT DO UPDATE's SET list but
//! not its WHERE (onConflictWhere), so the kinds under that WHERE were missing and such statements were placed at M4.
//! - regress-full insert_conflict-001 (`... on conflict (key, fruit) do update set fruit = excluded.fruit where exists
//!   (select 1 from insertconflicttest ii where ii.key = excluded.key)`): the SubPlan under onConflictWhere names
//!   nest.subplan (M8). Since IN-CONFLICT (modifytable) decompose places the WHERE's SubPlan as a step (expr_lists), and
//!   its inner is an index-only scan of the fixture's index 18252, so the fixture is read whole.
//! - regress-full with-084 (`WITH aa AS (SELECT 1 a, 2 b) INSERT INTO withz ... ON CONFLICT (k) DO UPDATE SET v = ' update'
//!   WHERE withz.k = (SELECT a FROM aa)`): the WHERE reads an InitPlan's output, which names nest.initplan (M8). The
//!   ModifyTable's own initPlan list already named it before D779, so this one guards the form, not the defect.
use gravity_lowerv4_it as common;

use std::collections::BTreeSet;

fn kinds(rel: &str, drop: &[&str]) -> BTreeSet<String> {
    let p = common::plans_dir().join(rel);
    let t = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    let t: String = t.lines().filter(|l| !drop.iter().any(|d| l.starts_with(d))).map(|l| format!("{l}\n")).collect();
    common::props_fixture(&t).unwrap_or_else(|| panic!("{rel}: decompose declined")).0
}

#[test]
fn d779_subplan_under_onconflict_where_names_nest_subplan() {
    let k = kinds("regress-full/insert_conflict-001.plan", &[]);
    assert!(k.contains("write.onconflict"), "insert_conflict-001 is an upsert: {k:?}");
    assert!(k.contains("nest.subplan"), "insert_conflict-001's DO UPDATE WHERE EXISTS (...) is a SubPlan: {k:?}");
}

#[test]
fn d779_initplan_read_by_onconflict_where_names_nest_initplan() {
    let k = kinds("regress-full/with-084.plan", &[]);
    assert!(k.contains("write.onconflict"), "with-084 is an upsert: {k:?}");
    assert!(k.contains("nest.initplan"), "with-084's DO UPDATE WHERE reads (SELECT a FROM aa): {k:?}");
}
