//! Kind `nest.loop` (NestLoop INNER / LEFT / SEMI / ANTI over an embedded inner; M8-plan §3.1, the contract's R1-R6; B1)
//! (cards/M8.md, M8-changes.md §1). PG's ExecNestLoop per outer row: the NestLoopParams written from the outer row
//! (params.rs `store`), the inner re-opened (its per-open words and its source's seek, whose keys read those words, L0-a),
//! then each inner row joined to the outer one and tested by the join qual in PG's order; a match hands the joined row
//! on, SEMI and a unique inner stop at the first (PG's single_match), ANTI leaves at the first, and an unmatched outer
//! row of a LEFT or ANTI nest is handed on NULL-extended last. The other qual is the filter op after this one.
//! The step's offset word keeps its place in the outer row across a stop, so a resumed call re-enters the row and never
//! re-opens the inner: 0 fresh, 1 walking with no match, 2 walking after a match, 3 the final (NULL-extended) row handed
//! on, 4 a single match handed on. The inner's cursor names the row handed on (nest/inner.rs), so a stop past it walks on
//! and one inside it (a later expanding step's offset set, D688) re-enters it. The parameters are written again at every
//! entry: a resumed call re-enters the same outer row, and a NestLoopParam is a Var of it (a volatile one refuses).
//! A pgrc inner's dictionary and window cells are made site-free under the inner's site before the join qual reads them
//! (`site_free`): the owner's continuation renders through its own site. A nest's pipeline runs serially (`StepMech::serial`) unless `widen` marks its inner site one a worker probes (D3014O nest.parallel) or finds its inner an eager spool filled whole ahead of the outer (D4532), and an instrumented one refuses. Owner M8-NEST.
use gravity_decompose::join::JoinDesc;
use gravity_decompose::Op;
use gravity_ir::builder::{FunctionBuilder, ProgramBuilder};
use gravity_ir::{Block, ConvOp, IntBinOp, IntCond, Type, Value};
use types_nodes::jointype::JoinType;

use crate::rerun::inner as nest_inner;
use crate::rerun::inner::Inner;
use crate::cell::{At, Cell, CellHome, CellLife, Nullable};
use crate::face::kinds::KindId;
use crate::face::step::{Expand, ExpandKind, ExprFace, StepMech};
use crate::frame::env::{Env, Filled};
use crate::frame::frame::{Next, StepMaker};
use crate::frame::open::{Base, OpenWord, OpenWords};
use crate::frame::px::{ExId, Px};
use crate::frame::row::{Carry, RowVals};
use crate::frame::stop::{self, Exits, ResumeWord};
use crate::admit::refusal::{Reason, ShapeWhy};
use crate::ops::Table;
use gravity_program::Until;
use mcx::{CollectIn, GrowIn, Mcx, PgBox, PgVec};

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::NestLoop, &StepMaker(make))], ..crate::ops::NONE };

/// The offset word's states (the module doc).
const WALKING: i64 = 1;
const MATCHED: i64 = 2;
const FINAL: i64 = 3;
const SINGLE: i64 = 4;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Jt {
    Inner,
    Left,
    Semi,
    Anti,
}

struct Loop<'g> {
    /// The planning context the step's lowering-time vectors (`params`, `qual`, `shape`) live in.
    mcx: Mcx<'g>,
    node: i32,
    jt: Jt,
    /// PG's single_match: SEMI, or an inner the planner proved unique for the outer row.
    single: bool,
    params: PgVec<'g, (i32, ExId)>,
    qual: PgVec<'g, ExId>,
    ex: &'static ExprFace,
    inner: Inner<'g>,
    offset: i32,
    held: u8,
    /// D1086: the inner holds an owner of its own (a nest, a SubPlan) whose re-opens may pass these parameters to a store:
    /// a resumed row keeps their seen bits (params.rs `store_row`).
    keep: bool,
    /// The inner's output cells, their values' types and columns as the first joined row carried them (its NULL-extended
    /// row's shape), made while lowering.
    shape: core::cell::RefCell<Option<(PgVec<'g, Cell>, PgVec<'g, Type>, PgVec<'g, Option<u16>>)>>,
    /// D2157: the inner's eager-form Material filled at the first outer row (material's rerun/spool/lazy.rs).
    lazy: Option<crate::rerun::spool::lazy::Lazy>,
    /// Construct C2: the licence a loud fill the first-row wait cannot defer runs ahead in pipeline order under
    /// (lazy.rs `declare`). Held, not read.
    #[allow(dead_code)]
    fill_ahead: Option<gravity_decompose::effects::Licence>,
    /// D2709: a streamed sorted group (`Op::GroupRun`, ops/group/stream.rs) after the nest in its pipeline. It hands a group
    /// on when the next group's first row comes, and a stop inside that hand-on resumes by re-entering the row (its `at`);
    /// a joined row's `at` is its `next` (`joined`), so the stop cannot say so, and every stop the continuation makes is
    /// inside such a hand-on: a resumed row re-enters the inner row it was at (PG's agg_retrieve_direct keeps that row as
    /// the next group's first), never walks past it. Only filters, gates and projections between (no step keeping per-row
    /// state, as a LIMIT's count, that a re-entered row would pass twice).
    regroup: bool,
    /// D3014O nest.parallel: the inner's site is a worker probe (`widen::mark`), or (D4532) the inner reads an eager spool
    /// filled whole ahead of the outer (`widen::spool`), so the step does not keep its pipeline serial.
    wide: bool,
    /// D3776: a LEFT nest lowers the steps after it once for its matched and NULL-extended rows (`lower_shared`).
    share: bool,
}

fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

fn make<'g>(px: &mut Px<'g, '_>, k: usize, j: usize) -> PgBox<'g, dyn StepMech + 'g> {
    let gx = px.gx;
    let p = &gx.pipelines()[k];
    let node = nest_inner::op_node(p, j);
    let Some(Op::Join(JoinDesc::Nest { jointype, params, qual, unique, .. })) = p.ops.get(j) else { crate::refuse!(node, Reason::Bug { payload: "a nest step over another op" }) };
    let jt = match *jointype {
        JoinType::JOIN_INNER => Jt::Inner,
        JoinType::JOIN_LEFT => Jt::Left,
        JoinType::JOIN_SEMI => Jt::Semi,
        JoinType::JOIN_ANTI => Jt::Anti,
        _ => shape(node, "a nest of another join type"),
    };
    let ex: &'static ExprFace = crate::frame::iface::expr_face(&node);
    let mcx = px.mcx();
    let mut ps: PgVec<'g, (i32, ExId)> = PgVec::new_in(mcx);
    for (paramno, value) in gravity_decompose::facts::hoist::nest_params(params) {
        let e = ex.0.compile_at(px, (k, j), value, None, node);
        if !px.placed(e).held.is_empty() {
            shape(node, "a nest parameter computed by a volatile call");
        }
        ps.push_in((paramno, e));
    }
    let qual: PgVec<'g, ExId> = qual.iter().map(|q| ex.0.compile_at(px, (k, j), q, None, node)).collect_in(mcx);
    let held = qual.iter().map(|&q| px.placed(q).held.len()).sum::<usize>() as u8;
    // D2709: the first op after the nest past filters, gates and projections (no per-row state between) is a streamed group
    let regroup = p.ops[j + 1..].iter().find(|o| !matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_))).is_some_and(|o| matches!(o, Op::GroupRun(_)));
    let lean = jt == Jt::Left && !regroup && gx.fills_share(k);
    let inner = nest_inner::make_through(px, k, j, lean);
    let keep = matches!(p.ops.get(j), Some(Op::Join(JoinDesc::Nest { inner: ip, .. })) if ip.ops.iter().any(|o| matches!(o, Op::Join(JoinDesc::Nest { .. }) | Op::SubPlan(_))));
    // D4532: a nest over an eager spool its quiet fill makes whole ahead of the outer widens, and its fill stays in
    // pipeline order (no D2157 wait: every worker reads the record from the outer's first row on)
    let spool = super::widen::spool(px, k, j, &inner, regroup, keep);
    let (lazy, fill_ahead) = if spool { (None, None) } else { crate::rerun::spool::lazy::declare(px, k, j) };
    let offset = stop::resume_word(px, ResumeWord::StepOffset { k, j: crate::frame::px::key_j(j) });
    let single = jt == Jt::Semi || *unique;
    // D3014O: a streamed group after the nest or an inner holding an owner of its own keeps the step serial (widen.rs)
    let wide = spool || super::widen::mark(px, k, j, &inner, lazy.is_some(), regroup, keep);
    let share = lean && inner.walks_once();
    mcx::pg_box_dyn!(mcx, Loop { mcx, node, jt, single, params: ps, qual, ex, inner, offset, held, keep, shape: core::cell::RefCell::new(None), lazy, fill_ahead, regroup, wide, share } => dyn StepMech + 'g)
}

/// D688: a later expanding step's offset word is set (it stopped inside the row handed on: re-enter it); `None` without one.
fn later_set(b: &mut FunctionBuilder<'_>, env: &Env) -> Option<Value> {
    let later: PgVec<'_, i32> = env.expands.iter().filter(|e| env.step.is_some_and(|j| e.0 > j)).map(|e| e.1).collect_in(env.mcx);
    any_set(b, env, later)
}

/// Whether any of offset words `offs` is set; `None` without one.
fn any_set(b: &mut FunctionBuilder<'_>, env: &Env, offs: impl IntoIterator<Item = i32>) -> Option<Value> {
    let mut set = None;
    for o in offs {
        let w = env.load_word(b, o);
        set = Some(set.map_or(w, |a| b.int_bin(IntBinOp::Or, a, w)));
    }
    Some(b.icmp(IntCond::Ne, set?, env.zero))
}

/// The outer row joined to inner cells `(cells, vals, cols)`: the outer's cells then the inner's, each inner cell's life
/// at most the row (the inner's window moves on with the next outer row), and the map of the inner's columns into it
/// (`env.inner`). The row resumes at the outer row (`next = at`: a stop re-enters it).
pub(crate) fn joined<'r, 'm>(outer: &RowVals<'r>, inner: (&[Cell], &[(Value, Option<Value>)], &[Option<u16>]), carry: &Carry, mcx: Mcx<'m>) -> (RowVals<'r>, PgVec<'m, Option<u16>>) {
    let base = outer.row.cells.len();
    let mut r = RowVals { row: outer.row.clone(), vals: outer.vals.clone(), cols: outer.cols.clone(), carry: carry.copy_in(*outer.vals.allocator()), next: outer.at, at: outer.at };
    for (c, &v) in inner.0.iter().zip(inner.1.iter()) {
        let mut c = *c;
        if matches!(c.life.until, Until::Window | Until::Part | Until::Life) {
            c.life.until = Until::Row;
        }
        r.row.cells.push_in(c);
        r.vals.push_in(v);
    }
    let map = inner.2.iter().map(|c| c.map(|k| (base + k as usize) as u16)).collect_in(mcx);
    (r, map)
}

impl Loop<'_> {
    /// The joined row `row2` handed on through `next` with the inner's columns mapped (`env.inner`).
    fn hand_on<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, (row2, map): (RowVals, PgVec<'e, Option<u16>>), next: Next<'_, 'e>) -> Carry<'e> {
        let saved = env.inner.replace(map);
        let c = next(b, env, &row2);
        env.inner = saved;
        c
    }

    /// A pgrc inner's dictionary (`Rep::Code`) and window (`PackedNum` `Window`) cells read through the inner's site, which
    /// the owner's continuation does not lower under: each is made site-free here, under the inner's site (a code its
    /// rendered varlena, a window element its order word); `None` when the row holds no such cell.
    fn site_free<'r>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, r: &RowVals<'r>) -> Option<RowVals<'r>> {
        use gravity_program::{NumForm, Rep};
        let sited = |c: &Cell| matches!(c.rep, Rep::Code { .. } | Rep::PackedNum { form: NumForm::Window, .. } | Rep::PackedNumDyn);
        if !r.row.cells.iter().any(sited) {
            return None;
        }
        // D3845: an inner reading a run (a LATERAL top-N's or group's emit) has no scan site, but a code its run record holds
        // renders through the record's producer site (`Env::site_of`, D538's late-rendered code lanes), as at any emit
        let site = self.inner.site();
        let record = |c: &Cell| matches!(c.rep, Rep::Code { .. }) && matches!(c.life.home, CellHome::Record(_)) && env.site_of(c).is_some();
        if site.is_none() && !r.row.cells.iter().filter(|c| sited(c)).all(record) {
            shape(self.node, "a nest inner column read by its site's dictionary or window form (an inner without a scan site)");
        }
        let here = site.or(env.site);
        let saved = core::mem::replace(&mut env.site, here);
        let (mut cells, mut vals) = (r.row.cells.clone(), r.vals.clone());
        {
            let mut cx = env.lx(b, self.node);
            for (c, v) in cells.iter_mut().zip(vals.iter_mut()) {
                if !sited(c) {
                    continue;
                }
                let tf = crate::step::expr::tf_datum(c.typid);
                let cr = crate::rep::CellRef { cell: &*c, val: v.0, null: v.1 };
                let (cell, val) = if matches!(c.rep, Rep::Code { .. }) {
                    let (d, null) = crate::rep::datum(&mut cx, cr, &tf);
                    let life = CellLife { until: Until::Row, home: CellHome::FrameSlot };
                    (Cell { rep: Rep::VarlenaPtr, life, at: crate::rep::word::datum::off_lane(c.at), null: Nullable::Maybe, ..*c }, (d, Some(null)))
                } else {
                    // the arm itself, not `rep::retain`: a window cell's life already reaches the row, so that would keep
                    // the lane cell, and the NULL-extended row (`final_row`) would then hold a window cell off its lane
                    let k = (crate::rep::packednum::datum::ARM.retain)(&mut cx, cr, &tf, Until::Row, crate::rep::RetainHome::Frame);
                    (k.cell, (k.val, k.null))
                };
                *c = cell;
                *v = val;
            }
        }
        env.site = saved;
        Some(RowVals { row: crate::cell::Row { cells }, vals, cols: r.cols.clone(), carry: r.carry.copy_in(*r.vals.allocator()), next: r.next, at: r.at })
    }

    /// One inner row `r` against the outer row: the join qual in order, then the match's hand-on per the join type.
    /// `(carry, leave)`: `leave` ends the walk (a single match handed on, ANTI's first match).
    fn on_row<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, r: &RowVals, next: Next<'_, 'e>) -> (Carry<'e>, Option<Value>) {
        // D2670: a pair the join qual drops gives its by-reference results back (PG's per-tuple reset per inner row); D3591:
        // and resets the function-manager windows of the sites the pair called (`Ir::reset_seen`), the outer row's kept
        let mark = env.hir.mark(b);
        let seen = env.hir.seen();
        let plain = self.site_free(b, env, r);
        let r = plain.as_ref().unwrap_or(r);
        let types: PgVec<'_, Type> = r.vals.iter().map(|&(v, _)| b.value_type(v)).collect_in(self.mcx);
        self.shape.borrow_mut().get_or_insert_with(|| (mcx::to_vec_in(&r.row.cells[..], self.mcx), types, mcx::to_vec_in(&r.cols[..], self.mcx)));
        let (row2, map) = joined(row, (&r.row.cells, &r.vals, &r.cols), &r.carry, env.mcx);
        let ctypes: PgVec<'_, Type> = r.carry.params().iter().map(|&v| b.value_type(v)).collect_in(env.mcx);
        let jtypes: PgVec<'_, Type> = ctypes.iter().copied().chain([Type::Bool]).collect_in(env.mcx);
        let l_join = b.create_block(&jtypes[..]);
        let no = b.bconst(false);
        let saved = env.inner.replace(mcx::to_vec_in(&map[..], env.mcx));
        let mut l_drop = None;
        for &q in self.qual.iter() {
            let pass = self.ex.0.pred(b, env, self.node, &row2, q);
            let (l_yes, l_no) = (b.create_block(&[]), *l_drop.get_or_insert_with(|| b.create_block(&[])));
            b.cond_br(pass, (l_yes, &[]), (l_no, &[]));
            b.switch_to(l_yes);
        }
        env.inner = saved;
        let l_on = b.current();
        if let Some(l) = l_drop {
            b.switch_to(l);
            // EXPLAIN ANALYZE: Rows Removed by Join Filter (ExecNestLoop's InstrCountFiltered1), one per inner row the walk
            // drops; the walk tests PostgreSQL's rows in its order and stops where it stops
            crate::frame::instr::count_one(b, env.ctx, self.node, crate::frame::instr::Ctr::Filtered1);
            if let Some(m) = mark {
                env.hir.give_back(b, m);
            }
            env.hir.reset_seen(b, seen, env.mcx);
            b.br(l_join, &mcx::concat_in(&[&r.carry.params()[..], &[no][..]], env.mcx));
            b.switch_to(l_on);
        }
        let yes = b.bconst(true);
        let (state, leave) = match (self.jt, self.single) {
            (Jt::Anti, _) => (MATCHED, yes),
            (_, true) => (SINGLE, yes),
            _ => (MATCHED, no),
        };
        let s = b.iconst(Type::I64, state);
        env.store_word(b, self.offset, s);
        // D1933 (c): the inner row stays in reach of a write's EvalPlanQual redo in the IR (epq_ir.rs `Redo::Nest`)
        env.redo_nests.push_in((self.node, (mcx::to_vec_in(&r.row.cells[..], env.mcx), mcx::to_vec_in(&r.vals[..], env.mcx), mcx::to_vec_in(&r.cols[..], env.mcx), Filled::No)));
        // D2739: and the outer row, for a write whose target is this nest's inner (epq_ir.rs `Redo::InnerOf`)
        env.redo_outers.push_in((self.node, (mcx::to_vec_in(&row.row.cells[..], env.mcx), mcx::to_vec_in(&row.vals[..], env.mcx), mcx::to_vec_in(&row.cols[..], env.mcx), Filled::No)));
        let c = match self.jt {
            Jt::Anti => r.carry.copy_in(env.mcx),
            _ => self.hand_on(b, env, (row2, map), next),
        };
        env.redo_outers.pop();
        env.redo_nests.pop();
        // D2967: a pair handed on gives its bytes back too once it is through (PG's per-tuple reset per inner row); what
        // a later step or the end keeps it copies, as across windows
        if let Some(m) = mark {
            env.memo.clear();
            env.hir.give_back(b, m);
        }
        env.hir.reset_seen(b, seen, env.mcx);
        b.br(l_join, &mcx::concat_in(&[&c.params()[..], &[leave][..]], env.mcx));
        b.switch_to(l_join);
        let p = b.block_params(l_join);
        (Carry::from_params(&p[..p.len() - 1], env.mcx), Some(p[p.len() - 1]))
    }

    /// The outer row NULL-extended (LEFT) or itself (ANTI), handed on last: the inner's cells NULL.
    #[inline(never)] // D3833: its frame stays out of `lower`'s, which every nest level of the walk's recursion holds
    fn final_row<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, carry: &Carry, next: Next<'_, 'e>) -> Carry<'e> {
        let j = {
            let held = self.shape.borrow();
            let Some((cells, types, cols)) = held.as_ref() else { shape(self.node, "a LEFT / ANTI nest whose inner hands on no row (its NULL-extended row's shape)") };
            let (cells, vals) = null_row(b, env, &cells[..], &types[..]);
            let s = b.iconst(Type::I64, FINAL);
            env.store_word(b, self.offset, s);
            // producerchain C: the NULL-extended row stays in reach of a write's EvalPlanQual redo too (epq_ir.rs `Redo::Nest`:
            // no inner row matched, so no test tuple: the redo hands the newest version on NULL-extended again)
            env.redo_nests.push_in((self.node, (mcx::to_vec_in(&cells[..], env.mcx), mcx::to_vec_in(&vals[..], env.mcx), mcx::to_vec_in(&cols[..], env.mcx), Filled::Yes)));
            joined(row, (&cells[..], &vals[..], &cols[..]), carry, env.mcx)
        };
        let c = self.hand_on(b, env, j, next);
        env.redo_nests.pop();
        c
    }
}

/// D3776: the shared rest's block, made at the matched row (`on_row_shared`): its parameters are the inner's values, their
/// NULL flags, the final tag, the row's region mark and the carry.
struct Share<'g> {
    l_cont: Block,
    cells: PgVec<'g, Cell>,
    types: PgVec<'g, Type>,
    cols: PgVec<'g, Option<u16>>,
    seen: usize,
    mark: bool,
}

impl<'g> Loop<'g> {
    /// `on_row` for a sharing nest: the join qual in order, and a match enters the shared rest (`Share::l_cont`) with the
    /// inner row's values instead of lowering the rest here. The walk goes on past a dropped row only.
    fn on_row_shared<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, r: &RowVals, sh: &mut Option<Share<'g>>) -> (Carry<'e>, Option<Value>) {
        let mark = env.hir.mark(b);
        let seen = env.hir.seen();
        let plain = self.site_free(b, env, r);
        let r = plain.as_ref().unwrap_or(r);
        let types: PgVec<'g, Type> = r.vals.iter().map(|&(v, _)| b.value_type(v)).collect_in(self.mcx);
        let (row2, map) = joined(row, (&r.row.cells, &r.vals, &r.cols), &r.carry, env.mcx);
        let ctypes: PgVec<'_, Type> = r.carry.params().iter().map(|&v| b.value_type(v)).collect_in(env.mcx);
        let jtypes: PgVec<'_, Type> = ctypes.iter().copied().chain([Type::Bool]).collect_in(env.mcx);
        let l_join = b.create_block(&jtypes[..]);
        let no = b.bconst(false);
        let saved = env.inner.replace(map);
        let mut l_drop = None;
        for &q in self.qual.iter() {
            let pass = self.ex.0.pred(b, env, self.node, &row2, q);
            let (l_yes, l_no) = (b.create_block(&[]), *l_drop.get_or_insert_with(|| b.create_block(&[])));
            b.cond_br(pass, (l_yes, &[]), (l_no, &[]));
            b.switch_to(l_yes);
        }
        env.inner = saved;
        let l_on = b.current();
        if let Some(l) = l_drop {
            b.switch_to(l);
            crate::frame::instr::count_one(b, env.ctx, self.node, crate::frame::instr::Ctr::Filtered1);
            if let Some(m) = mark {
                env.hir.give_back(b, m);
            }
            env.hir.reset_seen(b, seen, env.mcx);
            b.br(l_join, &mcx::concat_in(&[&r.carry.params()[..], &[no][..]], env.mcx));
            b.switch_to(l_on);
        }
        let s = b.iconst(Type::I64, if self.single { SINGLE } else { MATCHED });
        env.store_word(b, self.offset, s);
        if sh.is_some() {
            crate::refuse!(self.node, Reason::Bug { payload: "a sharing nest's inner row handed on at two places" });
        }
        let mut ktypes: PgVec<'_, Type> = mcx::to_vec_in(&types[..], env.mcx);
        ktypes.extend_in(core::iter::repeat_n(Type::Bool, types.len() + 1));
        if mark.is_some() {
            ktypes.extend_in([Type::Ptr, Type::Ptr]);
        }
        ktypes.extend_from_slice_in(&ctypes[..]);
        let l_cont = b.create_block(&ktypes[..]);
        let args: PgVec<'_, Value> = r.vals.iter().map(|v| v.0).chain(r.vals.iter().map(|v| v.1.unwrap_or(no))).chain([no]).chain(mark.into_iter().flat_map(|m| [m.0, m.1])).chain(r.carry.params()).collect_in(env.mcx);
        b.br(l_cont, &args);
        *sh = Some(Share { l_cont, cells: mcx::to_vec_in(&r.row.cells[..], self.mcx), types, cols: mcx::to_vec_in(&r.cols[..], self.mcx), seen, mark: mark.is_some() });
        env.memo.clear();
        b.switch_to(l_join);
        let p = b.block_params(l_join);
        (Carry::from_params(&p[..p.len() - 1], env.mcx), Some(p[p.len() - 1]))
    }

    /// `lower` for a sharing LEFT nest (`share`, D3776). The steps after the nest are lowered once, in `Share::l_cont`,
    /// which a matched row enters from the walk and the NULL-extended row from `l_fin`; a matched row's rest goes back to
    /// the walk past its inner row (`skip` 1; a single match ends the outer row). `l_walk` takes a final flag, so the final
    /// row re-entered after a stop comes through it too and `l_walk` stays the one header of that loop.
    #[inline(never)] // D3833: its frame stays out of `lower`'s, which every nest level of the walk's recursion holds
    fn lower_shared<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        if let Some(l) = self.lazy.as_ref() {
            l.wait(b, env, row, self.offset);
        }
        let t = b.bconst(true);
        let fresh = self.keep.then(|| {
            let o = env.load_word(b, self.offset);
            b.icmp(IntCond::Eq, o, env.zero)
        });
        for &(id, e) in self.params.iter() {
            let (v, valid) = self.ex.0.emit(b, env, self.node, row, e);
            let null = b.int_bin(IntBinOp::Xor, valid, t);
            crate::rerun::params::store_row(b, env, self.node, id, (v, null), fresh);
        }
        let x = env.x;
        let ctypes: PgVec<'_, Type> = row.carry.params().iter().map(|&v| b.value_type(v)).collect_in(env.mcx);
        let wtypes: PgVec<'_, Type> = [Type::I64, Type::Bool].into_iter().chain(ctypes.iter().copied()).collect_in(env.mcx);
        let (l_out, l_walk, l_fin) = (b.create_block(&ctypes[..]), b.create_block(&wtypes[..]), b.create_block(&ctypes[..]));
        let o = env.load_word(b, self.offset);
        let later = later_set(b, env);
        let (zero, f) = (env.zero, b.bconst(false));
        let cp = row.carry.params();
        let mcx = env.mcx;
        let walk_args = |skip: Value, fin: Value, c: &[Value]| [skip, fin].into_iter().chain(c.iter().copied()).collect_in(mcx);
        let (l_fresh, l_res) = (b.create_block(&[]), b.create_block(&[]));
        let is_fresh = b.icmp(IntCond::Eq, o, zero);
        b.cond_br(is_fresh, (l_fresh, &[]), (l_res, &[]));
        b.switch_to(l_fresh);
        let r = self.inner.open(b, env);
        let bad = b.icmp(IntCond::Slt, r, zero);
        let l_go = b.create_block(&[]);
        b.cond_br(bad, (x.l_exit, &[r]), (l_go, &[]));
        b.switch_to(l_go);
        let w = b.iconst(Type::I64, WALKING);
        env.store_word(b, self.offset, w);
        b.br(l_walk, &walk_args(zero, f, &cp));
        b.switch_to(l_res);
        let outer_later: PgVec<'_, i32> = env.expands.iter().filter(|e| env.step.is_some_and(|j| e.0 > j)).map(|e| e.1).collect_in(env.mcx);
        let skip = match any_set(b, env, outer_later.into_iter().chain(self.inner.offsets())) {
            Some(l) => {
                let one = env.one;
                b.select(l, zero, one)
            }
            None => env.one,
        };
        let (l_r3, l_r4, l_r12, l_r4or12) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
        let (fv, sv) = (b.iconst(Type::I64, FINAL), b.iconst(Type::I64, SINGLE));
        let is_final = b.icmp(IntCond::Eq, o, fv);
        b.cond_br(is_final, (l_r3, &[]), (l_r4or12, &[]));
        b.switch_to(l_r4or12);
        let is_single = b.icmp(IntCond::Eq, o, sv);
        b.cond_br(is_single, (l_r4, &[]), (l_r12, &[]));
        b.switch_to(l_r3);
        match later {
            Some(l) => b.cond_br(l, (l_walk, &walk_args(zero, t, &cp)), (l_out, &cp)),
            None => b.br(l_out, &cp),
        }
        b.switch_to(l_r4);
        match later {
            Some(l) => b.cond_br(l, (l_walk, &walk_args(zero, f, &cp)), (l_out, &cp)),
            None => b.br(l_out, &cp),
        }
        b.switch_to(l_r12);
        b.br(l_walk, &walk_args(skip, f, &cp));
        b.switch_to(l_walk);
        b.mark_loop_header(l_walk);
        env.bounded.push_in(l_walk);
        let p = b.block_params(l_walk);
        let (skip, fin, wc) = (p[0], p[1], Carry::from_params(&p[2..], env.mcx));
        let l_go = b.create_block(&[]);
        b.cond_br(fin, (l_fin, &wc.params()), (l_go, &[]));
        b.switch_to(l_go);
        let mut sh = None;
        let c = self.inner.walk(b, env, &x, (wc, skip), &mut |b, env, r| self.on_row_shared(b, env, row, r, &mut sh));
        let st = env.load_word(b, self.offset);
        let w = b.iconst(Type::I64, WALKING);
        let unmatched = b.icmp(IntCond::Eq, st, w);
        b.cond_br(unmatched, (l_fin, &c.params()), (l_out, &c.params()));
        let Some(sh) = sh else { shape(self.node, "a LEFT / ANTI nest whose inner hands on no row (its NULL-extended row's shape)") };
        b.switch_to(l_fin);
        let fc = mcx::to_vec_in(&b.block_params(l_fin)[..], env.mcx);
        let s = b.iconst(Type::I64, FINAL);
        env.store_word(b, self.offset, s);
        // a lane cell's value is its element's address, which a reader loads before it tests NULL: a zeroed word's
        let lanes = sh.cells.iter().any(|c| matches!(c.at, At::Lane(_)));
        let blank = lanes.then(|| {
            let slot = b.declare_stack_slot(16, 8);
            let a = b.stack_addr(slot);
            b.store(a, 0, zero);
            b.store(a, 8, zero);
            a
        });
        let mut args: PgVec<'_, Value> = mcx::reserved_in(2 * sh.types.len() + 3 + fc.len(), env.mcx);
        for (c, &ty) in sh.cells.iter().zip(sh.types.iter()) {
            let v = match (c.at, blank) {
                (At::Lane(_), Some(a)) if ty == Type::Ptr => a,
                (At::Lane(_), Some(a)) => b.conv(ConvOp::PtrToInt, ty, a),
                _ => b.const_raw(ty, 0),
            };
            args.push_in(v);
        }
        args.extend_in(core::iter::repeat_n(t, sh.types.len() + 1));
        if sh.mark {
            let Some(m) = env.hir.mark(b) else { crate::refuse!(self.node, Reason::Bug { payload: "a sharing nest's region mark lost" }) };
            args.extend_in([m.0, m.1]);
        }
        args.extend_from_slice_in(&fc[..]);
        b.br(sh.l_cont, &args);
        b.switch_to(sh.l_cont);
        let ks = b.block_params(sh.l_cont);
        let n = sh.types.len();
        let tag = ks[2 * n];
        let m = sh.mark.then(|| (ks[2 * n + 1], ks[2 * n + 2]));
        let carry = Carry::from_params(&ks[2 * n + 1 + 2 * sh.mark as usize..], env.mcx);
        let cells: PgVec<'_, Cell> = sh.cells.iter().map(|c| Cell { null: Nullable::Maybe, ..*c }).collect_in(env.mcx);
        let vals: PgVec<'_, (Value, Option<Value>)> = (0..n).map(|i| (ks[i], Some(ks[n + i]))).collect_in(env.mcx);
        // D3818: the inner row stays in reach of a write's or lock's EvalPlanQual redo (epq_ir.rs `Redo::Nest`), as
        // `on_row` / `final_row` keep it; whether it is the NULL-extended row is the block's tag, known at run time
        // (`Filled::At`). No outer row (`Env::redo_outers`): a LEFT nest's inner is never the target, and a MERGE, whose
        // NOT MATCHED row is a LEFT nest's NULL-extended one, does not share (`Gx::fills_share`)
        env.redo_nests.push_in((self.node, (mcx::to_vec_in(&cells[..], env.mcx), mcx::to_vec_in(&vals[..], env.mcx), mcx::to_vec_in(&sh.cols[..], env.mcx), Filled::At(tag))));
        let (row2, map) = joined(row, (&cells, &vals, &sh.cols), &carry, env.mcx);
        let memo = env.memo.fork();
        let c = self.hand_on(b, env, (row2, map), next);
        env.memo.restore(memo);
        env.redo_nests.pop();
        let l_matched = b.create_block(&ctypes);
        b.cond_br(tag, (l_out, &c.params()), (l_matched, &c.params()));
        b.switch_to(l_matched);
        let cm = mcx::to_vec_in(&b.block_params(l_matched)[..], env.mcx);
        if let Some(m) = m {
            env.memo.clear();
            env.hir.give_back(b, m);
        }
        env.hir.reset_seen(b, sh.seen, env.mcx);
        match self.single {
            true => b.br(l_out, &cm),
            false => {
                // D3810: an inner expanding step that handed the row on (its offset set: a nest of a bushy plan) may have
                // more rows for the same inner row, so the walk re-enters it (`skip` 0) as a resumed call does
                let skip = match any_set(b, env, self.inner.offsets()) {
                    Some(l) => {
                        let one = env.one;
                        b.select(l, zero, one)
                    }
                    None => env.one,
                };
                b.br(l_walk, &walk_args(skip, f, &cm));
            }
        }
        b.switch_to(l_out);
        env.store_word(b, self.offset, zero);
        Carry::from_params(b.block_params(l_out), env.mcx)
    }
}

/// An inner row's NULL-extended form over its shape (its cells and their values' types): each cell NULL, its life the row.
/// A lane cell's value is its element's address, which a reader loads before it tests NULL (rep/mod.rs `datum_word`): the
/// NULL cell is its column's value instead, a zero word; any other cell keeps its value's type. `final_row`'s (D3818: the
/// EvalPlanQual redo's NULL-extended row keeps the joined row's form instead, epq_ir.rs `left_merge`).
pub(crate) fn null_row<'e>(b: &mut FunctionBuilder<'_>, env: &Env<'e, '_>, cells: &[Cell], types: &[Type]) -> (PgVec<'e, Cell>, PgVec<'e, (Value, Option<Value>)>) {
    let t = b.bconst(true);
    let vals: PgVec<'e, (Value, Option<Value>)> = cells.iter().zip(types.iter()).map(|(c, &ty)| match c.at {
        At::Lane(_) => (env.zero, Some(t)),
        _ => (b.const_raw(ty, 0), Some(t)),
    }).collect_in(env.mcx);
    let cells: PgVec<'e, _> = cells.iter().map(|&c| Cell {
        life: CellLife { until: Until::Row, home: CellHome::FrameSlot },
        at: match c.at { At::Lane(l) => At::Value(l.attno), a => a },
        null: Nullable::Maybe,
        ..c
    }).collect_in(env.mcx);
    (cells, vals)
}

impl StepMech for Loop<'_> {
    fn open_words(&self, w: &mut OpenWords) {
        w.declare(OpenWord::open("nest.loop state", Base::Words, self.offset, Type::I64, 0));
        self.inner.owner_words(w);
    }

    fn declare(&mut self, pb: &mut ProgramBuilder) {
        self.inner.declare(pb);
    }

    fn prologue(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, x: &Exits) {
        self.inner.prologue(b, env, x);
    }

    fn epilogue(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, x: &Exits) {
        self.inner.epilogue(b, env, x);
    }

    fn expand(&self) -> Option<Expand> {
        Some(Expand { kind: ExpandKind::Nest, offset: self.offset, held: self.held })
    }

    /// D3810: a sharing LEFT nest lowers its continuation once (`lower_shared`); an INNER or SEMI nest once per place its
    /// inner hands a row on (one when the inner walks once); an ANTI nest once, for its unmatched row. Not a nest before a
    /// streamed group (`regroup`): its resumed row re-enters the inner row it handed on, which would hand it on again.
    fn hands_on_once(&self) -> bool {
        !self.regroup && match self.jt {
            Jt::Left => self.share,
            Jt::Anti => true,
            Jt::Inner | Jt::Semi => self.inner.walks_once() && self.lazy.is_none(),
        }
    }

    /// D3014O: a step `widen` marked (its inner a worker-probed btree site, or D4532's filled eager spool) runs on every
    /// worker; any other stays serial.
    fn serial(&self) -> bool {
        !self.wide
    }

    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        if self.share {
            return self.lower_shared(b, env, row, next);
        }
        if let Some(l) = self.lazy.as_ref() {
            l.wait(b, env, row, self.offset); // D2157: the Material's fill runs at the first outer row
        }
        let t = b.bconst(true);
        let fresh = self.keep.then(|| {
            let o = env.load_word(b, self.offset);
            b.icmp(IntCond::Eq, o, env.zero)
        });
        for &(id, e) in self.params.iter() {
            let (v, valid) = self.ex.0.emit(b, env, self.node, row, e);
            let null = b.int_bin(IntBinOp::Xor, valid, t);
            crate::rerun::params::store_row(b, env, self.node, id, (v, null), fresh);
        }
        let x = env.x;
        let ctypes: PgVec<'_, Type> = row.carry.params().iter().map(|&v| b.value_type(v)).collect_in(env.mcx);
        let wtypes: PgVec<'_, Type> = core::iter::once(Type::I64).chain(ctypes.iter().copied()).collect_in(env.mcx);
        let outer = matches!(self.jt, Jt::Left | Jt::Anti);
        let (l_out, l_walk) = (b.create_block(&ctypes[..]), b.create_block(&wtypes[..]));
        let l_final = outer.then(|| b.create_block(&ctypes[..]));
        let o = env.load_word(b, self.offset);
        let later = if self.regroup { Some(b.bconst(true)) } else { later_set(b, env) }; // D2709
        let zero = env.zero;
        let cp = row.carry.params();
        let (l_fresh, l_res) = (b.create_block(&[]), b.create_block(&[]));
        let fresh = b.icmp(IntCond::Eq, o, zero);
        b.cond_br(fresh, (l_fresh, &[]), (l_res, &[]));
        // a fresh outer row: the inner re-opened (a failing open leaves the row with its status)
        b.switch_to(l_fresh);
        let r = self.inner.open(b, env);
        let bad = b.icmp(IntCond::Slt, r, zero);
        let l_go = b.create_block(&[]);
        b.cond_br(bad, (x.l_exit, &[r]), (l_go, &[]));
        b.switch_to(l_go);
        let w = b.iconst(Type::I64, WALKING);
        env.store_word(b, self.offset, w);
        b.br(l_walk, &mcx::concat_in(&[&[zero][..], &cp[..]], env.mcx));
        // a resumed row: past the row handed on, or into it when a later expanding step stopped inside it, the outer's or
        // (B1b) an inner step's after the inner row (its step resumes inside its set)
        b.switch_to(l_res);
        let outer_later: PgVec<'_, i32> = env.expands.iter().filter(|e| env.step.is_some_and(|j| e.0 > j)).map(|e| e.1).collect_in(env.mcx);
        let inside = any_set(b, env, outer_later.into_iter().chain(self.inner.offsets()));
        let skip = match inside {
            _ if self.regroup => zero, // D2709: the streamed group after the nest re-enters the row it stopped at
            Some(l) => {
                let one = env.one;
                b.select(l, zero, one)
            }
            None => env.one,
        };
        let (l_r3, l_r4, l_r12) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
        let (f, s) = (b.iconst(Type::I64, FINAL), b.iconst(Type::I64, SINGLE));
        let is_final = b.icmp(IntCond::Eq, o, f);
        let l_r4or12 = b.create_block(&[]);
        b.cond_br(is_final, (l_r3, &[]), (l_r4or12, &[]));
        b.switch_to(l_r4or12);
        let is_single = b.icmp(IntCond::Eq, o, s);
        b.cond_br(is_single, (l_r4, &[]), (l_r12, &[]));
        // the final row handed on: re-entered, or through (the outer row is done)
        b.switch_to(l_r3);
        match (l_final, later) {
            (Some(lf), Some(l)) => b.cond_br(l, (lf, &cp), (l_out, &cp)),
            _ => b.br(l_out, &cp),
        }
        // a single match handed on: re-entered (walked again from it, then left), or through
        b.switch_to(l_r4);
        match later {
            Some(l) => b.cond_br(l, (l_walk, &mcx::concat_in(&[&[zero][..], &cp[..]], env.mcx)), (l_out, &cp)),
            None => b.br(l_out, &cp),
        }
        b.switch_to(l_r12);
        b.br(l_walk, &mcx::concat_in(&[&[skip][..], &cp[..]], env.mcx));
        // the walk over the inner's rows
        b.switch_to(l_walk);
        let p = b.block_params(l_walk);
        let (skip, wc) = (p[0], Carry::from_params(&p[1..], env.mcx));
        let c = self.inner.walk(b, env, &x, (wc, skip), &mut |b, env, r| self.on_row(b, env, row, r, &mut *next));
        match l_final {
            Some(lf) => {
                let st = env.load_word(b, self.offset);
                let w = b.iconst(Type::I64, WALKING);
                let unmatched = b.icmp(IntCond::Eq, st, w);
                b.cond_br(unmatched, (lf, &c.params()), (l_out, &c.params()));
                b.switch_to(lf);
                let fc = Carry::from_params(b.block_params(lf), env.mcx);
                let c2 = self.final_row(b, env, row, &fc, &mut *next);
                b.br(l_out, &c2.params());
            }
            None => b.br(l_out, &c.params()),
        }
        // the outer row done: the next one is fresh
        b.switch_to(l_out);
        env.store_word(b, self.offset, zero);
        Carry::from_params(b.block_params(l_out), env.mcx)
    }
}
