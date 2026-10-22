//! Kind `window.agg`'s partition buffer (window/window.rs): its producer and its reader over the window object (hosts
//! v4/m10.rs). Owner M10-WINDOW.
//! - The end (the Materialize under the WindowAgg, V4R-C): each input row stored whole, `m10_win_put`: the child's columns
//!   as datums, then each window function's arguments and FILTER (decompose window.rs `arg_cells`), evaluated here once.
//!   A cell whose evaluation raises is marked (`m10_win_argerr`) and stored NULL: the object raises its error when PG
//!   would evaluate it (a frame, an offset, a lag reading that row), never for a row nothing reads. Under a VOLATILE
//!   argument no cell is evaluated here: each is stored `CELL_LAZY` and the object evaluates it at each read (D1232). A
//!   computed by-reference argument (D3584, lower.rs `lazy_cells`) is evaluated here, and the object's store keeps a wide
//!   value as `CELL_LAZY` instead of its copy, evaluating it again where a function reads it. A
//!   `step_arg` cell (D1415) is stored NULL: the window step writes it before the object reads it.
//!   The pipeline is serial (a named `Sink` reason): the rows keep the plan's order, the partition and peer order PG's
//!   WindowAgg reads.
//! - The source: the frame offsets evaluated once per open (PG's `calculate_frame_offsets` at the first row), then the
//!   stored rows in order in windows of `WINDOW` rows, row `j`'s child columns read from `m10_win_rec`.
//! - Each open of either restarts the object (`m10_win_open`, PG's ExecReScanWindowAgg; D2346): a rerun set re-runs the
//!   fill and the reader per outer row or call, and neither reads the last run's rows, positions or offsets.
//! - D3336 (road win.partruns, face/partruns.rs): a window read by claims. The serial fill stores each partition whole into
//!   the object whose turn it is (`m10_win_put_part`; the objects are fresh per execution and no rerun set holds the
//!   window, so no `OPEN_FILL`); the reader is eligible, its claims the objects, one each (`m10_win_part` at the open).
//! - The exact request (PLAN-v5 §2.5; W4, `lower::pulled`): the fill is a pulled set the object runs from its spool, one
//!   row per pull: `m10_win_put` answers 1 once the row the pull wants is stored and the end stops resumably after it
//!   (nest.cte's budget stop); the reader asks for row `cursor` (`m10_win_ask`, a pull when it is not stored) instead of
//!   reading a count at its open. Both run on the leader. A fill that streams (W10, `lower::grows`) is pulled the same
//!   way, each pull's budget doubling from one row (the object's `Rt::more`): the IR is the exact form's.
use core::cell::Cell as Once;

use gravity_decompose::ops::window::arg_cells;
use gravity_decompose::catalog::typefacts::TypeFacts;
use gravity_decompose::Source;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, ConvOp, IntBinOp, IntCond, Type, Value};
use gravity_kernels::ops::window::entries as abi;
use gravity_kernels::ops::window::raw;
use gravity_program::{ExprLife, ExprOut, LifeId, NumTypmod, Pipeline, Reason as Why, Rep, Until};
use mcx::{CollectIn, GrowIn, PgBox, PgVec};
use types_nodes::Node;

use crate::cell::{At, Cell, CellHome, CellLife, Nullable, Row};
use crate::face::end::EndMech;
use crate::face::kinds::{Face, KindId};
use crate::face::source::{SourceMech, WinConds};
use crate::face::step::ExprMech;
use crate::frame::env::Env;
use crate::frame::frame::{EndMaker, SourceMaker, Win};
use crate::frame::open::{Base, OpenWord, OpenWords};
use crate::frame::px::{ExId, Px, WordKey};
use crate::frame::row::{Carry, RowVals};
use crate::frame::stop::Exits;
use crate::lx::CellRef;
use crate::admit::refusal::Reason;
use crate::ops::Table;

use crate::ops::window::lower::{admit, desc_of, handle, host, lazy_cells, lazy_params, load_handle, pulled, raise_below_zero, shape, step_arg, window_after_args};

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::WindowAgg, &EndMaker(make_end)), (KindId::WindowAgg, &SourceMaker(make_source))], ..crate::ops::NONE };

/// The rows one reader window reads (a window's end is a cancel check).
const WINDOW: i64 = 4096;

/// An expression the lowering evaluates as a stored cell: its compile, its cell facts and type facts.
pub(crate) struct Ev {
    ex: ExId,
    out: ExprOut,
    tf: TypeFacts,
}

/// W3: an executor parameter the object's lazy cells read: an owner's, evaluated at the reader's open, or an InitPlan's,
/// handed over as its Init cell's handle word (`params::init_handle`) for the object to run at its first read.
enum LazyParam {
    Value(Ev),
    Init(u16),
}

pub(crate) fn ev<'p>(px: &mut Px<'_, 'p>, ex: &dyn ExprMech, at: Option<(usize, usize)>, e: Node<'p>, node: i32) -> Ev {
    let typ = gravity_decompose::catalog::expr_type::type_of(e).unwrap_or_else(|| ex.out_type(px.gx, e, node));
    let tf = ex.tf_of(px.gx, typ);
    let out = px.gx.out_of(e).unwrap_or(ExprOut { rep: Rep::of_storage(tf.typlen, tf.byval), typid: typ, life: ExprLife::Row });
    let id = match at {
        Some(at) => ex.compile_at(px, at, e, None, node),
        None => ex.compile(px, e, None, node),
    };
    Ev { ex: id, out, tf }
}

/// `e`'s datum word and NULL flag over `row`: the value's cell as its facts say, read as a datum (rep.rs).
pub(crate) fn datum_of(b: &mut FunctionBuilder<'_>, env: &mut Env, ex: &dyn ExprMech, node: i32, row: &RowVals, e: &Ev, key: u16) -> (Value, Value) {
    let (v, valid) = ex.emit(b, env, node, row, e.ex);
    let t = b.bconst(true);
    let null = b.int_bin(IntBinOp::Xor, valid, t);
    let cell = Cell::of_expr(e.out, key, key, Nullable::Maybe);
    let mut cx = env.lx(b, node);
    let (d, n) = crate::rep::datum(&mut cx, CellRef { cell: &cell, val: v, null: Some(null) }, &e.tf);
    env.memo = cx.memo;
    let w = b.select(n, env.zero, d);
    (w, n)
}

/// The child's columns' type facts (the stored row's first cells).
fn child_cols<'g>(px: &Px<'g, '_>, ex: &dyn ExprMech, k: usize) -> PgVec<'g, TypeFacts> {
    let (d, node) = desc_of(px, k);
    let Some(child) = gravity_decompose::ops::window::child_tlist(d.plan) else { shape(node, "a WindowAgg without its child") };
    child.iter().map(|te| {
        let e = te.as_target_entry().map_or(te, |t| t.expr);
        let typ = gravity_decompose::catalog::expr_type::type_of(e).unwrap_or_else(|| ex.out_type(px.gx, e, node));
        ex.tf_of(px.gx, typ)
    }).collect_in(px.mcx())
}

/// The reader of producer `k`'s buffer: the pipeline whose source is the slot `k` fills and whose first op is a window.
fn reader_of(px: &Px<'_, '_>, k: usize) -> usize {
    reader_opt(px, k).unwrap_or_else(|| crate::refuse!(px.gx.pipelines()[k].node_id, Reason::Bug { payload: "a window buffer without its window reader" }))
}

/// `reader_of`, `None` when pipeline `k` fills no window buffer (D3336: the window's `handle` asks, lower.rs).
pub(crate) fn reader_opt(px: &Px<'_, '_>, k: usize) -> Option<usize> {
    let ps = px.gx.pipelines();
    // D1415: the window may follow the SubPlans decompose declares ahead of it (desc_of refuses by name those no step fills)
    let reads = |q: usize| matches!(ps[q].source, Source::Breaker { slot } if px.gx.life_of(q, slot) == Some(LifeId(k as u16))) && window_after_args(&ps[q].ops).is_some();
    // material W-INNER: the reader may be a nest's or a nested SubPlan's inner (graph.rs `spool_kind`)
    (0..ps.end()).find(|&q| reads(q))
}

/// D3336: the frame word holding the object a reader's claim of a parted window reads (`i` 2, beside its cursor and
/// count), which the window step reads too.
pub(crate) fn object_word(px: &mut Px<'_, '_>, k: usize) -> i32 {
    px.frame_word(WordKey::Open { face: Face::Window, k, j: u64::MAX, i: 2 }) as i32 * 8
}

/// The producer.
struct WinEnd<'g> {
    node: i32,
    h: u16,
    ex: &'static dyn ExprMech,
    cols: PgVec<'g, TypeFacts>,
    /// Per stored argument / FILTER cell (after the child's).
    args: PgVec<'g, Arg>,
    /// W4: the fill is pulled (the exact request): it stops after the row a pull wants.
    pulled: bool,
    /// D3336: the window is read by claims: its form (the fill stores into the objects in turn).
    part: Option<crate::face::partruns::Parted>,
    /// D3336 (A1a, face/partfill.rs): the fill runs as claims over K-cut parts: the frame word of the claim's object (its
    /// source's open files it, `agg_runs_claim`), into which the claim stores every row; 0 there: the objects in turn.
    obj: Option<i32>,
    /// Construct C2: the whole-read licence of an acting argument SubPlan's fill (lower.rs `admit`, D3374). Held, not
    /// read.
    #[allow(dead_code)]
    ahead: Option<gravity_decompose::effects::Licence>,
}

/// A stored argument / FILTER cell: evaluated here, left to the object (`CELL_LAZY`, D1232) or to the step (D1415).
/// Construct C2: an evaluated cell runs at the fill, ahead of the frame reads PostgreSQL evaluates it on, its error
/// marked and raised at PostgreSQL's read: it holds the held-error licence, `None` where the argument calls user code
/// that may write (`effects::code_may_write`; the fill evaluates it all the same).
enum Arg {
    Ev(Ev, #[allow(dead_code)] Option<gravity_decompose::effects::Licence>),
    Lazy,
    Step,
}

fn make_end<'g>(px: &mut Px<'g, '_>, k: usize) -> PgBox<'g, dyn EndMech + 'g> {
    let mcx = px.mcx();
    let q = reader_of(px, k);
    let ahead = admit(px, q);
    let (d, node) = desc_of(px, q);
    let ex = crate::frame::iface::expr_face(&node).0;
    let cols = child_cols(px, ex, q);
    if cols.len() != d.natts as usize {
        crate::refuse!(node, Reason::Bug { payload: "a window buffer whose row is not the child's" });
    }
    let at = (k, px.gx.pipelines()[k].ops.len());
    let funcs = d.functions.iter().map(|f| f.node).collect_in(mcx);
    let mut args: PgVec<'_, Arg> = PgVec::new_in(mcx);
    for (f, (base, n, filter)) in d.functions.iter().zip(arg_cells(&funcs[..], cols.len())) {
        if base != cols.len() + args.len() || filter.is_some_and(|c| c != base + n) {
            crate::refuse!(node, Reason::Bug { payload: "a window argument cell out of the stored row's order" });
        }
        for (i, a) in f.func.args.iter().chain(f.func.aggfilter).enumerate() {
            let cell = if step_arg(f, i) {
                Arg::Step
            } else if d.volatile_args {
                Arg::Lazy
            } else {
                let e = ev(px, ex, Some(at), a, node);
                let writes = gravity_decompose::effects::code_may_write(px.gx.facts, a);
                Arg::Ev(e, gravity_decompose::effects::held_error(gravity_decompose::effects::Effects::RAISES, writes, None).ok())
            };
            args.push_in(cell);
        }
    }
    let evs = args.iter().filter_map(|a| if let Arg::Ev(e, _) = a { Some(&e.tf) } else { None });
    if cols.iter().chain(evs).any(|t| t.typlen == -2) {
        shape(node, "a window buffer cell of type cstring");
    }
    let h = handle(px, node, k, d);
    let part = crate::ops::window::lower::parted(px, node).filter(|p| p.fill == k);
    let obj = part.filter(|p| p.cut.is_some()).map(|_| crate::face::partfill::object_word(px, k));
    mcx::pg_box_dyn!(px.mcx(), WinEnd { node, h, ex, cols, args, pulled: pulled(px, node), part, obj, ahead } => dyn EndMech + 'g)
}

/// `m10_win_open(ctx, h, open)` at a pipeline's open (PG's ExecReScanWindowAgg, D2346): the object's walk restarts, and
/// at the fill's open (`abi::OPEN_FILL`) its stored rows go too, so a rerun set's re-open reads none of the last run's; a
/// reader's open says whether a later open walks the same rows again (`abi::OPEN_REWALK`, D3951: lifetime/store.rs
/// `rewalks`).
fn open_object(b: &mut FunctionBuilder<'_>, env: &mut Env, h: u16, open: u64) {
    let h = load_handle(b, env, h);
    let f = host(b, "m10_win_open", &[Type::Ptr, Type::I64, Type::I64], gravity_kernels::contract::entry2::<raw::WinOpen> as *const () as usize);
    let fill = b.iconst(Type::I64, open as i64);
    let st = b.call_host(f, &[env.ctx, h, fill], Some(Type::I64)).expect("m10_win_open returns its status");
    raise_below_zero(b, env, None, st);
}

impl<'g> EndMech for WinEnd<'g> {
    /// The fill starts again: the object drops the rows a last run stored (D2346). D3336: a window read by claims has
    /// fresh objects each execution and runs its fill once (no rerun set holds it): nothing to drop.
    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) {
        if self.part.is_none() {
            open_object(b, env, self.h, abi::OPEN_FILL);
        }
    }

    /// PG `spool_tuples`' `tuplestore_puttupleslot`, the row's argument cells with it.
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, _x: &Exits) -> Carry<'e> {
        let (n, nc) = (self.cols.len(), self.cols.len() + self.args.len());
        let slot = b.declare_stack_slot(abi::record_bytes(nc) as u32, 8);
        let rec = b.stack_addr(slot);
        let h = match &self.part {
            Some(p) => crate::face::partruns::current(b, env, p),
            None => load_handle(b, env, self.h),
        };
        // D3336 (A1a): a K-cut claim's own object, else the one whose turn it is
        let own = self.obj.map(|w| {
            let o = env.load_word(b, w);
            (o, b.icmp(IntCond::Ne, o, env.zero))
        });
        let h = match own {
            Some((o, set)) => b.select(set, o, h),
            None => h,
        };
        let nulls = 8 * nc as i32;
        let mut cx = env.lx(b, self.node);
        for (c, tf) in self.cols.iter().enumerate() {
            let Some(cr) = row.col(c as i16 + 1) else { crate::refuse!(self.node, Reason::Bug { payload: "a stored column past the row" }) };
            let (d, null) = crate::rep::datum(&mut cx, cr, tf);
            let w = cx.b.select(null, env.zero, d);
            cx.b.store(rec, 8 * c as i32, w);
            let nb = cx.b.conv(ConvOp::Zext, Type::I8, null);
            cx.b.store(rec, nulls + c as i32, nb);
        }
        env.memo = cx.memo;
        // D3584: the region's top before the argument cells: once the store holds their copies (or keeps a wide one to
        // evaluate again), their bytes go back, PG's per-tuple reset (a wide argument per row filled the region)
        let mark = if self.args.iter().any(|a| matches!(a, Arg::Ev(..))) { env.hir.mark(b) } else { None };
        let frame_raise = env.l_raise_row;
        let types = [Type::I64, b.value_type(row.carry.out), b.value_type(row.next)];
        for (i, a) in self.args.iter().enumerate() {
            let c = n + i;
            let a = match a {
                Arg::Ev(a, _) => a,
                Arg::Lazy | Arg::Step => {
                    b.store(rec, 8 * c as i32, env.zero);
                    let nb = b.iconst(Type::I8, if matches!(a, Arg::Lazy) { abi::CELL_LAZY as i64 } else { 1 });
                    b.store(rec, nulls + c as i32, nb);
                    continue;
                }
            };
            let memo = env.memo.set_aside();
            let (l_catch, l_join) = (b.create_block(&types), b.create_block(&[Type::I64, Type::Bool]));
            b.mark_cold(l_catch);
            env.l_raise_row = Some(l_catch);
            let (w, null) = datum_of(b, env, self.ex, self.node, row, a, 0x4000 + c as u16);
            env.l_raise_row = frame_raise;
            b.br(l_join, &[w, null]);
            // the cell's error kept by the object, raised where PG evaluates it. D2656: a host call's error (`raise::HOST`)
            // waits in its site's cell (the function-manager road of a lane nested on the leader: a pulled fill, W4 / W10),
            // so it is stashed first, as the row's raise block does (`hir.raised`), for `m10_win_argerr` to keep it
            b.switch_to(l_catch);
            let err = b.block_params(l_catch)[0];
            env.hir.raised(b);
            let f = host(b, "m10_win_argerr", &[Type::Ptr, Type::I64, Type::I64, Type::I64], gravity_abi::host_words!(abi::m10_win_argerr(ctx, h, col, err)));
            let col = b.iconst(Type::I64, c as i64);
            let st = b.call_host(f, &[env.ctx, h, col, err], Some(Type::I64)).expect("m10_win_argerr returns its status");
            raise_below_zero(b, env, Some(row), st);
            let t = b.bconst(true);
            b.br(l_join, &[env.zero, t]);
            b.switch_to(l_join);
            let p = b.block_params(l_join);
            let (w, null) = (p[0], p[1]);
            b.store(rec, 8 * c as i32, w);
            let nb = b.conv(ConvOp::Zext, Type::I8, null);
            b.store(rec, nulls + c as i32, nb);
            env.memo.restore(memo);
        }
        let put = |b: &mut FunctionBuilder<'_>| {
            let f = host(b, "m10_win_put", &[Type::Ptr, Type::I64, Type::Ptr], gravity_abi::host_words!(abi::m10_win_put(ctx, h, rec)));
            b.call_host(f, &[env.ctx, h, rec], Some(Type::I64)).expect("m10_win_put returns its status")
        };
        let st = match (&self.part, own) {
            // D3336 (A1a): the claim's object takes the row as a serial fill's one object does; else the objects in turn
            (Some(p), Some((_, set))) => {
                let (l_own, l_turn, l_put) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[Type::I64]));
                b.cond_br(set, (l_own, &[]), (l_turn, &[]));
                b.switch_to(l_own);
                let st = put(b);
                b.br(l_put, &[st]);
                b.switch_to(l_turn);
                let st = crate::face::partruns::put(b, env, p, rec);
                b.br(l_put, &[st]);
                b.switch_to(l_put);
                b.block_params(l_put)[0]
            }
            (Some(p), None) => crate::face::partruns::put(b, env, p, rec),
            (None, _) => put(b),
        };
        raise_below_zero(b, env, Some(row), st);
        if let Some(m) = mark {
            env.hir.give_back(b, m);
        }
        let out = b.int_bin(IntBinOp::Add, row.carry.out, env.one);
        let carry = Carry::new(out, env.mcx);
        if self.pulled {
            // W4: 1 = the pull's row is stored: a resumable stop after it (the next pull resumes the fill here)
            let (full, l_stop, l_on) = (b.icmp(IntCond::Ne, st, env.zero), b.create_block(&[]), b.create_block(&[]));
            b.cond_br(full, (l_stop, &[]), (l_on, &[]));
            b.switch_to(l_stop);
            let s = b.iconst(Type::I64, gravity_abi::status::suspend(false));
            env.stop(b, &carry, row.next, s);
            b.switch_to(l_on);
        }
        carry
    }

    /// One worker fills the buffer, in the plan's order. D3336 (A1a): a K-cut fill's claims each fill their own object.
    fn pipeline_desc(&self, _px: &Px<'_, '_>, _k: usize, d: Pipeline) -> Pipeline {
        if self.obj.is_some() {
            return d;
        }
        match d.is_eligible() {
            true => Pipeline { parallel: gravity_program::Parallel::Ineligible { site: d.driving_site, reason: Why::Sink }, ..d },
            false => d,
        }
    }
}

/// The reader.
struct WinRows<'g> {
    node: i32,
    h: u16,
    ex: &'static dyn ExprMech,
    cols: PgVec<'g, TypeFacts>,
    ncols: usize,
    /// The frame's start / end offsets, when the frame has them.
    offsets: [Option<Ev>; 2],
    /// W3 (window_core-0190 / -0191): the executor parameters the object's lazy cells read (`lazy_params`), by paramid.
    params: PgVec<'g, (i32, LazyParam)>,
    w_cursor: i32,
    w_total: i32,
    handle: Once<Option<Value>>,
    /// W4: the fill is pulled: the reader asks for each row past the stored ones (`m10_win_ask`).
    pulled: bool,
    /// D3951: a later open walks the same stored rows again (lifetime/store.rs `rewalks`): the walk frees no partition.
    rewalk: bool,
    /// D3336: the window is read by claims: its form and the frame word of this claim's object (`object_word`).
    part: Option<(crate::face::partruns::Parted, i32)>,
    /// Construct C2: the whole-read licence of an acting argument SubPlan's fill (lower.rs `admit`, D3374). Held, not
    /// read.
    #[allow(dead_code)]
    ahead: Option<gravity_decompose::effects::Licence>,
}

fn make_source<'g>(px: &mut Px<'g, '_>, k: usize) -> PgBox<'g, dyn SourceMech + 'g> {
    let mcx = px.mcx();
    let ahead = admit(px, k);
    let (d, node) = desc_of(px, k);
    let ex = crate::frame::iface::expr_face(&node).0;
    let cols = child_cols(px, ex, k);
    let funcs = d.functions.iter().map(|f| f.node).collect_in(mcx);
    let ncols = arg_cells(&funcs[..], cols.len()).last().map_or(cols.len(), |&(base, n, f)| base + n + f.is_some() as usize);
    let mut offsets = [None, None];
    for (i, (flag, e)) in [(types_nodes::rawnodes::FRAMEOPTION_START_OFFSET, d.plan.startOffset), (types_nodes::rawnodes::FRAMEOPTION_END_OFFSET, d.plan.endOffset)].into_iter().enumerate() {
        if d.plan.frameOptions & flag == 0 {
            continue;
        }
        let Some(e) = e else { crate::refuse!(node, Reason::Bug { payload: "a frame offset without its expression" }) };
        offsets[i] = Some(ev(px, ex, None, e, node));
    }
    let lazy = lazy_cells(px.gx, d);
    let params = match lazy.contains(&true) {
        true => lazy_params(px.gx, d, &lazy).unwrap_or_else(|why| shape(node, why)).into_iter().map(|(id, e, init)| (id, if init { LazyParam::Init(crate::rerun::params::init_handle(px, id)) } else { LazyParam::Value(ev(px, ex, None, e, node)) })).collect_in(mcx),
        false => PgVec::new_in(mcx),
    };
    let word = |px: &mut Px<'_, '_>, i: u8| px.frame_word(WordKey::Open { face: Face::Window, k, j: u64::MAX, i }) as i32 * 8;
    let (w_cursor, w_total) = (word(px, 0), word(px, 1));
    let h = handle(px, node, k, d);
    let rewalk = crate::lifetime::store::rewalks(px, k);
    let part = crate::ops::window::lower::parted(px, node).filter(|p| p.reader == k).map(|p| (p, object_word(px, k)));
    mcx::pg_box_dyn!(px.mcx(), WinRows { node, h, ex, cols, ncols, offsets, params, w_cursor, w_total, handle: Once::new(None), pulled: pulled(px, node), rewalk, part, ahead } => dyn SourceMech + 'g)
}

impl<'g> SourceMech for WinRows<'g> {
    fn open_words(&self, w: &mut OpenWords) {
        w.declare(OpenWord::open("window cursor", Base::Words, self.w_cursor, Type::I64, 0));
        w.declare(OpenWord::open("window rows", Base::Words, self.w_total, Type::I64, 0));
        if let Some((_, at)) = self.part {
            w.declare(OpenWord::open("window object", Base::Words, at, Type::I64, 0));
        }
    }

    /// The object's walk restarted (D2346), PG `calculate_frame_offsets` (NULL / negative checked by the object), the lazy
    /// cells' executor parameters (W3), then the stored rows' count.
    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) -> Value {
        if let Some((p, at)) = &self.part {
            return self.open_part(b, env, p, *at);
        }
        open_object(b, env, self.h, if self.rewalk { abi::OPEN_REWALK } else { abi::OPEN_READER });
        let h = load_handle(b, env, self.h);
        let empty = RowVals { row: Row::new_in(env.mcx), vals: PgVec::new_in(env.mcx), cols: PgVec::new_in(env.mcx), carry: Carry::new(env.zero, env.mcx), next: env.zero, at: env.zero };
        if !self.params.is_empty() {
            // one record per parameter: paramid, datum, NULL, `typlen | byval << 16` (the object copies a by-reference value)
            let slot = b.declare_stack_slot(32 * self.params.len() as u32, 8);
            let vals = b.stack_addr(slot);
            for (i, (id, p)) in self.params.iter().enumerate() {
                let (w, nw, layout) = match p {
                    LazyParam::Value(e) => {
                        let (w, null) = datum_of(b, env, self.ex, self.node, &empty, e, 0x7100 + i as u16);
                        (w, b.conv(ConvOp::Zext, Type::I64, null), b.iconst(Type::I64, (e.tf.typlen as u16 as i64) | (e.tf.byval as i64) << 16))
                    }
                    // the Init cell, unread: the object runs it at the parameter's first read
                    LazyParam::Init(h) => (b.load(Type::I64, env.params, *h as i32 * 8), env.zero, b.iconst(Type::I64, abi::PARAM_INIT as i64)),
                };
                let id = b.iconst(Type::I64, *id as i64);
                for (k, v) in [id, w, nw, layout].into_iter().enumerate() {
                    b.store(vals, 32 * i as i32 + 8 * k as i32, v);
                }
            }
            let f = host(b, "m10_win_params", &[Type::Ptr, Type::I64, Type::Ptr, Type::I64], gravity_kernels::contract::entry3::<raw::WinParams> as *const () as usize);
            let n = b.iconst(Type::I64, self.params.len() as i64);
            let st = b.call_host(f, &[env.ctx, h, vals, n], Some(Type::I64)).expect("m10_win_params returns its status");
            raise_below_zero(b, env, None, st);
        }
        if self.offsets.iter().any(Option::is_some) {
            let slot = b.declare_stack_slot(32, 8);
            let vals = b.stack_addr(slot);
            for (i, o) in self.offsets.iter().enumerate() {
                let (w, null) = match o {
                    Some(e) => datum_of(b, env, self.ex, self.node, &empty, e, 0x7000 + i as u16),
                    None => (env.zero, b.bconst(false)),
                };
                let nw = b.conv(ConvOp::Zext, Type::I64, null);
                b.store(vals, 16 * i as i32, w);
                b.store(vals, 16 * i as i32 + 8, nw);
            }
            let f = host(b, "m10_win_offsets", &[Type::Ptr, Type::I64, Type::Ptr], gravity_abi::host_words!(abi::m10_win_offsets(ctx, h, vals)));
            let st = b.call_host(f, &[env.ctx, h, vals], Some(Type::I64)).expect("m10_win_offsets returns its status");
            raise_below_zero(b, env, None, st);
        }
        if self.pulled {
            // W4: nothing stored yet (the fill runs at the first ask); the window's count is the stored rows'
            env.store_word(b, self.w_total, env.zero);
            return env.zero;
        }
        let f = host(b, "m10_win_rows", &[Type::Ptr, Type::I64], gravity_abi::host_words!(abi::m10_win_rows(ctx, h)));
        let total = b.call_host(f, &[env.ctx, h], Some(Type::I64)).expect("m10_win_rows returns its status");
        let bad = b.icmp(IntCond::Slt, total, env.zero);
        let kept = b.select(bad, env.zero, total);
        env.store_word(b, self.w_total, kept);
        b.select(bad, total, env.zero)
    }

    fn next_window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l_eof: Block) -> Win {
        if self.pulled {
            return self.ask_window(b, env, l_eof);
        }
        let (cursor, total) = (env.load_word(b, self.w_cursor), env.load_word(b, self.w_total));
        let (left, l_win) = (b.icmp(IntCond::Slt, cursor, total), b.create_block(&[]));
        b.cond_br(left, (l_win, &[]), (l_eof, &[env.zero]));
        b.switch_to(l_win);
        self.handle.set(Some(match self.part {
            Some((_, at)) => env.load_word(b, at),
            None => load_handle(b, env, self.h),
        }));
        let far = { let w = b.iconst(Type::I64, WINDOW); b.int_bin(IntBinOp::Add, cursor, w) };
        let end = { let past = b.icmp(IntCond::Sgt, far, total); b.select(past, total, far) };
        Win { cursor, end, facts: WinConds::default() }
    }

    /// Stored row `j`: its child columns, each a datum word and a NULL byte (by reference: the object's copy).
    fn row<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, _win: &Win, j: Value, carry: Carry<'e>, next: Value) -> RowVals<'e> {
        let h = self.handle.get().unwrap_or_else(|| crate::refuse!(self.node, Reason::Bug { payload: "a window row before its window" }));
        let f = host(b, "m10_win_rec", &[Type::Ptr, Type::I64, Type::I64], gravity_abi::host_words!(abi::m10_win_rec(ctx, h, j)));
        let r = b.call_host(f, &[env.ctx, h, j], Some(Type::I64)).expect("m10_win_rec returns the row");
        let (bad, l_bad, l_row) = (b.icmp(IntCond::Slt, r, env.zero), b.create_block(&[]), b.create_block(&[]));
        b.cond_br(bad, (l_bad, &[]), (l_row, &[]));
        b.switch_to(l_bad);
        env.stop(b, &carry, j, r);
        b.switch_to(l_row);
        let p = b.conv(ConvOp::IntToPtr, Type::Ptr, r);
        let n = self.cols.len();
        let mut row = RowVals { row: Row { cells: mcx::reserved_in(n, env.mcx) }, vals: mcx::reserved_in(n, env.mcx), cols: (0..n as u16).map(Some).collect_in(env.mcx), carry, next, at: j };
        for (c, tf) in self.cols.iter().enumerate() {
            let life = CellLife { until: Until::Row, home: CellHome::FrameSlot };
            row.row.cells.push_in(Cell { rep: Rep::of_storage(tf.typlen, tf.byval), typid: tf.typid, num: NumTypmod::NONE, life, at: At::Slot(c as u16), null: Nullable::Maybe });
            let v = b.load(Type::I64, p, 8 * c as i32);
            let nb = b.load_zext(Type::I8, p, 8 * self.ncols as i32 + c as i32);
            let null = b.icmp(IntCond::Ne, nb, env.zero);
            row.vals.push_in((v, Some(null)));
        }
        row
    }

    fn write_back(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, cur: Value, _out: Value) {
        env.store_word(b, self.w_cursor, cur);
    }

    /// D3336: a window read by claims is eligible, its driving site nominal (its claims are the objects).
    fn nominal_site(&self, _px: &Px<'_, '_>) -> Option<usize> {
        self.part.map(|(p, _)| p.site)
    }

    /// W4: on the leader (a pull runs the fill there). D3336: a window read by claims runs wide over its objects, unless a
    /// call a pool thread cannot run keeps it on the leader (`hir::hosted_serial`, as a scan's; its row sort or top then
    /// seals its own run: table/emit_group.rs `kept_on_leader` reads the same calls), where it reads its one object.
    fn pipeline_desc(&self, px: &Px<'_, '_>, k: usize) -> Pipeline {
        let d = px.gx.pipelines()[k].desc;
        if let Some((p, _)) = self.part {
            let parallel = match crate::hir::hosted_serial(px, k) {
                Some(reason) => gravity_program::Parallel::Ineligible { site: p.site as i16, reason },
                None => gravity_program::Parallel::Eligible,
            };
            return Pipeline { parallel, driving_site: p.site as i16, ..d };
        }
        match self.pulled && d.is_eligible() {
            true => Pipeline { parallel: gravity_program::Parallel::Ineligible { site: d.driving_site, reason: Why::Host }, ..d },
            false => d,
        }
    }
}

impl<'g> WinRows<'g> {
    /// D3336: a claim's open over a window read by claims: its object (none: no row, no object call), restarted for the
    /// reader (no rerun set holds it: never a rewalk), then its stored rows' count. No lazy parameter and no frame offset
    /// (partruns.rs `decide`).
    fn open_part(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, p: &crate::face::partruns::Parted, at: i32) -> Value {
        let h = crate::face::partruns::object(b, env, p);
        env.store_word(b, at, h);
        let (none, l_some, l_out) = (b.icmp(IntCond::Eq, h, env.zero), b.create_block(&[]), b.create_block(&[Type::I64]));
        b.cond_br(none, (l_out, &[env.zero]), (l_some, &[]));
        b.switch_to(l_some);
        let f = host(b, "m10_win_open", &[Type::Ptr, Type::I64, Type::I64], gravity_kernels::contract::entry2::<raw::WinOpen> as *const () as usize);
        let reader = b.iconst(Type::I64, abi::OPEN_READER as i64);
        let st = b.call_host(f, &[env.ctx, h, reader], Some(Type::I64)).expect("m10_win_open returns its status");
        raise_below_zero(b, env, None, st);
        let f = host(b, "m10_win_rows", &[Type::Ptr, Type::I64], gravity_abi::host_words!(abi::m10_win_rows(ctx, h)));
        let total = b.call_host(f, &[env.ctx, h], Some(Type::I64)).expect("m10_win_rows returns its status");
        b.br(l_out, &[total]);
        b.switch_to(l_out);
        let total = b.block_params(l_out)[0];
        let bad = b.icmp(IntCond::Slt, total, env.zero);
        let kept = b.select(bad, env.zero, total);
        env.store_word(b, self.w_total, kept);
        b.select(bad, total, env.zero)
    }

    /// W4 (PG ExecWindowAgg's `spool_tuples(winstate, currentpos)` at each row): the stored rows from the cursor, else
    /// `m10_win_ask(h, cursor)`: the fill pulled one row per pull until row `cursor` is stored (the stored count) or it
    /// ended (0: the end); below zero, the pull's error (stashed by the host), raised inside the reader's read.
    fn ask_window(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, l_eof: Block) -> Win {
        let (cursor, total) = (env.load_word(b, self.w_cursor), env.load_word(b, self.w_total));
        let (left, l_win, l_ask) = (b.icmp(IntCond::Slt, cursor, total), b.create_block(&[Type::I64]), b.create_block(&[]));
        b.cond_br(left, (l_win, &[total]), (l_ask, &[]));
        b.switch_to(l_ask);
        let h = load_handle(b, env, self.h);
        let f = host(b, "m10_win_ask", &[Type::Ptr, Type::I64, Type::I64], gravity_kernels::contract::entry2::<raw::WinAsk> as *const () as usize);
        let n = b.call_host(f, &[env.ctx, h, cursor], Some(Type::I64)).expect("m10_win_ask returns its status");
        raise_below_zero(b, env, None, n);
        let (ended, l_got) = (b.icmp(IntCond::Eq, n, env.zero), b.create_block(&[]));
        b.cond_br(ended, (l_eof, &[env.zero]), (l_got, &[]));
        b.switch_to(l_got);
        env.store_word(b, self.w_total, n);
        b.br(l_win, &[n]);
        b.switch_to(l_win);
        let total = b.block_params(l_win)[0];
        self.handle.set(Some(load_handle(b, env, self.h)));
        let far = { let w = b.iconst(Type::I64, WINDOW); b.int_bin(IntBinOp::Add, cursor, w) };
        let end = { let past = b.icmp(IntCond::Sgt, far, total); b.select(past, total, far) };
        Win { cursor, end, facts: WinConds::default() }
    }
}
