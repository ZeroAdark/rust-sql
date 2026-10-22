// AGB-28 / AGC-27 / AGB-22: the facts home's row bound (`catalog::rows`) over captured plans (tests/plans/cte.sql). The
// fixtures carry no operator families, so `Keyed` answers `unique_bound` from the fixture's index lines (unique, not
// deferrable, every key column bound) the way the relcache answers it; `FixtureFacts` alone proves no key (false), and a
// deferrable index proves none. The F5 reading of a scalar SubPlan follows the bound: raise-free over a key probe.
use std::fs;
use std::path::PathBuf;

use gravity_decompose::catalog::{rows, AttFact, ExprFp, Facts, IndexExt, IndexFact, InsteadOf, KeyBind, KeyOrder, KeyReq, LeafFact, RelFact};
use gravity_decompose::corpus::{self, FixtureFacts};
use gravity_decompose::effects::FuncProps;
use gravity_decompose::graph::{Op, PipelineNode};
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::modifytable::trig_facts::TrigExt;
use gravity_decompose::ops::shared::subplan::SubForm;
use gravity_decompose::decompose_in;
use gravity_program::Arena;
use mcx::MemoryContext;
use types_nodes::plannodes::PlannedStmt;

mod harness;

struct Keyed<'a> {
    inner: &'a FixtureFacts,
    immediate: bool,
}

impl Facts for Keyed<'_> {
    fn rel(&self, relid: u32) -> Option<RelFact> {
        self.inner.rel(relid)
    }
    fn att(&self, relid: u32, attno: i16) -> Option<AttFact> {
        self.inner.att(relid, attno)
    }
    fn att_range(&self, relid: u32, attno: i16) -> Option<(i64, i64)> {
        self.inner.att_range(relid, attno)
    }
    fn index(&self, indexid: u32) -> Option<IndexFact> {
        self.inner.index(indexid).map(|i| IndexFact { immediate: i.immediate && self.immediate, ..i })
    }
    fn index_am(&self, indexid: u32) -> gravity_decompose::catalog::IndexAm {
        self.inner.index_am(indexid)
    }
    fn indexes_of(&self, relid: u32, f: &mut dyn FnMut(IndexFact)) {
        self.inner.indexes_of(relid, &mut |i| f(IndexFact { immediate: i.immediate && self.immediate, ..i }))
    }
    fn index_ext(&self, indexid: u32) -> IndexExt {
        self.inner.index_ext(indexid)
    }
    fn att_generated(&self, relid: u32, attno: i16) -> u8 {
        self.inner.att_generated(relid, attno)
    }
    fn key(&self, req: KeyReq) -> Option<KeyOrder> {
        self.inner.key(req)
    }
    fn instead_of(&self, relid: u32) -> InsteadOf {
        self.inner.instead_of(relid)
    }
    fn shred_election(&self, relid: u32, attno: i16, keys: &[&str]) -> Option<(u8, i32)> {
        self.inner.shred_election(relid, attno, keys)
    }
    fn trig_ext(&self, relid: u32) -> TrigExt {
        self.inner.trig_ext(relid)
    }
    fn partition_leaves(&self, root: u32, f: &mut dyn FnMut(LeafFact)) -> bool {
        self.inner.partition_leaves(root, f)
    }
    fn index_expr(&self, indexid: u32, col: u8) -> Option<ExprFp> {
        self.inner.index_expr(indexid, col)
    }
    fn reads_hash_functions(&self) -> bool {
        self.inner.reads_hash_functions()
    }
    fn func(&self, funcid: u32) -> Option<FuncProps> {
        self.inner.func(funcid)
    }
    fn unique_bound(&self, indexid: u32, binds: &[KeyBind], partial_ok: bool) -> bool {
        let (Some(i), e) = (self.index(indexid), self.index_ext(indexid)) else { return false };
        let n = if e.nkeyatts == 0 { i.nkeys } else { e.nkeyatts } as usize;
        i.unique && i.immediate && !e.deferrable && e.valid && (partial_ok || !i.partial) && n > 0 && (0..n).all(|k| binds.iter().any(|b| b.col as usize == k && self.inner.op_eq(b.opno)))
    }
}

fn fixture(sql: &str) -> PathBuf {
    let hits: Vec<PathBuf> = harness::fixtures_in(&harness::plans_root())
        .into_iter()
        .filter(|p| p.file_name().and_then(|f| f.to_str()).is_some_and(|f| f.starts_with("cte-")))
        .filter(|p| fs::read_to_string(p).unwrap().lines().any(|l| l.strip_prefix("-- sql: ").is_some_and(|q| q.trim().trim_end_matches(';') == sql)))
        .collect();
    assert_eq!(hits.len(), 1, "cte fixtures of {sql}: {hits:?}");
    hits.into_iter().next().unwrap()
}

/// The row bound of each subplan's root under the three sources: the fixture alone, keyed, keyed with every index deferrable.
fn bounds(sql: &str) -> Vec<(Option<u64>, Option<u64>, Option<u64>)> {
    let text = fs::read_to_string(fixture(sql)).unwrap();
    let ctx = MemoryContext::new("facts_rows");
    let fx = corpus::load(ctx.mcx(), &text).unwrap();
    let stmt: &PlannedStmt<'_> = fx.stmt;
    let (keyed, deferred) = (Keyed { inner: &fx.facts, immediate: true }, Keyed { inner: &fx.facts, immediate: false });
    stmt.subplans.iter().flatten().map(|r| (rows::max_rows(&fx.facts, stmt, r), rows::max_rows(&keyed, stmt, r), rows::max_rows(&deferred, stmt, r))).collect()
}

/// The F5 raise-free reading of every SubPlan op of the decomposed statement.
fn raise_free(sql: &str, immediate: bool) -> Vec<bool> {
    let text = fs::read_to_string(fixture(sql)).unwrap();
    let ctx = MemoryContext::new("facts_rows");
    let fx = corpus::load(ctx.mcx(), &text).unwrap();
    let keyed = Keyed { inner: &fx.facts, immediate };
    let arena = Arena::new(1 << 20);
    let g = decompose_in(arena.handle(), fx.stmt, &keyed, fx.mcx).unwrap_or_else(|r| panic!("{sql}: refused {r:?}"));
    let mut out = Vec::new();
    fn walk(p: &PipelineNode<'_>, out: &mut Vec<bool>) {
        for op in p.ops.iter() {
            match op {
                Op::SubPlan(d) => {
                    out.push(d.raise_free);
                    if let SubForm::Nested { inner } = &d.form {
                        walk(inner, out);
                    }
                }
                Op::Join(JoinDesc::Nest { inner, .. }) => walk(inner, out),
                _ => {}
            }
        }
    }
    for p in g.pipelines.iter() {
        walk(p, &mut out);
    }
    out
}

#[test]
fn a_key_probe_subquery_is_one_row() {
    // the correlated scalar subquery by primary key, a constant key, a row comparison and a MULTIEXPR by key
    for sql in [
        "SELECT a, (SELECT z FROM u WHERE u.x = t.a) FROM t WHERE a < 3",
        "SELECT a FROM t WHERE b = (SELECT y FROM u WHERE x = 7)",
        "SELECT a FROM t WHERE (a, b) = (SELECT x, y FROM u WHERE x = 7)",
        "UPDATE u SET (y, z) = (SELECT b, c FROM t WHERE t.a = u.x) WHERE x = 5",
    ] {
        let b = bounds(sql);
        assert!(!b.is_empty(), "{sql}: no subplan");
        for (alone, keyed, deferred) in b {
            assert_eq!((alone, keyed, deferred), (None, Some(1), None), "{sql}");
        }
    }
}

#[test]
fn the_structural_roots_keep_their_bound() {
    for sql in [
        "SELECT a, (SELECT count(*) FROM u WHERE u.y = t.b) FROM t WHERE a < 3",
        "SELECT a, (SELECT y FROM u WHERE u.x > t.a ORDER BY y LIMIT 1) FROM t WHERE a < 3",
        "SELECT (SELECT a FROM t WHERE b = 1 ORDER BY a LIMIT 1) AS first, x FROM u WHERE x < 3",
    ] {
        for (alone, keyed, deferred) in bounds(sql) {
            assert_eq!((alone, keyed, deferred), (Some(1), Some(1), Some(1)), "{sql}");
        }
    }
}

#[test]
fn a_range_or_a_non_unique_key_has_no_bound() {
    let sql = "SELECT a, ARRAY(SELECT y FROM u WHERE u.x = t.a) FROM t WHERE a < 3";
    assert!(bounds(sql).iter().all(|&(a, k, _)| a.is_none() && k == Some(1)), "{sql}");
    let text = fs::read_to_string(fixture("WITH c AS (SELECT a, b FROM t WHERE b = 1) SELECT a FROM c")).unwrap();
    let ctx = MemoryContext::new("facts_rows");
    let fx = corpus::load(ctx.mcx(), &text).unwrap();
    let keyed = Keyed { inner: &fx.facts, immediate: true };
    assert_eq!(rows::max_rows(&keyed, fx.stmt, fx.stmt.planTree.unwrap()), None);
}

#[test]
fn f5_reads_a_key_probe_subplan_raise_free() {
    let sql = "SELECT a, (SELECT z FROM u WHERE u.x = t.a) FROM t WHERE a < 3";
    assert_eq!(raise_free(sql, true), vec![true], "{sql}: keyed");
    assert_eq!(raise_free(sql, false), vec![false], "{sql}: deferrable");
}
