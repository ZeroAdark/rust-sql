//! Build the smaller side from exact bind facts (row order may change; the multiset must match). Hooks /
//! levers: H18c R5-L06. Budget 160 lines (files.tsv, +25 % gate). Owner M1-JOIN.
//!
//! What a build carries and who probes it (`Shape`, one per build slot and producer, in `Px::ext_mut(Face::Join)`): the
//! probing pipeline, the build row's columns that pipeline reads after the probe (its INNER_VAR columns: the join qual
//! and the ops up to the projection that collapses the joined row), the forms its probe reads (the roads that are on),
//! and what the build's lowering learns of the record (image words, record bytes, each payload column's kept cell).
use core::cell::{Cell as VCell, RefCell};
use std::rc::Rc;

use gravity_decompose::graph::{PipelineNode, Source};
use gravity_decompose::join::JoinDesc;
use gravity_decompose::Op;
use gravity_program::LifeId;
use mcx::{CollectIn, GrowIn, Mcx, PgBox, PgVec};
use types_nodes::jointype::JoinType;
use types_nodes::primnodes::{INNER_VAR, OUTER_VAR};
use types_nodes::Node;

use crate::cell::Cell;
use crate::face::end::EndMech;
use crate::ops::hashjoin::face::{SideImpl, SideMech, Swap};
use crate::table::face::TablePolicy;
use crate::face::kinds::Face;
use crate::face::step::StepMech;
use crate::frame::px::Px;
use crate::graph::Gx;
use crate::admit::refusal::{Reason, ShapeWhy};

use crate::ops::hashjoin::build::{A_DENSE, A_TAGS};

crate::strategies! {
    /// Build the smaller side from exact bind facts (row order may change, the multiset must match).
    JOIN_BUILDSIDE = 54, "join.buildside", Join;
}

pub(crate) const ROWS: crate::ops::Table = crate::ops::Table { strategies: &[(crate::face::strategy::JOIN_BUILDSIDE, &SideImpl(&SIDE))], ..crate::ops::NONE };

pub(crate) struct JoinExt<'g> {
    pub(crate) shapes: PgVec<'g, Rc<Shape>>,
    /// Per build pipeline, its spilling probes' kept row layout (spill/pair.rs).
    pub(crate) pairs: PgVec<'g, (usize, Rc<crate::ops::hashjoin::spill::pair::Rows<'g>>)>,
    /// D2717: `(q, P')` while spill/pair.rs makes probe pipeline `q`'s ops again under the pair pass `P'`'s identity.
    pub(crate) pairing: Option<(usize, usize)>,
    /// D3512 / D3513: the `Arms` protocols a spill pair grew, by index into `Px::protos`, once per growth (spill/pair.rs
    /// `grows`): each counted one pass `P'` into the run's span, so its last graph pipeline is that many before its end.
    pub(crate) grown: PgVec<'g, usize>,
    /// D4054: the pair runs of hash build ends, `(q, P', slot)`: probe pipeline `q` ends in the build of `slot` and its pair
    /// pass `P'` appends to it too (spill/pair.rs `pair`, recorded before it makes `P'`'s copy of `q`'s end and kept for
    /// `q`'s own end, made later). build.rs `run_of` reads the roles from it.
    pub(crate) pair_runs: PgVec<'g, (usize, usize, u8)>,
}

impl<'g> JoinExt<'g> {
    pub(crate) fn new(m: Mcx<'g>) -> JoinExt<'g> {
        JoinExt { shapes: PgVec::new_in(m), pairs: PgVec::new_in(m), pairing: None, grown: PgVec::new_in(m), pair_runs: PgVec::new_in(m) }
    }
}
crate::frame::px::ext_g!(JoinExt<'g>, |m| JoinExt::new(m));

pub(crate) struct Shape {
    pub slot: u8,
    pub build: usize,
    pub probe: Option<usize>,
    /// The build row's columns the probe's pipeline reads (INNER_VAR attnos), in record order.
    pub payload: Vec<i16>,
    pub allow: u8,
    /// Set by the build's lowering (before its probe's): image words, record bytes, a one-word image, each kept cell.
    pub words: VCell<u8>,
    pub rec: VCell<u32>,
    pub range: VCell<bool>,
    /// A compact build's payload value bits (dense_bits.rs `value_bits`; 1 otherwise).
    pub vbits: VCell<u8>,
    pub kept: RefCell<Vec<Cell>>,
    /// Bit i: payload column i is an image in the region, kept as its offset from the base (a region grows and moves, D254).
    pub rel: VCell<Rel>,
    /// join.mult: the probe hands its rows on with their key's count (multiplicity.rs `candidate` / `decide`).
    pub mult: VCell<bool>,
    /// join.dedup: 0, a candidate (`CAND`, the probe's make), on (`ON`, its end keeps one copy of a row: dedup.rs).
    pub dedup: VCell<u8>,
    /// join.buildside swapped the pair: the records hold the probe side's rows (OUTER_VAR), the probe reads the build side's.
    pub swapped: bool,
    pub hoist: VCell<Option<crate::ops::hashjoin::strategies::hoist::Hoist>>, // A': set by the build (hoist.rs `build`)
    /// D2854: the record form the arms of a build's Arms run write (image words, record bytes, compact, value bits) and
    /// bit i: an arm that may write a value in payload column i set its kept cell (`settle_arm`).
    pub form: VCell<Option<(u8, u32, bool, u8)>>,
    pub valued: VCell<Rel>,
    /// D1511: a build's Arms run's partition bits (or why it has none), decided once by the arm made first and held for
    /// the others (`run_bits`), so every arm opens the slot at one PBITS.
    pub bits: VCell<Option<Result<u8, gravity_program::spill::NoSpill>>>,
    /// join.payload_fp (strategies/payload_fp.rs): the payload columns (INNER_VAR attnos, each with whether its key is bpchar)
    /// a group past the probe keys by their Fp128, and (set by the build's lowering) the payload indices whose records carry
    /// the two words, one bit each (bit `i`: payload column `i`, the first 64 only), in record order (`fp_off`).
    pub fp: crate::ops::hashjoin::strategies::payload_fp::FpCols,
    pub fp_live: core::cell::Cell<u64>,
}

/// The life a kept payload column's record cell renders through (`Cell::of_record`): a code the build kept from a record
/// (another build's, join.payload_fp multi-hop, or a group's emit, D4445; build.rs) keeps that record's, every other column
/// the build's own `own`.
pub fn home_of(c: &Cell, own: LifeId) -> LifeId {
    match (c.rep, c.life.home) {
        (gravity_program::Rep::Code { .. }, crate::cell::CellHome::Record(l)) => l,
        _ => own,
    }
}

/// D2854: why two arms of one build's Arms run cannot share its records.
const ARMS_APART: &str = "a build's arms write their records or keep a payload column in different forms (D2854)";

impl Shape {
    pub fn link_off(&self) -> i32 {
        8 * self.words.get() as i32
    }

    pub fn pay_off(&self, i: usize) -> i32 {
        8 * (self.words.get() as i32 + 1 + i as i32)
    }

    /// join.payload_fp: the byte offset of the `j`th live column's two Fp128 words, after the payload and its wide NULL words.
    pub fn fp_off(&self, j: usize) -> i32 {
        let pay = self.payload.len();
        8 * (self.words.get() as usize + 1 + pay + crate::ops::hashjoin::layout::null_words(pay) + 2 * j) as i32
    }

    /// join.payload_fp: payload column `i`'s place among the live columns (`fp_off`'s `j`), when its record carries the words.
    pub fn fp_at(&self, i: usize) -> Option<usize> {
        let m = self.fp_live.get();
        (i < 64 && m >> i & 1 == 1).then(|| (m & ((1u64 << i) - 1)).count_ones() as usize)
    }

    /// join.payload_fp: how many payload columns' records carry the two words.
    pub fn fp_n(&self) -> usize {
        self.fp_live.get().count_ones() as usize
    }

    /// Payload column `i`'s NULL bit: (byte offset in the record, bit) (layout.rs `null_at`).
    pub fn null_at(&self, i: usize) -> (i32, u32) {
        let (off, bit) = crate::ops::hashjoin::layout::null_at(self.words.get() as usize, self.payload.len(), i);
        (off as i32, bit)
    }

    /// The first record's byte offset in the region (layout.rs `base`).
    pub fn base(&self) -> usize {
        crate::ops::hashjoin::layout::base(self.payload.len())
    }

    /// Payload column `i` is an image in the region, kept as its offset from the region's end.
    pub fn is_rel(&self, i: usize) -> bool {
        self.rel.get()[i / 64] >> (i % 64) & 1 != 0
    }

    /// D2854: the record form `f` an arm writes; every arm of a build's Arms run (B4d) writes the one its readers read.
    pub fn arm_form(&self, node: i32, f: (u8, u32, bool, u8)) {
        if self.form.replace(Some(f)).is_some_and(|g| g != f) {
            crate::refuse!(node, Reason::Shape { why: ShapeWhy(ARMS_APART) });
        }
    }

    /// D1511: the partition bits of the build's Arms run: `decide`'s answer for the arm made first, the held one for every
    /// later arm. Each arm's copy of the end sees another `first` (`arms_of`), never another last pipeline or probe.
    pub fn run_bits(&self, decide: impl FnOnce() -> Result<u8, gravity_program::spill::NoSpill>) -> Result<u8, gravity_program::spill::NoSpill> {
        let bits = self.bits.get().unwrap_or_else(decide);
        self.bits.set(Some(bits));
        bits
    }

    /// D2854: an arm's kept cells and image bits (`rel`); `quiet` bit i: its column i is NULL in every row (a seeded
    /// drain's probe-side columns). The readers and the lay-out read the one form the shape ends with, never past a NULL
    /// bit, so a quiet column leaves the form an arm with values set; values kept in two forms refuse.
    pub fn settle_arm(&self, node: i32, kept: Vec<Cell>, rel: Rel, quiet: Rel) {
        let on = |r: &Rel, i: usize| r[i / 64] >> (i % 64) & 1 != 0;
        let mut have = self.kept.borrow_mut();
        if have.len() != kept.len() {
            have.clone_from(&kept); // alloc: Program
        }
        let (mut valued, mut out) = (self.valued.get(), self.rel.get());
        for (i, c) in kept.into_iter().enumerate() {
            let (q, v) = (on(&quiet, i), on(&valued, i));
            if q && v {
                continue;
            }
            if v && (!one_form(have[i].rep, c.rep) || on(&out, i) != on(&rel, i)) {
                crate::refuse!(node, Reason::Shape { why: ShapeWhy(ARMS_APART) });
            }
            have[i] = c;
            let m = 1u64 << (i % 64);
            out[i / 64] = if on(&rel, i) { out[i / 64] | m } else { out[i / 64] & !m };
            if !q {
                valued[i / 64] |= m;
            }
        }
        (self.valued.set(valued), self.rel.set(out));
    }
}

/// D2854: a record keeps a word cell's datum word whatever its width (rep::datum_word: off a lane the word as is), so two
/// arms' words of one column are one form there.
fn one_form(a: gravity_program::Rep, b: gravity_program::Rep) -> bool {
    a == b || matches!((a, b), (gravity_program::Rep::Word { .. }, gravity_program::Rep::Word { .. }))
}

/// One bit per payload column (`layout::MAX_PAY` fits).
pub(crate) type Rel = [u64; crate::ops::hashjoin::layout::REL_WORDS];

/// The build `slot` of producer `build`, computed once per program.
pub(crate) fn shape(px: &mut Px<'_, '_>, slot: u8, build: usize) -> Rc<Shape> {
    let fp = crate::ops::hashjoin::strategies::payload_fp::cols(px.gx, build);
    cached(px, slot, build, false, |px| Shape { fp, ..compute(px, slot, build) })
}

fn cached(px: &mut Px<'_, '_>, slot: u8, build: usize, swapped: bool, f: impl FnOnce(&Px<'_, '_>) -> Shape) -> Rc<Shape> {
    if let Some(s) = px.ext_mut::<JoinExt>(Face::Join).shapes.iter().find(|s| s.slot == slot && s.build == build && s.swapped == swapped) {
        return s.clone();
    }
    let s = Rc::new(f(px)); // alloc: Program
    px.ext_mut::<JoinExt>(Face::Join).shapes.push_in(s.clone());
    s
}

fn new_shape(slot: u8, build: usize, probe: Option<usize>, payload: Vec<i16>, swapped: bool) -> Shape {
    // a payload past the link word's NULL bits takes the plain form only (layout.rs)
    let narrow = payload.len() <= crate::ops::hashjoin::layout::LINK_NULLS;
    let on = |f: bool, a: u8| if f && narrow { a } else { 0 };
    let allow = on(TablePolicy::DenseObserved.on(), A_DENSE) | on(TablePolicy::BloomTag.on(), A_TAGS);
    let (words, rec, range, kept, rel, mult) = (VCell::new(1), VCell::new(0), VCell::new(false), RefCell::new(Vec::new()), VCell::new([0; crate::ops::hashjoin::layout::REL_WORDS]), VCell::new(false));
    Shape { slot, build, probe, payload, allow, words, rec, range, vbits: VCell::new(1), kept, rel, mult, dedup: VCell::new(0), swapped, hoist: Default::default(), form: VCell::new(None), valued: VCell::new([0; crate::ops::hashjoin::layout::REL_WORDS]), bits: VCell::new(None), fp: crate::ops::hashjoin::strategies::payload_fp::FpCols::NONE, fp_live: VCell::new(0) }
}

fn compute(px: &Px<'_, '_>, slot: u8, build: usize) -> Shape {
    let gx = px.gx;
    let bp = &gx.pipelines()[build];
    let found = gx.pipelines().iter().enumerate().skip(build + 1).find_map(|(q, p)| {
        let j = p.ops.iter().position(|op| matches!(op, Op::Join(JoinDesc::Probe { slot: s, .. }) if *s == slot))?;
        (gx.life_of(q, slot) == Some(LifeId(build as u16))).then(|| (q, j, read_after(gx, p, j, INNER_VAR)))
    });
    let probe = found.as_ref().map(|f| f.0);
    // D3194: the probe tests the build's Bloom words (rowfilter.rs), so its HASH lay-out lays them
    let rf = found.as_ref().is_some_and(|f| crate::ops::hashjoin::strategies::rowfilter::takes(gx, f.0, f.1));
    // a catalog probe reads a key's matches in record order (nest.eqkey: the Nest's order)
    let fifo = found.as_ref().is_some_and(|f| matches!(gx.pipelines()[f.0].ops.get(f.1), Some(Op::Join(JoinDesc::Probe { catalog: true, .. }))));
    let inner = || gx.pipelines().inners().into_iter().find_map(|(_, p)| p.ops.iter().position(|op| matches!(op, Op::Join(JoinDesc::Probe { slot: s, .. }) if *s == slot)).map(|j| read_after(gx, p, j, INNER_VAR))); // D1018 row B0: a build only a nest inner probes (nest/inner_probe.rs): its INNER_VAR columns, `probe` none (no skip, spill or rtfilter)
    let pay = match found.map_or_else(|| inner().unwrap_or(Some(Vec::new())), |f| f.2) {
        Some(v) => v,
        None => match bp.ops.iter().rev().find(|op| matches!(op, Op::Project(_))) {
            Some(Op::Project(l)) => (1..=l.len() as i16).collect(), // alloc: Program
            _ => crate::refuse!(bp.node_id, Reason::Shape { why: ShapeWhy("a join payload the build row cannot bound") }),
        },
    };
    if pay.len() > crate::ops::hashjoin::layout::MAX_PAY {
        crate::refuse!(bp.node_id, Reason::Shape { why: ShapeWhy("a join payload wider than PostgreSQL's 1664 target columns") });
    }
    let mut s = new_shape(slot, build, probe, pay, false);
    if fifo {
        s.allow |= crate::ops::hashjoin::build::A_FIFO;
    }
    if rf {
        s.allow |= crate::ops::hashjoin::build::A_RF;
    }
    s
}

/// The `varno` columns `n` reads, into `out`; false when the walker cannot see all of `n` (a whole-row reference too).
fn uses(n: Node<'_>, varno: i32, out: &mut Vec<i16>) -> bool {
    crate::stack_check();
    if let Some(v) = n.as_var() {
        if v.varno == varno && v.varattno > 0 && !out.contains(&v.varattno) {
            out.push(v.varattno); // alloc: Program
        }
        return v.varno != varno || v.varattno > 0;
    }
    let mut ok = true;
    let known = gravity_decompose::expr::kids(n, &mut |k| ok &= uses(k, varno, out));
    known && ok
}

/// The `varno` columns probe op `j` of `p` reads in its join qual (with its hash clauses when it runs them per candidate:
/// keys.rs `confirms`, D1899) and the ops up to the projection that collapses the joined row; `None` when the walker cannot
/// bound them (no such projection, or an op or node it cannot see through).
pub(crate) fn read_after(gx: &Gx<'_, '_>, p: &PipelineNode<'_>, j: usize, varno: i32) -> Option<Vec<i16>> {
    let Op::Join(JoinDesc::Probe { qual, clauses, .. }) = &p.ops[j] else { return None };
    let mut pay = Vec::new(); // alloc: Program
    let confirm = crate::ops::hashjoin::keys::confirms(gx, clauses);
    let mut ok = clauses.iter().filter(|_| confirm).chain(qual.iter()).fold(true, |ok, n| uses(n, varno, &mut pay) && ok);
    for op in p.ops[j + 1..].iter() {
        if crate::frame::iface::limit_bounds(op).is_some() {
            continue;
        }
        match op {
            Op::Filter(n) | Op::Gate(n) => ok &= uses(*n, varno, &mut pay),
            // a SubPlan evaluated on the joined row reads it only through its arguments (the parameters its plan reads)
            // and its test (an ANY / ALL / ROWCOMPARE's left operands, a hashed SubPlan's probe keys); its result is a
            // cell the ops after it read (psql's \dp: a correlated ARRAY SubPlan over the probe side after the probe)
            Op::SubPlan(d) => ok &= d.test.is_none_or(|t| uses(t, varno, &mut pay)) && d.args.iter().fold(true, |ok, a| uses(a, varno, &mut pay) && ok),
            Op::Project(l) => return (l.iter().fold(ok, |ok, n| uses(n, varno, &mut pay) && ok)).then_some(pay),
            _ => ok = false,
        }
    }
    None
}

/// join.buildside: the swapped INNER pair's shape: the records are the probe side's rows, their payload the OUTER_VAR
/// columns the probe's qual and the ops up to the collapsing projection read.
pub(crate) fn shape_swapped(px: &mut Px<'_, '_>, s: &Swap) -> Rc<Shape> {
    let p = &px.gx.pipelines()[s.probe];
    let Op::Join(JoinDesc::Probe { slot, .. }) = &p.ops[s.at] else { crate::refuse!(p.node_id, Reason::Bug { payload: "a swap without its probe" }) };
    let (slot, pay) = (*slot, read_after(px.gx, p, s.at, OUTER_VAR).unwrap_or_default());
    cached(px, slot, s.build, true, |_| new_shape(slot, s.build, Some(s.probe), pay, true))
}

/// join.buildside: an INNER pair swaps when both sides are range scans that consume no slot, the build side's rows are
/// exact (`rel_rows` with only projections before the build), the probe side's exact or, on a heap table, bounded from
/// above by its pages (`rel_rows_bound`: pages x MaxHeapTuplesPerPage, no statistic), the build side's records outweigh
/// the probe side's (the cost home's `build_side_swaps`: rows and record words), the probe side's columns the probe reads
/// after it are all seen (its payload is bounded), and nothing either side evaluates can raise (D237: PG's order of
/// evaluation is the unswapped one). A stale choice is only slower; a heap side's grows with no invalidation (an INSERT
/// extends it), so its decision is a relation guard of the program (lower key.rs `rel_decide`), decided again on the
/// heap's pages whenever the program is to run: past the bound, the statement runs the unswapped program.
struct Side;

static SIDE: Side = Side;

impl SideMech for Side {
    fn swaps<'g>(&self, px: &Px<'g, '_>) -> PgVec<'g, Swap> {
        let gx = px.gx;
        let ops = gx.pipelines().iter().enumerate().flat_map(|(q, p)| p.ops.iter().enumerate().map(move |(j, op)| (q, j, op)));
        ops.filter_map(|(q, j, op)| match op {
            Op::Join(JoinDesc::Probe { slot, jointype: JoinType::JOIN_INNER, catalog: false, .. }) => gx.life_of(q, *slot).map(|b| Swap { build: b.0 as usize, probe: q, at: j }),
            _ => None,
        })
        .filter(|s| swaps(gx, s.build, s.probe, s.at))
        .inspect(|_| fires::fire("join.buildside")) // shapecheck fire signal: an INNER pair built on its probe side
        .collect_in(px.mcx())
    }

    fn make_build<'g>(&self, px: &mut Px<'g, '_>, s: &Swap) -> PgBox<'g, dyn EndMech + 'g> {
        crate::ops::hashjoin::build::make_swapped(px, s)
    }

    fn make_probe<'g>(&self, px: &mut Px<'g, '_>, s: &Swap) -> PgBox<'g, dyn StepMech + 'g> {
        crate::ops::hashjoin::probe::make_swapped(px, s)
    }
}

pub(crate) fn swaps(gx: &Gx<'_, '_>, b: usize, q: usize, j: usize) -> bool {
    let (bp, pp) = (&gx.pipelines()[b], &gx.pipelines()[q]);
    let (Source::Range { relid: rb, .. }, Source::Range { relid: rp, .. }) = (&bp.source, &pp.source) else { return false };
    let (Some(Op::Join(JoinDesc::Build { keys: bk, .. })), Op::Join(JoinDesc::Probe { keys: pk, qual, clauses, .. })) = (bp.ops.last(), &pp.ops[j]) else { return false };
    let pre = &bp.ops[..bp.ops.len() - 1];
    // the side the swap would build (the probe's) may be a heap table: its rows are bounded from above by its pages
    // (PS1-21 / PS1-26 step 1b: no statistic, so none can be stale); the side the swap would probe with keeps exact rows
    let Some(nb) = gx.facts.rel_rows(*rb) else { return false };
    let (np, heap) = match gx.facts.rel_rows(*rp) {
        Some(n) => (n, false),
        None => match gx.facts.rel_rows_bound(*rp) {
            Some(n) => (n, true),
            None => return false,
        },
    };
    let raise_free = |n: Node<'_>| crate::effects::raise_free(gx, n);
    let Some(pay_p) = read_after(gx, pp, j, OUTER_VAR) else { return false };
    // the build record's payload as `compute` bounds it: the columns the probe reads, else the build's projection
    let pay_b = read_after(gx, pp, j, INNER_VAR).map_or_else(|| pre.iter().rev().find_map(|op| if let Op::Project(l) = op { Some(l.len()) } else { None }).unwrap_or(0), |v| v.len());
    let (wb, wp) = ((bk.len() + 1 + pay_b) as u64, (bk.len() + 1 + pay_p.len()) as u64);
    bp.desc.consumes.is_empty()
        && pp.desc.consumes.len() == 1
        && pp.ops.iter().filter(|op| matches!(op, Op::Join(_))).count() == 1
        && pre.iter().all(|op| matches!(op, Op::Project(_)))
        && !crate::ops::hashjoin::keys::confirms(gx, clauses)
        && pay_p.len() <= crate::ops::hashjoin::layout::MAX_PAY
        && crate::effects::ops_raise_free(gx, pre)
        && crate::effects::ops_raise_free(gx, &pp.ops[..j])
        && crate::effects::ops_raise_free(gx, &pp.ops[j + 1..])
        && bk.iter().chain(pk.iter()).chain(qual.iter()).all(raise_free)
        // the sizes last: a heap side's decision is filed as a relation guard only for a pair that swaps but for its size
        && match heap {
            false => gravity_program::cost::build_side_swaps(nb, np, wb, wp),
            true => gravity_lower::inputs::key::rel_decide(*rp, np, heap_swaps, [nb, wb, wp, 0]) != 0,
        }
}

/// A heap side's relation guard (lower key.rs `rel_decide`): `build_side_swaps` on its page bound now (`np`), against the
/// build side's exact rows and the two record widths it was lowered with (`a`: rows, build words, probe words).
fn heap_swaps(np: u64, a: &[u64; 4]) -> u64 {
    gravity_program::cost::build_side_swaps(a[0], np, a[1], a[2]) as u64
}

#[cfg(test)]
mod tests {
    use gravity_program::{NumForm, NumTypmod, Rep, Until};

    use crate::admit::refusal::Refusal;
    use crate::cell::{At, Cell, CellHome, CellLife, Nullable};

    /// A payload bit set whose first word is `x`.
    fn w(x: u64) -> super::Rel {
        let mut r = [0; crate::ops::hashjoin::layout::REL_WORDS];
        r[0] = x;
        r
    }

    fn cell(rep: Rep, typid: u32) -> Cell {
        Cell { rep, typid, num: crate::rep::num_of(rep), life: CellLife { until: Until::Life, home: CellHome::RowCx }, at: At::Value(0), null: Nullable::Maybe }
    }

    fn refuses(f: impl FnOnce()) -> bool {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        r.err().and_then(|e| e.downcast::<Refusal>().ok()).is_some_and(|r| r.reason.to_string().contains("(D2854)"))
    }

    /// D2854: the arms of one build's Arms run settle one form per payload column. A column one arm writes NULL in every
    /// row (a seeded drain's probe-side column) keeps the form of the arm with values, whichever arm comes first; words of
    /// two widths are one form; two arms with values in different forms (rep or image bit) or two record forms refuse.
    #[test]
    fn arms_of_a_run_settle_one_form() {
        let text = cell(Rep::VarlenaPtr, 25);
        let num = cell(Rep::PackedNum { facts: NumTypmod::of_typmod((12 << 16 | 2) + 4), form: NumForm::Word }, 1700);
        let drained = cell(Rep::VarlenaPtr, 1700);
        let fresh = || super::new_shape(2, 2, None, vec![1, 2], false);
        for drain_first in [false, true] {
            let s = fresh();
            let (probe, drain) = ((vec![text, num], w(0b01), w(0)), (vec![text, drained], w(0b11), w(0b10)));
            let arms = if drain_first { [drain, probe] } else { [probe, drain] };
            for (kept, rel, quiet) in arms {
                s.arm_form(4, (1, 40, false, 1));
                s.settle_arm(4, kept, rel, quiet);
            }
            assert_eq!(s.kept.borrow()[1].rep, num.rep, "drain first: {drain_first}");
            assert_eq!(s.rel.get()[0], 0b01, "drain first: {drain_first}");
        }
        let s = fresh();
        s.settle_arm(4, vec![text, cell(Rep::Word { bytes: 4 }, 23)], w(0b01), w(0));
        s.settle_arm(4, vec![text, cell(Rep::Word { bytes: 8 }, 23)], w(0b01), w(0));
        let s = fresh();
        s.settle_arm(4, vec![text, num], w(0b01), w(0));
        assert!(refuses(|| s.settle_arm(4, vec![text, drained], w(0b11), w(0))), "two reps with values");
        let s = fresh();
        s.settle_arm(4, vec![text, num], w(0b01), w(0));
        assert!(refuses(|| s.settle_arm(4, vec![text, num], w(0), w(0))), "an image copied in one arm, by pointer in another");
        let s = fresh();
        s.arm_form(4, (1, 40, false, 1));
        assert!(refuses(|| s.arm_form(4, (1, 8, true, 1))), "two record forms");
    }
}
