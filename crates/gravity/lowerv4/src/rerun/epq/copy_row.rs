//! A ROW_MARK_COPY mark's stored row read back on an EvalPlanQual recheck (D732, V4R-268; D732-rest, V4R-290). Owner
//! M4-LOCK. PostgreSQL carries a non-target VALUES / subquery / CTE relation's row whole (the junk `wholerow{rowmarkId}`,
//! a blessed record) and EvalPlanQualFetchRowMark's copy leg hands the recheck that stored row, never the relation
//! re-evaluated. `marks::store` writes the record datum into the scan's copy site (`SiteKind::Copy`); the driver flags it
//! `POS_COPY` on the recheck node only (`Node::set_copies`).
//! - `copying` / `fields`: the flag, and the record's fields read in place (`copy_field`); VALUES (`source/values.rs`) and
//!   the subquery entry below both read them.
//! - `CopyEntry` (D732-rest): a SubqueryScan whose row a COPY mark carries. The pipeline running it owns a late copy site;
//!   under `POS_COPY` the call is that one row, the record's fields as the subplan's output columns (the scan tuple
//!   EvalPlanQual hands the SubqueryScan), entering the chain at the SubqueryScan's first op, so its quals and projection
//!   run again as ExecScan's EvalPlanQual path runs them (frame.rs `copy_entry`). The source and the ops below it (the
//!   subquery's own) never run, and `marks.rs` marks the producers only it reads `recheck_skip` (the driver's rerun opens
//!   their slots empty): a volatile subquery is evaluated once, as in PostgreSQL.
mod raw;

use std::cell::Cell as SiteCell;
use std::rc::Rc;

use gravity_abi::ctx::{site_off, POS_COPY};
use gravity_abi::site::site;
use gravity_decompose::join::JoinDesc;
use gravity_decompose::ops::shared::append::AppendDesc;
use gravity_decompose::ops::shared::setop::SetDesc;
use gravity_decompose::ops::shared::srf::SrfPlace;
use gravity_decompose::ops::shared::record_row::RecordRow;
use gravity_decompose::{Op, Source};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{HostFlags, IntBinOp, IntCond, Type, Value};
use gravity_program::{ExprLife, ExprOut, OutCol, SiteDesc};

use gravity_ir::builder::ProgramBuilder;
use gravity_ir::Block;
use gravity_program::{ArenaRef, Pipeline};
use mcx::{CollectIn, GrowIn, Mcx, PgBox, PgVec};

use crate::cell::{Cell, Nullable, Row};
use crate::face::source::{DecodeOrder, GranuleIdent, Locator, Packed, ReadPlan, SourceMech, VerdictKind};
use crate::frame::env::Env;
use crate::frame::frame::{Win, WordWin};
use crate::frame::open::OpenWords;
use crate::frame::px::Px;
use crate::frame::row::{Carry, Mask, RowVals};
use crate::admit::refusal::{Reason, ShapeWhy};

/// Whether copy site `c` holds a recheck's stored row (`POS_COPY`, set on the recheck node only: driver `set_copies`).
pub fn copying(b: &mut FunctionBuilder<'_>, env: &Env, c: usize) -> Value {
    let flags = b.load_zext(Type::I16, env.ctx, site_off(c) as i32 + site::POS_FLAGS as i32);
    let bit = b.iconst(Type::I64, i64::from(POS_COPY));
    let on = b.int_bin(IntBinOp::And, flags, bit);
    b.icmp(IntCond::Ne, on, env.zero)
}

/// D1685: whether copy site `c` holds the empty copy (datum 0: `marks::store`'s NULL wholerow, a null-extended row),
/// which a recheck reads as no row (EvalPlanQualFetchRowMark's copy leg on a NULL datum).
pub fn none(b: &mut FunctionBuilder<'_>, env: &Env, c: usize) -> Value {
    let img = b.load_unaligned(Type::I64, env.ctx, site_off(c) as i32 + site::POS_REL as i32);
    b.icmp(IntCond::Eq, img, env.zero)
}

/// PostgreSQL's VARATT_SHORT_MAX (varatt.h; `types_tuple::varatt::VARATT_SHORT_MAX`, kept local because lowerv4 does not
/// depend on types_tuple): the largest total size of a short-header (1-byte header) varlena.
const VARATT_SHORT_MAX: usize = 0x7f;

/// The plain rebuild of a short-header record image: at most VARATT_SHORT_MAX bytes with its 1-byte header, so at most
/// VARATT_SHORT_MAX + 3 (130) with the 4-byte one, rounded up to a power of two for the stack slot. Derived, not tuned:
/// every short-header image fits, and no larger image is short-header (a plain one is read in place).
const COPY_BUF: usize = (VARATT_SHORT_MAX + 3).next_power_of_two();

/// The stored record's fields `1..=typids.len()` of copy site `c`, each `(datum word, valid)`; an error raises.
pub fn fields(b: &mut FunctionBuilder<'_>, env: &mut Env, c: usize, typids: &[u32]) -> Vec<(Value, Value)> {
    let img = b.load_unaligned(Type::I64, env.ctx, site_off(c) as i32 + site::POS_REL as i32);
    let cols: PgVec<'_, (i16, u32)> = typids.iter().enumerate().map(|(col, &t)| (col as i16 + 1, t)).collect_in(env.mcx);
    fields_of(b, env, img, &cols[..])
}

/// The record datum `img`'s fields `(fieldnum, type)`, each `(datum word, valid)`, read as `fields` reads a copy site's
/// (V4R-353: a MERGE into a view reads its old row's unchanged columns from the wholerow image, write/merge_view.rs); a field of type 0 (D3374: a dropped attribute) is NULL.
pub fn fields_of(b: &mut FunctionBuilder<'_>, env: &mut Env, img: Value, cols: &[(i16, u32)]) -> Vec<(Value, Value)> {
    let flags = HostFlags { may_error: true, touches_memory: true, ..HostFlags::default() };
    let h = b.declare_host_once("values::copy_field", &[Type::I64, Type::I64, Type::Ptr, Type::Ptr], Some(Type::I64), raw::copy_field as *const () as usize, flags);
    let slot = b.declare_stack_slot(8, 8);
    let plain = b.declare_stack_slot(COPY_BUF as u32, 8);
    let mut out = Vec::with_capacity(cols.len()); // alloc: Program
    for &(fieldnum, typid) in cols.iter() {
        // D3374: a dropped attribute (type 0) holds no value: NULL, no field read
        if typid == 0 {
            let no = b.bconst(false);
            out.push((env.zero, no)); // alloc: Program
            continue;
        }
        let word = b.iconst(Type::I64, i64::from(fieldnum) | (i64::from(typid) << 16));
        let (at, pb) = (b.stack_addr(slot), b.stack_addr(plain));
        let r = b.call_host(h, &[img, word, at, pb], Some(Type::I64)).expect("returns its status");
        let failed = b.icmp(IntCond::Eq, r, env.zero);
        let l_ok = b.create_block(&[]);
        let e = b.iconst(Type::I64, gravity_abi::status::raise::bit(gravity_abi::status::raise::HOST) as i64);
        b.cond_br(failed, (env.l_raise, &[e]), (l_ok, &[]));
        b.switch_to(l_ok);
        let v = b.load(Type::I64, at, 0);
        let one = b.iconst(Type::I64, 1);
        out.push((v, b.icmp(IntCond::Eq, r, one))); // alloc: Program
    }
    out
}

/// D732-rest: a SubqueryScan's copy entry in the pipeline that runs it: its copy site, the op the SubqueryScan starts at
/// (its `NodeOps::first_op`) and the record's fields as that op's input columns (`resno`, the plain datum's type).
pub struct CopyEntry<'g> {
    pub site: Rc<SiteCell<usize>>,
    pub boundary: usize,
    pub cols: PgVec<'g, (i16, ExprOut)>,
    /// D1685: a union's row (`RecordRow::whole`): the input column (resno) the stored record itself fills, 0 for none.
    pub whole: (i16, Option<ExprOut>),
}

/// The `record` pseudo-type (a COPY mark's whole row).
const RECORDOID: u32 = 2249;

/// D3374: the placeholder representation of a dropped attribute's NULL cell (never read).
const INT4OID: u32 = 23;

const DEEP: ShapeWhy = ShapeWhy("a COPY-marked subquery or function row entering past the 64th op of its pipeline (D732-rest, D890)");

/// Graph pipeline `k`'s copy entry: a SubqueryScan it runs, or (D890) a FROM function it calls, whose whole row a write's
/// or lock's COPY mark carries (decompose `RecordRow`; where it enters: `boundary`); the copy site added late and
/// registered in `marks::CopySites` by the row's range table index. A VALUES scan's is `source/values.rs`'s own.
pub fn entry<'g>(px: &mut Px<'g, '_>, k: usize) -> Option<CopyEntry<'g>> {
    let gx = px.gx;
    let p = &gx.pipelines()[k];
    let values = |rti: u32| matches!(p.source, Source::Values { scanrelid, .. } if u32::from(scanrelid) == rti);
    let graph: &gravity_decompose::graph::PipelineGraph<'_> = gx.graph;
    let (r, at) = graph.record_rows.iter().filter(|r| !values(r.scanrelid) && crate::rerun::epq::marks::copied(gx, r.scanrelid)).find_map(|r| boundary(gx, k, r).map(|at| (r, at)))?;
    if at >= 64 {
        crate::refuse!(p.node_id, Reason::Shape { why: DEEP });
    }
    let cols = cols_of(gx, r, p.node_id);
    let whole = match r.whole {
        0 => (0, None),
        w => {
            let Ok(tf) = gx.catalog.type_facts(RECORDOID) else { crate::refuse!(p.node_id, Reason::Bug { payload: "the record type the catalog refused" }) };
            (w, Some(ExprOut { rep: OutCol::new("", RECORDOID, -1, tf.len, tf.byval).rep, typid: RECORDOID, life: ExprLife::Row }))
        }
    };
    // W5-C: made once (marks.rs copy_pos makes the rechecking pipeline's own entry before its source)
    let made = px.ext::<crate::rerun::epq::marks::CopySites>(crate::face::kinds::Face::End).and_then(|c| c.0.iter().find(|e| e.0 == r.scanrelid)).map(|e| e.1.clone());
    let site = made.unwrap_or_else(|| {
        let site = px.add_late_site(SiteDesc::copy(px.arena, r.scanrelid));
        px.ext_mut::<crate::rerun::epq::marks::CopySites>(crate::face::kinds::Face::End).0.push_in((r.scanrelid, site.clone()));
        site
    });
    Some(CopyEntry { site, boundary: at, cols, whole })
}

/// The stored record `r`'s fields as the input columns of the scan it enters (`CopyEntry::cols`: each one's resno and
/// plain datum form); `node` refuses a type the catalog does not know. D3569 REFCOPY reads them too (epq_ir.rs
/// `Fetch::copy`).
pub fn cols_of<'g>(gx: &crate::graph::Gx<'g, '_>, r: &RecordRow<'_>, node: i32) -> PgVec<'g, (i16, ExprOut)> {
    let mut cols = mcx::reserved_in(r.cols.len(), gx.mcx);
    for c in r.cols.iter() {
        // D3374: a function row's dropped attribute (typed 0, decompose `function_record_row`) holds no value: its field is
        // read as NULL (`fields_of`) into a cell no reader takes (srf/function_store.rs skips a column typed 0)
        if c.expr.is_none() && c.ty.0 == 0 {
            cols.push_in((c.resno, ExprOut { rep: OutCol::new("", INT4OID, -1, 4, true).rep, typid: 0, life: ExprLife::Row }));
            continue;
        }
        let typ = c.expr.and_then(gravity_decompose::catalog::expr_type::type_of).unwrap_or(c.ty.0);
        let Ok(tf) = gx.catalog.type_facts(typ) else { crate::refuse!(node, Reason::Bug { payload: "a COPY-marked subquery column type the catalog refused" }) };
        cols.push_in((c.resno, ExprOut { rep: OutCol::new("", typ, -1, tf.len, tf.byval).rep, typid: typ, life: ExprLife::Row }));
    }
    cols
}

/// The stored record's row as the SubqueryScan's input: field `i` is column `cols[i].0` (a resno), a plain datum cell.
pub fn row<'e>(b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, e: &CopyEntry, accs: PgVec<'e, Value>) -> RowVals<'e> {
    let img = b.load_unaligned(Type::I64, env.ctx, site_off(e.site.get()) as i32 + site::POS_REL as i32);
    let carry = Carry { out: env.zero, accs };
    let mcx = env.mcx;
    image_row(b, env, mcx, img, (&e.cols, e.whole), (carry, env.one, env.zero))
}

/// `row` over record datum `img` (a COPY mark's stored row: a copy site's, or D3569 REFCOPY's junk whole-row column,
/// epq_ir.rs `Fetch::copy`), its fields `cols` (a union's whole-row column `whole`), with `carry`, `next` and `at`.
pub fn image_row<'e, 'x>(b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, mcx: Mcx<'x>, img: Value, (fcols, whole): (&[(i16, ExprOut)], (i16, Option<ExprOut>)), (carry, next, at): (Carry<'x>, Value, Value)) -> RowVals<'x> {
    let typed: PgVec<'_, (i16, u32)> = fcols.iter().enumerate().map(|(i, c)| (i as i16 + 1, c.1.typid)).collect_in(env.mcx);
    let got = fields_of(b, env, img, &typed[..]);
    let t = b.bconst(true);
    let width = fcols.iter().map(|c| c.0.max(0) as usize).chain([whole.0.max(0) as usize]).max().unwrap_or(0);
    let (mut cells, mut vals, mut cols) = (mcx::reserved_in(got.len(), mcx), mcx::reserved_in(got.len(), mcx), mcx::pg_vec![in mcx; None; width]);
    for (i, ((resno, out), (v, ok))) in fcols.iter().zip(got).enumerate() {
        let null = b.int_bin(IntBinOp::Xor, ok, t);
        cells.push_in(Cell::of_expr(*out, i as u16, i as u16, Nullable::Maybe));
        vals.push_in((v, Some(null)));
        if *resno > 0 {
            cols[*resno as usize - 1] = Some(i as u16);
        }
    }
    // D1685: a union's whole-row column is the stored record itself (the datum the arm's ROW(..) made, never null here)
    if let (w, Some(out)) = whole {
        let i = cells.len();
        cells.push_in(Cell::of_expr(out, i as u16, i as u16, Nullable::Maybe));
        let no = b.bconst(false);
        vals.push_in((img, Some(no)));
        cols[w as usize - 1] = Some(i as u16);
    }
    RowVals { row: Row { cells }, vals, cols, carry, next, at }
}

/// The products graph pipeline `q` reads below op `boundary`: its source's breaker and a hash / merge join's build there.
/// writesubplan (gen setop_core-0157 / 0158): or a set operation's right group (setop.setop's step reads it whole; its
/// left group is the source's breaker). D3557: or a nest's inner there, every product it reads (a Materialize's spool:
/// the inner runs inside the nest, below the entry, so its producers are never rerun either).
fn pre_slots<'g>(gx: &crate::graph::Gx<'g, '_>, q: usize, boundary: usize) -> PgVec<'g, u8> {
    fn reads(p: &gravity_decompose::graph::PipelineNode<'_>, boundary: usize, out: &mut PgVec<'_, u8>) {
        crate::stack_check(); // D3832
        if let Source::Breaker { slot } = p.source {
            out.push_in(slot);
        }
        for o in p.ops.iter().take(boundary) {
            match o {
                Op::Join(JoinDesc::Probe { slot, .. } | JoinDesc::Merge { slot, .. }) | Op::SetOp(SetDesc::Set { right: slot, .. }) => out.push_in(*slot),
                Op::Join(JoinDesc::Nest { inner, .. }) => reads(inner, inner.ops.len(), out),
                _ => {}
            }
        }
    }
    let mut out = mcx::PgVec::new_in(gx.mcx);
    reads(&gx.pipelines()[q], boundary, &mut out);
    out
}

/// The producers graph pipeline `q` reads below its copy entry's op `boundary`, and theirs, transitively: the pipelines a
/// recheck through that entry never reruns (`Pipeline::recheck_skip`). A consumed slot's producers are its nearest earlier
/// producer and every earlier one before another reader of the slot (lockrows WITHCOPY, with#285: a streamed union's arms
/// all fill the one breaker they feed, an aggregate's or a sort's, so each arm is below the entry, not only the last).
pub fn upstream<'g>(gx: &crate::graph::Gx<'g, '_>, q: usize, boundary: usize) -> PgVec<'g, usize> {
    let ps = gx.pipelines();
    let producers = |c: usize, slot: u8| {
        let mut v = mcx::PgVec::new_in(gx.mcx);
        for p in (0..c).rev() {
            if ps[p].desc.produces() == Some(slot) {
                v.push_in(p);
            } else if !v.is_empty() && ps[p].desc.consumes.contains(&slot) {
                break;
            }
        }
        v
    };
    let mut out: PgVec<'g, usize> = PgVec::new_in(gx.mcx);
    for s in pre_slots(gx, q, boundary) {
        for p in producers(q, s) {
            if !out.contains(&p) {
                out.push_in(p);
            }
        }
    }
    let mut todo = mcx::to_vec_in(&out[..], gx.mcx);
    while let Some(c) = todo.pop() {
        for &slot in ps[c].desc.consumes.iter() {
            for p in producers(c, slot) {
                if !out.contains(&p) {
                    out.push_in(p);
                    todo.push_in(p);
                }
            }
        }
    }
    out
}

/// The program's copy entries: each (pipeline, boundary) whose source is one (`skipped`'s list, found once per program).
pub fn copy_entries(mechs: &[crate::frame::frame::Mechs<'_>]) -> Vec<(usize, usize)> {
    mechs.iter().filter_map(|m| m.source.copy_entry().map(|e| (m.k, e.boundary))).collect() // alloc: Program
}

/// Whether graph pipeline `q` is a producer only a copy entry's part below its boundary reads: a recheck never reruns it
/// (`Pipeline::recheck_skip`; the driver opens its slot empty). `entries`: `copy_entries` (D3585: found once, not per
/// pipeline: each pipeline asked every mechanism, quadratic in an Append's arms).
pub fn skipped(gx: &crate::graph::Gx<'_, '_>, entries: &[(usize, usize)], q: usize) -> bool {
    entries.iter().any(|&(k, at)| upstream(gx, k, at).contains(&q))
}

/// D3592: where COPY row `r` enters an embedded nest inner (rerun/inner.rs `copy_walk`): (the inner's `Pipes` index,
/// its boundary), else none.
pub fn inner_entry(gx: &crate::graph::Gx<'_, '_>, r: &RecordRow<'_>) -> Option<(usize, usize)> {
    let n = gx.pipelines().len();
    (n..n + gx.pipelines().inners().len()).find_map(|x| boundary(gx, x, r).map(|at| (x, at)))
}

/// D3592: whether graph pipeline `q` is a producer only the part below a COPY-marked row's entry into an embedded nest
/// inner reads (`inner_entry`): a recheck never reruns it (`Pipeline::recheck_skip`), as `skipped`'s for a pipeline's
/// own entry: the re-open enters with the stored row (rerun/inner.rs `copy_walk`), as PostgreSQL's EvalPlanQual hands
/// the Subquery Scan its stored row and never runs the subquery below it.
pub fn inner_skipped(gx: &crate::graph::Gx<'_, '_>, q: usize) -> bool {
    let rows = gx.graph.record_rows.iter().filter(|r| crate::rerun::epq::marks::copied(gx, r.scanrelid));
    rows.filter_map(|r| inner_entry(gx, r)).any(|(x, at)| upstream(gx, x, at).contains(&q))
}

/// Whether a product of `set` (`upstream(q, boundary)`) is read outside it: by another pipeline, or by `q` at or past
/// `boundary` (then the recheck could not skip it). D3278 (CTE TWICE): a read by another copy entry of the rechecking
/// statement (`beside`, `entries`) below its own boundary is none: that entry hands its own mark's stored row too (a
/// writing CTE's store two CteScans read, each ROW_MARK_COPY), so no rerun reads the product.
pub fn read_elsewhere(gx: &crate::graph::Gx<'_, '_>, q: usize, boundary: usize, set: &[usize], beside: &[(usize, usize)]) -> bool {
    let ps = gx.pipelines();
    let producer = |c: usize, slot: u8| (0..c).rev().find(|&p| ps[p].desc.produces() == Some(slot));
    let pre = pre_slots(gx, q, boundary);
    let past = ps[q].desc.consumes.iter().filter(|s| !pre.contains(s)).any(|&s| producer(q, s).is_some_and(|p| set.contains(&p)));
    let entered = |c: usize, s: u8| beside.iter().any(|&(e, at)| e == c && pre_slots(gx, e, at).contains(&s));
    past || (0..ps.len()).filter(|c| *c != q && !set.contains(c)).any(|c| ps[c].desc.consumes.iter().any(|&s| producer(c, s).is_some_and(|p| set.contains(&p)) && !entered(c, s)))
}

/// D3278 (CTE TWICE): the copy entries of the COPY marks `rtis` (each mark's record row and the pipeline op where its stored
/// row enters: `boundary`), for `read_elsewhere`'s `beside`.
pub fn entries<'g>(gx: &crate::graph::Gx<'g, '_>, rtis: impl Iterator<Item = u32>) -> PgVec<'g, (usize, usize)> {
    let n = gx.pipelines().len();
    let rows = rtis.filter_map(|rti| gx.graph.record_rows.iter().find(|r| r.scanrelid == rti));
    rows.filter_map(|r| (0..n).find_map(|q| boundary(gx, q, r).map(|at| (q, at)))).collect_in(gx.mcx)
}

/// The op of graph pipeline `q` where a COPY mark's stored row `r` enters, if `q` runs its scan: a SubqueryScan's first op
/// (plan node `r.node_id`); D890, a function row's (`node_id` -1): the op after its FROM step (srf.function, inline or
/// store form), so the function is never called again and the FunctionScan's qual and projection run on the row.
pub fn boundary(gx: &crate::graph::Gx<'_, '_>, q: usize, r: &RecordRow<'_>) -> Option<usize> {
    let p = &gx.pipelines()[q];
    if r.whole > 0 {
        return union_at(p, r);
    }
    if r.node_id >= 0 {
        return p.node_ops.iter().find(|n| n.node_id == r.node_id).map(|n| n.first_op as usize);
    }
    let from = |o: &Op<'_>| matches!(o, Op::Srf(d) if matches!(d.place, SrfPlace::From { scanrelid, .. } if u32::from(scanrelid) == r.scanrelid));
    p.ops.iter().position(from).map(|j| j + 1)
}

/// D1685: a flattened UNION ALL's row (`RecordRow::whole`, the plan node its Append) enters the union's emit, past its
/// Union op, when the emit is a hash build (the union settled behind a Hash: the target probes it, so the recheck's
/// first row is the target's, as in PostgreSQL's plan): the arms feeding the emit's spool are below the entry (never
/// rerun). An arm's feed or a streamed union's arm (the Append's ops there) is no entry: the write refuses by COPY_MARK.
fn union_at(p: &gravity_decompose::graph::PipelineNode<'_>, r: &RecordRow<'_>) -> Option<usize> {
    let at = p.node_ops.iter().find(|n| n.node_id == r.node_id)?.first_op as usize;
    let union = matches!(p.ops.get(at), Some(Op::Append(AppendDesc::Union { prune: None, .. })));
    let build = matches!(p.ops.last(), Some(Op::Join(JoinDesc::Build { .. })));
    (union && build && matches!(p.source, Source::Breaker { .. })).then_some(at + 1)
}

/// D732-rest: pipeline `k`'s source with its copy entry (`SourceMech::copy_entry`); every other hook is the source's own.
pub fn wrap<'g>(mcx: Mcx<'g>, inner: PgBox<'g, dyn SourceMech + 'g>, entry: CopyEntry<'g>) -> PgBox<'g, dyn SourceMech + 'g> {
    mcx::pg_box_dyn!(mcx, Copied { inner, entry } => dyn SourceMech + 'g)
}

struct Copied<'g> {
    inner: PgBox<'g, dyn SourceMech + 'g>,
    entry: CopyEntry<'g>,
}

impl SourceMech for Copied<'_> {
    fn declare(&mut self, pb: &mut ProgramBuilder) { self.inner.declare(pb) }
    fn site_desc(&self, arena: ArenaRef) -> Option<SiteDesc> { self.inner.site_desc(arena) }
    fn seal(&mut self, px: &mut Px<'_, '_>, k: usize, site: &mut SiteDesc) { self.inner.seal(px, k, site) }
    fn pipeline_desc(&self, px: &Px<'_, '_>, k: usize) -> Pipeline { self.inner.pipeline_desc(px, k) }
    fn once(&self) -> bool { self.inner.once() }
    fn open_words(&self, w: &mut OpenWords) { self.inner.open_words(w) }
    fn read_plan(&self) -> ReadPlan { self.inner.read_plan() }
    fn verdicts(&self) -> &'static [VerdictKind] { self.inner.verdicts() }
    fn decode_order(&self) -> DecodeOrder { self.inner.decode_order() }
    fn fill_late(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, win: &Win, at: Option<(Value, Mask)>) { self.inner.fill_late(b, env, win, at) }
    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) -> Value { self.inner.open(b, env) }
    fn next_window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l_eof: Block) -> Win { self.inner.next_window(b, env, l_eof) }
    fn word_win<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, win: &Win) -> Option<WordWin<'e>> { self.inner.word_win(b, env, win) }
    fn packed(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, win: &Win, attno: i16, base: Value) -> Option<Packed> { self.inner.packed(b, env, win, attno, base) }
    fn row<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, win: &Win, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e> { self.inner.row(b, env, win, j, carry, next) }
    fn granule_ident(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, win: &Win) -> Option<GranuleIdent> { self.inner.granule_ident(b, env, win) }
    fn covers(&self) -> u64 { self.inner.covers() }
    fn passed(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, win: &Win, row: &RowVals) { self.inner.passed(b, env, win, row) }
    fn fills(&self) -> bool { self.inner.fills() }
    fn fills_blocks(&self) -> bool { self.inner.fills_blocks() }
    fn passed_block(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, win: &Win, base: Value, m: Mask) { self.inner.passed_block(b, env, win, base, m) }
    fn locator(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals) -> Option<Locator> { self.inner.locator(b, env, row) }
    fn write_back(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, cur: Value, out: Value) { self.inner.write_back(b, env, cur, out) }
    fn copy_entry(&self) -> Option<&CopyEntry> { Some(&self.entry) }
}
