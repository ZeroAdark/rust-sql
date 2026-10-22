// W8-EQUIVCLASS-SORTOP: equivclass #60's inner `Sort Key: ec1.f1 USING <` (the int8alias1 `<`, 39117, of a btree family
// that is not the type's default) decomposes with the key's `(opcintype, opfamily)` when the family has no order proc for
// it (PG raises at the Sort's first call: lowerv4 serves that raise), and orders by that family's comparator when it has
// one (USEROP, the CompareFn key kind: `SortKey::compare_fn`); the
// graph says it holds one (`sort::holds_no_proc`: exec never files that program in its plan memo, review r1 S1).
use std::fs;

use gravity_decompose::catalog::{IndexExt, KeyOrder, KeyReq, LeafFact};
use gravity_decompose::ops::sort;
use gravity_decompose::ops::modifytable::trig_facts::TrigExt;
use gravity_decompose::{corpus, decompose_in, AttFact, ExprFp, Facts, IndexAm, IndexFact, InsteadOf, Op, RelFact};
use gravity_program::Arena;
use mcx::MemoryContext;

mod harness;

const LT: u32 = 39117;
const NP: (u32, u32) = (39092, 1976);

/// The fixture's facts with `LT` a non-default family's operator (the server's `op_dir` answers only a default family's),
/// with no order proc for its type when `.1`.
struct NoProc<'f>(&'f corpus::FixtureFacts, bool);

impl Facts for NoProc<'_> {
    fn rel(&self, relid: u32) -> Option<RelFact> {
        self.0.rel(relid)
    }
    fn att(&self, relid: u32, attno: i16) -> Option<AttFact> {
        self.0.att(relid, attno)
    }
    fn att_range(&self, relid: u32, attno: i16) -> Option<(i64, i64)> {
        self.0.att_range(relid, attno)
    }
    fn index(&self, indexid: u32) -> Option<IndexFact> {
        self.0.index(indexid)
    }
    fn index_am(&self, indexid: u32) -> IndexAm {
        self.0.index_am(indexid)
    }
    fn indexes_of(&self, relid: u32, f: &mut dyn FnMut(IndexFact)) {
        self.0.indexes_of(relid, f)
    }
    fn index_ext(&self, indexid: u32) -> IndexExt {
        self.0.index_ext(indexid)
    }
    fn att_generated(&self, relid: u32, attno: i16) -> u8 {
        self.0.att_generated(relid, attno)
    }
    fn key(&self, req: KeyReq) -> Option<KeyOrder> {
        let (opcintype, family) = NP;
        if req.op == LT {
            return Some(KeyOrder { desc: false, family, opcintype, default_family: false, cmp: 0, no_proc: self.1 });
        }
        self.0.key(req)
    }
    fn op_image_eq(&self, opno: u32) -> Option<(u32, u32)> {
        self.0.op_image_eq(opno)
    }
    fn instead_of(&self, relid: u32) -> InsteadOf {
        self.0.instead_of(relid)
    }
    fn shred_election(&self, relid: u32, attno: i16, keys: &[&str]) -> Option<(u8, i32)> {
        self.0.shred_election(relid, attno, keys)
    }
    fn trig_ext(&self, relid: u32) -> TrigExt {
        self.0.trig_ext(relid)
    }
    fn partition_leaves(&self, root: u32, f: &mut dyn FnMut(LeafFact)) -> bool {
        self.0.partition_leaves(root, f)
    }
    fn index_expr(&self, indexid: u32, col: u8) -> Option<ExprFp> {
        self.0.index_expr(indexid, col)
    }
}

#[test]
fn a_sort_by_an_operator_with_no_order_proc_carries_pgs_raise() {
    let text = fs::read_to_string(harness::plans_root().join("regress-full/equivclass-012.plan")).unwrap();
    let ctx = MemoryContext::new("fixture");
    let fx = corpus::load(ctx.mcx(), &text).expect("equivclass-012 loads");
    let arena = Arena::new(1 << 20);
    let g = decompose_in(arena.handle(), fx.stmt, &NoProc(&fx.facts, true), fx.mcx).expect("the inner Sort decomposes");
    let d = g.pipelines.iter().flat_map(|p| p.ops.iter()).find_map(|op| match op {
        Op::Sort(d) if d.keys.iter().any(|k| k.opno == LT) => Some(d),
        _ => None,
    });
    let d = d.expect("the Sort by 39117");
    assert_eq!(sort::no_proc(d), Some(NP), "PG's raise names the key's (opcintype, opfamily)");
    assert!(sort::holds_no_proc(&g), "exec never files its program in the plan memo");
    assert!(d.keys.iter().all(|k| !k.own_family), "its type's word order, never its family's compare");
    let mut s = String::new();
    gravity_decompose::ops::shared::sort::dump_keys(&d.keys, &mut s).unwrap();
    assert!(s.contains("!noproc(39092,1976)"), "{s}");
    let other = g.pipelines.iter().flat_map(|p| p.ops.iter()).filter_map(|op| if let Op::Sort(d) = op { Some(d) } else { None });
    assert!(other.filter(|d| !d.keys.iter().any(|k| k.opno == LT)).all(|d| sort::no_proc(d).is_none()), "an int8 Sort has none");

    // USEROP: a family with an order proc orders the key by that comparator (the CompareFn key kind), so the Sort is no
    // longer refused by its operator; the fixture names no family for it (`Facts::op_family`), so the merge join above
    // finds no known order on its inner and refuses by that name (the server's facts name the family)
    let r = decompose_in(arena.handle(), fx.stmt, &NoProc(&fx.facts, false), fx.mcx).err().expect("the merge join refuses");
    assert_ne!(r.reason, "a sort operator outside the direction table");
    assert_eq!(r.reason, "a merge join's inner with no known order");

    // does not apply: a graph with no such Sort (equivclass-003, an index join) is filed as before
    let text = fs::read_to_string(harness::plans_root().join("regress-full/equivclass-003.plan")).unwrap();
    let fx = corpus::load(ctx.mcx(), &text).expect("equivclass-003 loads");
    let g = decompose_in(arena.handle(), fx.stmt, &NoProc(&fx.facts, true), fx.mcx).expect("equivclass-003 decomposes");
    assert!(!sort::holds_no_proc(&g));
}
