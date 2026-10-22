// D3016: a pgrcolumnar scan's near-unique text column that is read only as call arguments (substring(c_phone FROM 1
// FOR 2) in the scan's quals) is a by-reference lane, its codes never built; a column some read of which is a
// comparison, a LIKE or a bare output keeps its codes, and so does any column the facts do not count near-unique or
// whose values are wide.
use std::fs;
use std::path::PathBuf;

use gravity_abi::ctx::lane;
use gravity_decompose::catalog::{AttFact, ExprFp, Facts, IndexExt, IndexFact, InsteadOf, LeafFact, RelFact};
use gravity_decompose::corpus::{self, FixtureFacts};
use gravity_decompose::effects::FuncProps;
use gravity_decompose::graph::Source;
use gravity_decompose::ops::modifytable::trig_facts::TrigExt;
use gravity_decompose::decompose_in;
use gravity_program::{Arena, LaneId, LaneLife, Rep};
use mcx::MemoryContext;

mod harness;

/// The fixture's facts with pgrc's part counts: `rows` resident rows for every relation, `entries` per-code entries
/// for every column (none: the facts have no count, as on heap or before a publish), and every column's average width
/// (0: unknown).
struct Counted<'a> {
    inner: &'a FixtureFacts,
    counts: Option<(u64, u64)>,
    width: u32,
}

impl Facts for Counted<'_> {
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
        self.inner.index(indexid)
    }
    fn index_am(&self, indexid: u32) -> gravity_decompose::catalog::IndexAm {
        self.inner.index_am(indexid)
    }
    fn indexes_of(&self, relid: u32, f: &mut dyn FnMut(IndexFact)) {
        self.inner.indexes_of(relid, f)
    }
    fn index_ext(&self, indexid: u32) -> IndexExt {
        self.inner.index_ext(indexid)
    }
    fn att_generated(&self, relid: u32, attno: i16) -> u8 {
        self.inner.att_generated(relid, attno)
    }
    fn key(&self, req: gravity_decompose::catalog::KeyReq) -> Option<gravity_decompose::catalog::KeyOrder> {
        self.inner.key(req)
    }
    fn op_image_eq(&self, opno: u32) -> Option<(u32, u32)> {
        self.inner.op_image_eq(opno)
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
    fn rel_rows(&self, _relid: u32) -> Option<u64> {
        self.counts.map(|c| c.0)
    }
    fn att_entries(&self, _relid: u32, _attno: i16) -> Option<u64> {
        self.counts.map(|c| c.1)
    }
    fn att_width(&self, _relid: u32, _attno: i16) -> Option<u32> {
        Some(self.width).filter(|&w| w > 0)
    }
}

fn fixture(name: &str) -> PathBuf {
    harness::plans_root().join(format!("{name}.plan"))
}

/// Per range scan of relation `relid`, column `attno`'s lane: (the scan's lane count, kind, rep, life).
fn lanes_of(name: &str, counts: Option<(u64, u64)>, width: u32, relid: u32, attno: i16) -> Vec<(usize, u8, Rep, LaneLife)> {
    let text = fs::read_to_string(fixture(name)).unwrap();
    let ctx = MemoryContext::new("scan_datum_text");
    let fx = corpus::load(ctx.mcx(), &text).unwrap();
    let facts = Counted { inner: &fx.facts, counts, width };
    let arena = Arena::new(1 << 20);
    let g = decompose_in(arena.handle(), fx.stmt, &facts, fx.mcx).unwrap_or_else(|r| panic!("{name}: refused {r:?}"));
    let mut out = Vec::new();
    for p in g.pipelines.iter() {
        if let Source::Range { site, relid: r, .. } = &p.source {
            let n = site.lanes().len();
            out.extend(site.lanes().iter().filter(|l| *r == relid && l.id == LaneId::Attr(attno)).map(|l| (n, l.kind, l.rep, l.life)));
        }
    }
    out.sort_by_key(|l| l.0);
    out
}

const UNIQUE: Option<(u64, u64)> = Some((1_500_000, 1_500_000));

#[test]
fn a_near_unique_column_read_by_calls_alone_is_a_by_reference_lane() {
    // the sub-plan's customer scan (c_phone, c_acctbal) reads c_phone (attno 5) only as substring()'s argument in its
    // quals, and its parent Agg reads no c_phone entry of its physical target list: by reference. The other customer
    // scan (c_custkey, c_phone, c_acctbal) is the hashed side of the anti join: the Hash stores its rows, a parent no
    // rule reads through, so it keeps its codes though the join reads c_phone only through substring()
    let got = lanes_of("tpch-sf10-022", UNIQUE, 16, 16426, 5);
    let codes = (lane::KIND_CODES, Rep::Code { dict: gravity_program::DictRef { lane: 1, fp_table: gravity_program::DictRef::NO_TABLE } }, LaneLife { until: gravity_program::Until::Part, home: gravity_program::Home::Dictionary });
    // by the scan's lane count: the sub-plan's scan first
    assert_eq!(got.iter().map(|l| (l.1, l.2, l.3)).collect::<Vec<_>>(), vec![(lane::KIND_BYREF, Rep::VarlenaPtr, LaneLife::PGRC), codes]);
    assert!(got[0].0 < got[1].0, "{got:?}");
}

#[test]
fn without_a_near_unique_count_the_column_keeps_its_codes() {
    // no count (heap, before a publish, a bpchar with no footer NDV and no ANALYZE), and a count under PS1-29's 95 %
    for counts in [None, Some((1_500_000, 1_400_000)), Some((1_000, 1_000))] {
        let got = lanes_of("tpch-sf10-022", counts, 16, 16426, 5);
        assert!(got.len() == 2 && got.iter().all(|l| l.1 == lane::KIND_CODES), "{counts:?}: {got:?}");
    }
    // a near-unique column of wide values (a granule's images would sit in every reading source's arena), or of no
    // known width, keeps the shared dictionary
    for width in [0, gravity_program::cost::DATUM_TEXT_WIDTH_MAX + 1, 136 << 10] {
        let got = lanes_of("tpch-sf10-022", UNIQUE, width, 16426, 5);
        assert!(got.len() == 2 && got.iter().all(|l| l.1 == lane::KIND_CODES), "width {width}: {got:?}");
    }
}

#[test]
fn a_column_compared_or_projected_keeps_its_codes() {
    // media_url LIKE '%atlas%' (an operator over the column); notes <> '' and the bare group key; media_url read through
    // octet_length() but also by media_url <> ''
    for (name, relid, attno) in [("bench-154", 16498, 6), ("fieldobs-029", 16464, 28), ("fieldobs-043", 16464, 29)] {
        let got = lanes_of(name, UNIQUE, 16, relid, attno);
        assert!(!got.is_empty() && got.iter().all(|l| l.1 == lane::KIND_CODES), "{name}: {got:?}");
    }
}
