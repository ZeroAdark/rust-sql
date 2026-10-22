//! A run in a slot read by row index from another kind's step, and the Materialize producer that fills one in arrival
//! order (`SlotRunImpl`, face/runs.rs; M11 merge.join's inner, V4R pending). Declared in registry/m3.d (runs.sort's tree:
//! the run's Shape, its end and its cells are runs/sort.rs's). Owner M11-MERGE (files-extra). Budget 300 lines (D1521's
//! NULL fill; jmerge-0012's pulled run).
//!
//! - The end: runs.sort's `RunEnd` with no keys (kernels `sort_rows` keeps a keyless run's rows in their order), or a Sort's
//!   keys when the inner is a Sort (graph.rs end_need), no road form, its pipeline serial (a named `Sink` reason), so the
//!   last run is sealed where it lies: in the plan's order (PG's Material keeps it too), or sorted. D3105 (b), the put road
//!   (face/slot_window.rs): a Sort's or a Material's run grows only within its owner's limit and is put past it, as
//!   runs.sort puts a run (a keyless run's rows in their order: PG's Material spills its tuplestore past work_mem); a union
//!   arm's run full grows resident (`m11_run_grow`: D1235), never put. D3190 (road merge.ksort, `wide_fill`): a Sort that
//!   may not act fills wide, runs.sort's own end on every worker and its merge protocol at the close (the one sealed run,
//!   or the workers' put runs read through the window).
//! - The reader: `m11_run_view` over the consumed region at each window (`spool_view`'s sealed run, its by-reference cells
//!   pointed into it once; the empty run when no row came), then row `j` at `base + SORTRUN_HDR + j * row_bytes` with no
//!   host call, its cells as runs.sort's emit reads them. On the put road `m11_run_open` answers that run when nothing was
//!   put, else a window over the runs merged into one, read by the same index (face/slot_window.rs).
//! - The pulled run (jmerge-0012, the exact request's merge inner: `pulled_end` / `pulled_reader`): the end points each
//!   row's by-reference cells as it stores it (kernels sort raw.rs `SpoolPoint`; every row again after a growth) and stops
//!   resumably once the run holds the merge's want (one row past the stored ones under the exact request; MJ-AHEAD's
//!   doubling budget for a calm inner, ops/mergejoin/exact_inner.rs); the reader's open takes the run as stored so far
//!   (`SpoolLive`: sealed or filling, no put) and keeps its base in a frame word, so an open after each pull re-reads a run
//!   its growth moved.
use core::cell::Cell as Once;

use gravity_abi::breaker::{BreakerKind, SORTRUN_HDR};
use gravity_abi::status::raise;
use gravity_decompose::ops::shared::sort::SortForm;
use gravity_decompose::Op;
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{ConvOp, IntBinOp, IntCond, Type, Value};
use gravity_kernels::ops::mergejoin::entries as h11;
use gravity_kernels::ops::sort::entries as h3;
use gravity_program::spill::NoSpill;
use gravity_program::{ExprLife, ExprOut, Pipeline, Rep};
use mcx::{CollectIn, GrowIn, PgBox, PgVec};
use types_nodes::list::NodeList;

use crate::face::end::EndMech;
use crate::face::kinds::{Face, KindId};
use crate::face::runs::{RunReader, SlotRun, SlotRunImpl, SlotRunMech};
use crate::frame::env::Env;
use crate::frame::iface;
use crate::frame::open::OpenWords;
use crate::frame::px::{runs_word, Px, WordKey};
use crate::frame::row::{Carry, RowVals};
use crate::frame::stop::Exits;
use crate::admit::refusal::{Reason, ShapeWhy};
use crate::ops::Table;

use crate::ops::sort::strategies::resident as res;
use crate::ops::sort::lower::{sort_of, Arm, RunEmit, RunEnd};
use crate::face::runs::Shape;

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::RunsSort, &SlotRunImpl(&SLOT))], ..crate::ops::NONE };

static SLOT: Slot = Slot;
struct Slot;

fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

/// Pipeline `k`'s last projection: the row a keyless run holds.
fn cols<'g, 'p>(px: &Px<'g, 'p>, k: usize) -> &'g NodeList<'p> {
    let p = &px.gx.pipelines()[k];
    p.ops.iter().rev().find_map(|op| if let Op::Project(l) = op { Some(*l) } else { None }).unwrap_or_else(|| shape(p.node_id, "a spooled merge inner with no projection"))
}

/// The run a merge inner reads: its producer is a Sort or a Material (`shape_of` refuses any other by Shape), so no
/// other run can be named.
#[derive(Clone, Copy)]
enum RunKind {
    /// A Sort's, sealed sorted by its keys.
    Sort,
    /// A Material's, keyless, in the plan's order.
    Material,
}

impl RunKind {
    /// D3105 (b): producer `k`'s run (PG plans a Material over the inner Sort once that Sort passes work_mem) is put past
    /// its owner's limit and read merged through a window (face/slot_window.rs), never grown past the grant; but a union
    /// arm's, whose run carries its table copy (D2160) and grows resident (`m11_run_grow`: D1235).
    fn put_road(self, px: &Px<'_, '_>, k: usize) -> bool {
        match self {
            RunKind::Sort | RunKind::Material => crate::ops::sort::lower::union_arm(&px.gx.pipelines()[k]).is_none(),
        }
    }
}

/// Producer `q`'s run and its row shape: a full sort's (runs.sort's own form only) or a keyless spool's.
fn shape_of(px: &Px<'_, '_>, q: usize) -> (RunKind, Shape) {
    let p = &px.gx.pipelines()[q];
    match iface::end_breaker(px.gx, q) {
        Some(BreakerKind::Sort) => {
            let (sd, cols) = sort_of(px, q);
            if !matches!(sd.form, SortForm::Runs) || sd.presorted > 0 || !iface::run_holds_rows(px.gx, q) {
                shape(p.node_id, "a merge inner sorted by a top, an incremental or a group sort (not one sealed run of its rows)");
            }
            (RunKind::Sort, Shape::plan(px, p.node_id, &sd.keys, Some(sd.tlist), cols))
        }
        Some(BreakerKind::Materialize) => (RunKind::Material, Shape::plan(px, p.node_id, &[], None, cols(px, q))),
        _ => shape(p.node_id, "a merge inner that is neither a sort's run nor a spool"),
    }
}

/// `pulled_end`'s name for a Sort's run asked to fill as read (PG's Sort reads its input whole before its first row).
const PULLED_SORT: &str = "a pulled merge inner that is a Sort's run (its input read whole before its first row: no row-at-a-time fill)";

/// `pulled_end`'s name for a union arm's run asked to fill as read (its arms' runs share the one slot).
const PULLED_UNION: &str = "a pulled merge inner that is a union arm's run (its arms fill one slot)";

/// Producer `k`'s end, its rows filled as read (`pulled`) or whole.
fn make_end<'g>(px: &mut Px<'g, '_>, k: usize, pulled: bool) -> PgBox<'g, dyn EndMech + 'g> {
    let resident = crate::ops::sort::lower::union_arm(&px.gx.pipelines()[k]).is_some(); // D2160: a union arm's run carries its table copy
    // D3336 (road merge.wideemit): over a wide group emit (`emit_fill`, `Px::wide`) each claim thread fills and seals its own run
    let emit = !pulled && px.wide.contains(&k);
    let wide = !pulled && (wide_fill(px, k) || emit);
    let (arm, (run, sh)) = (Arm { keylane: false, natural: false, resident: resident || wide, finish: emit }, shape_of(px, k));
    let w_run = px.frame_word(WordKey::Open { face: Face::Runs, k, j: u64::MAX, i: runs_word::END_RUN }) as i32 * 8; // RunEnd's own
    let mut end = match run {
        // a Sort a merge join reads (graph.rs end_need): runs.sort's run over the sort's keys, sealed sorted at the end
        RunKind::Sort => {
            let (sd, cols) = sort_of(px, k);
            RunEnd::over(px, k, &sd.keys, Some(sd.tlist), cols, arm)
        }
        RunKind::Material => RunEnd::over(px, k, &[], None, cols(px, k), arm),
    };
    // ... but one the leader keeps (`kept_on_leader`) runs site-less in one call: filled and sealed serially, no merge protocol
    let emit = emit && !crate::table::emit_group::kept_on_leader(px, k);
    if !emit && px.wide.contains(&k) {
        end.kept_serial(px, k, resident);
    }
    let wide = wide && (emit || wide_fill(px, k));
    // D3190 (road merge.ksort): the sort's merge protocol lays the workers' runs as one sealed run at the close
    let wide = match (wide, iface::breaker_of(&px.gx.pipelines()[k].desc)) {
        (true, Some(br)) if end.resident => {
            res::declare(px, k, br.slot, (!end.shape.var()).then_some((end.shape.spec, 2 * end.shape.keys.len() as u16)));
            fires::fire("lower.merge.ksort"); // shapecheck fire signal (D3190): a merge join's inner Sort filled wide
            if emit {
                fires::fire("lower.merge.wideemit"); // shapecheck fire signal (D3336): ... by a group emit's claims
            }
            true
        }
        _ => false,
    };
    let outs = absent_outs(px, k);
    // a pulled end stops at its reader's want word (face/pull.rs: a merge join's, MJ-AHEAD, or a streamed set operation's)
    let want = pulled.then(|| crate::face::pull::want_word(px, k).unwrap_or_else(|| shape(px.gx.pipelines()[k].node_id, "a pulled run with no step pulling it")));
    let put = !pulled && !wide && run.put_road(px, k);
    // C10: a run neither put nor laid wide grows resident past its grant (`m11_run_grow`): a pulled fill's, else a union
    // arm's (`put_road` puts every other run of either kind)
    let grows = if put || wide {
        None
    } else if pulled {
        Some(NoSpill::MergeRunPulled)
    } else {
        Some(NoSpill::UnionArmRun)
    };
    mcx::pg_box_dyn!(px.mcx(), Spool(end, w_run, sh, outs, want, put, wide, grows) => dyn EndMech + 'g)
}

/// D3336 (road merge.wideemit): merge inner Sort `k` over a group emit takes the emit's claims (`Emit::new` asks), each filling
/// a run merged at the close as `wide_fill`'s, under `wide_fill`'s exclusions but the section end (the emit's nominal site's).
pub(crate) fn emit_fill(px: &Px<'_, '_>, k: usize) -> bool {
    let p = &px.gx.pipelines()[k];
    px.gx.end_need(k) == KindId::MergeJoin
        && iface::end_breaker(px.gx, k) == Some(BreakerKind::Sort)
        && p.ops.iter().any(|op| matches!(op, Op::Sort(sd) if matches!(sd.form, SortForm::Runs) && sd.presorted == 0))
        && iface::run_holds_rows(px.gx, k)
        && crate::ops::sort::lower::union_arm(p).is_none()
        && !p.desc.finish
        && !crate::rerun::sets::in_owner_set(px.gx, k)
        && !crate::effects::visible::merge::inner_may_act(px.gx, k, usize::MAX, None)
        && res::on(px, crate::face::strategy::MERGE_WIDEEMIT)
        && res::on(px, crate::face::strategy::MERGE_KSORT)
        && res::on(px, crate::face::strategy::SORT_RESIDENT)
}

/// D3190 (road merge.ksort, ops/mergejoin/strategies/ksort.rs): producer `k`, a Sort a merge join reads, fills wide: runs.sort's
/// end on every worker (sort.resident's open: each share sealed in place while every share fits, else put) and the sort's
/// merge protocol at the close, so the slot's region holds the one sealed run the reader reads by position, or the workers'
/// put runs it reads merged through the window. Not a pulled run (it stops as read), a union arm's (its arms fill one
/// slot), a pipeline with no section end (its last run is `finish`'s), a rerun set's member (each rerun would merge
/// again over its window) or an inner that may act (D1236: PG runs it only once an outer row needs it; merge.join's
/// lazy group or its run-ahead licence keeps it on one worker).
fn wide_fill(px: &Px<'_, '_>, k: usize) -> bool {
    let p = &px.gx.pipelines()[k];
    iface::end_breaker(px.gx, k) == Some(BreakerKind::Sort)
        && crate::ops::sort::lower::union_arm(p).is_none()
        && p.desc.finish
        && !crate::rerun::sets::in_owner_set(px.gx, k)
        && !crate::effects::visible::merge::inner_may_act(px.gx, k, usize::MAX, None)
        && res::on(px, crate::face::strategy::MERGE_KSORT)
        && res::on(px, crate::face::strategy::SORT_RESIDENT)
}

impl SlotRunMech for Slot {
    fn end<'g>(&self, px: &mut Px<'g, '_>, k: usize) -> PgBox<'g, dyn EndMech + 'g> {
        make_end(px, k, false)
    }

    fn pulled_end<'g>(&self, px: &mut Px<'g, '_>, k: usize) -> PgBox<'g, dyn EndMech + 'g> {
        let p = &px.gx.pipelines()[k];
        if iface::end_breaker(px.gx, k) == Some(BreakerKind::Sort) {
            shape(p.node_id, PULLED_SORT);
        }
        if crate::ops::sort::lower::union_arm(p).is_some() {
            shape(p.node_id, PULLED_UNION);
        }
        make_end(px, k, true)
    }

    fn pulled_reader<'g>(&self, px: &mut Px<'g, '_>, k: usize, slot: u8) -> PgBox<'g, dyn SlotRun + 'g> {
        let node = px.gx.pipelines()[k].node_id;
        let (Some(life), Some(i)) = (px.gx.life_of(k, slot), px.consumed(k, slot)) else {
            crate::refuse!(node, Reason::Bug { payload: "a pulled slot run reader of a slot its pipeline does not consume" })
        };
        let q = life.0 as usize; // its end is `pulled_end`'s (a Sort's run refused there)
        let emit = RunEmit::over(px, k, shape_of(px, q).1);
        let w_base = px.frame_word(WordKey::Open { face: Face::Runs, k, j: crate::frame::px::key_j(usize::MAX - 1 - slot as usize), i: runs_word::SLOT_BASE }) as i32 * 8;
        let lean = crate::ops::mergejoin::exact_inner::lean_view(px, q);
        let sh = shape_of(px, q).1;
        mcx::pg_box_dyn!(px.mcx(), Pulled { sh, emit, i: i as u64, w_base, lean } => dyn SlotRun + 'g)
    }

    fn reader<'g>(&self, px: &mut Px<'g, '_>, k: usize, slot: u8) -> PgBox<'g, dyn SlotRun + 'g> {
        let node = px.gx.pipelines()[k].node_id;
        let (Some(life), Some(i)) = (px.gx.life_of(k, slot), px.consumed(k, slot)) else {
            crate::refuse!(node, Reason::Bug { payload: "a slot run reader of a slot its pipeline does not consume" })
        };
        // the emit's cell mapping over the same shape (its words are the emit's own and stay unread here)
        let (run, emit_sh) = shape_of(px, life.0 as usize);
        let emit = RunEmit::over(px, k, emit_sh);
        let lean = crate::ops::mergejoin::exact_inner::lean_view(px, life.0 as usize);
        let put = run.put_road(px, life.0 as usize);
        let cosplit = crate::face::cosplit::of(px, k); // D3190: a co-split claim reads its slice of the run
        let sh = shape_of(px, life.0 as usize).1;
        crate::ops::sort::sort_noproc::wrap(px.gx, life.0 as usize, mcx::pg_box_dyn!(px.mcx(), Reader { sh, emit, i: i as u64, slot, base: Once::new(None), lean, put, opened: Once::new(None), cosplit } => dyn SlotRun + 'g))
    }
}

/// The keyless producer: runs.sort's end (its run word, its row shape), its pipeline serial, a full run grown resident;
/// pulled (jmerge-0012): each row pointed as stored and a resumable stop once the run holds the merge's want (its word);
/// a Sort's or a Material's (`RunKind::put_road`, D3105 (b)): grown within its owner's limit, else put; a wide Sort's
/// (`wide_fill`, D3190): runs.sort's own end on every worker, a full share put.
struct Spool<'g>(RunEnd, i32, Shape, PgVec<'g, ExprOut>, Option<i32>, bool, bool, Option<NoSpill>);

/// D1521: each run column's cell facts, for a column the pipeline never loaded. A Materialize keeps its scan's physical
/// target list, so a column no reader of the run reads has no lane (the site loads what is read): the run holds it NULL.
fn absent_outs<'g>(px: &Px<'g, '_>, k: usize) -> PgVec<'g, ExprOut> {
    let cols = match iface::end_breaker(px.gx, k) {
        Some(BreakerKind::Sort) => sort_of(px, k).1,
        _ => cols(px, k),
    };
    cols.iter().filter_map(|n| n.as_target_entry()).map(|te| {
        let typ = gravity_decompose::catalog::expr_type::type_of(te.expr).unwrap_or_else(|| gravity_decompose::catalog::expr_type::result_type(te.expr));
        let tf = gravity_decompose::catalog::typefacts::facts_of(px.gx.facts, typ).unwrap_or_else(|| shape(px.gx.pipelines()[k].node_id, "a spooled column whose type the catalog does not know"));
        ExprOut { rep: gravity_program::OutCol::new("", typ, -1, tf.typlen, tf.byval).rep, typid: typ, life: ExprLife::Row }
    }).collect_in(px.mcx())
}

/// `row` with each run column it lacks as a NULL cell of its own (D1521: never read, so any word stands for it).
fn with_absent<'r>(b: &mut FunctionBuilder<'_>, row: &RowVals<'r>, outs: &[ExprOut]) -> Option<RowVals<'r>> {
    if !(0..outs.len()).any(|c| row.col(c as i16 + 1).is_none()) {
        return None;
    }
    let (mut cells, mut vals, mut cols) = (row.row.cells.clone(), row.vals.clone(), row.cols.clone());
    cols.resize_in(outs.len().max(cols.len()), None);
    let t = b.bconst(true);
    for c in (0..outs.len()).filter(|&c| row.col(c as i16 + 1).is_none()) {
        let ty = if matches!(outs[c].rep, Rep::Word { .. }) { Type::I64 } else { Type::Ptr };
        cols[c] = Some(cells.len() as u16);
        cells.push_in(crate::cell::Cell::of_expr(outs[c], 0, 0, crate::cell::Nullable::Maybe));
        vals.push_in((b.const_raw(ty, 0), Some(t)));
    }
    Some(RowVals { row: crate::cell::Row { cells }, vals, cols, carry: row.carry.copy_in(*row.vals.allocator()), next: row.next, at: row.at })
}

impl<'g> EndMech for Spool<'g> {
    fn open_words(&self, w: &mut OpenWords) {
        self.0.open_words(w)
    }
    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) {
        self.0.open(b, env)
    }
    /// D1235: a full run off the put road (a union arm's) is never put (a merge rewinds to its mark by row index); it grows
    /// resident (`m11_run_grow`, the driver's D254 region_grow) and the row is written again at the new base. D3105 (b): a
    /// Sort's or a Material's run grows only within its owner's limit, else it is put (face/slot_window.rs `full`), the
    /// merge reading the runs merged.
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, _x: &Exits) -> Carry<'e> {
        let filled = with_absent(b, row, &self.3[..]);
        let pulled = self.4.is_some();
        let carry = self.0.append(b, env, filled.as_ref().unwrap_or(row), &mut |b, env, _| {
            if self.6 {
                return self.0.put(b, env, false);
            }
            if self.5 {
                let tbl = self.2.table(b);
                return crate::face::slot_window::full(b, env, &self.0, self.1, tbl);
            }
            let (run, tbl) = (self.0.run(b, env), self.2.table(b));
            let h = res::host(b, "m11_run_grow", &[Type::Ptr, Type::Ptr, Type::Ptr], Type::I64, gravity_abi::host_words!(h11::m11_run_grow(ctx, run, tbl)));
            let nb = b.call_host(h, &[env.ctx, run, tbl], Some(Type::I64)).expect("m11_run_grow returns the base");
            let (l_ok, l_bad, l_oom) = (b.create_block(&[]), b.create_block(&[]), b.create_block(&[]));
            let grown = b.icmp(IntCond::Sgt, nb, env.zero);
            b.cond_br(grown, (l_ok, &[]), (l_bad, &[]));
            b.switch_to(l_bad);
            let oom = b.icmp(IntCond::Eq, nb, env.zero);
            b.cond_br(oom, (l_oom, &[]), (env.x.l_exit, &[nb]));
            b.switch_to(l_oom);
            let e = b.iconst(Type::I64, raise::bit(raise::HOST) as i64);
            b.br(env.l_raise, &[e]);
            b.switch_to(l_ok);
            env.store_word(b, self.1, nb);
            if pulled {
                // the rows and the var copies moved: every stored row pointed again where it lies now
                let (run, tbl, from) = (b.conv(ConvOp::IntToPtr, Type::Ptr, nb), self.2.table(b), env.zero);
                point(b, env, run, tbl, from);
            }
        });
        if let Some(w_want) = self.4 {
            // jmerge-0012: the row just stored pointed (its reader reads it before any seal), then a resumable stop once the
            // run holds the merge's want: the pull that resumed the fill has its rows (MJ-AHEAD: a budget's)
            let (run, tbl) = (self.0.run(b, env), self.2.table(b));
            let len = b.load_zext(Type::I32, run, 4);
            let last = b.int_bin(IntBinOp::Sub, len, env.one);
            point(b, env, run, tbl, last);
            let want = env.load_word(b, w_want);
            let (l_stop, l_on, full) = (b.create_block(&[]), b.create_block(&[]), b.icmp(IntCond::Sge, len, want));
            b.cond_br(full, (l_stop, &[]), (l_on, &[]));
            b.switch_to(l_stop);
            let st = b.iconst(Type::I64, gravity_abi::status::suspend(false));
            env.stop(b, &carry, row.next, st);
            b.switch_to(l_on);
        }
        carry
    }
    fn finish(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, x: &Exits) {
        self.0.finish(b, env, x)
    }
    /// One worker fills the run, so its last run is sealed in place in arrival order; a wide Sort's (D3190, merge.ksort)
    /// runs on every worker, its pipeline runs.sort's.
    fn pipeline_desc(&self, px: &Px<'_, '_>, k: usize, d: Pipeline) -> Pipeline {
        let shape = if self.6 { gravity_program::StepShape::MergeInnerWide } else { gravity_program::StepShape::MergeInnerRun };
        let d = match gravity_program::step_serial(&shape) {
            Some(reason) if d.is_eligible() => Pipeline { parallel: gravity_program::Parallel::Ineligible { site: d.driving_site, reason }, ..d },
            _ if self.6 => self.0.pipeline_desc(px, k, d),
            _ => d,
        };
        match self.7 {
            Some(w) => iface::end_holds(d, gravity_program::spill::Shape::Grows(w)),
            None => d,
        }
    }
}

struct Reader {
    sh: Shape,
    emit: RunEmit,
    i: u64,
    slot: u8,
    base: Once<Option<Value>>,
    lean: bool,
    /// D3105 (b): a Sort's or a Material's run (`RunKind::put_road`): opened by `m11_run_open`, the sealed run or the
    /// merged runs' window.
    put: bool,
    opened: Once<Option<crate::face::slot_window::Opened>>,
    /// D3190: the co-split's range table word (face/cosplit.rs): the run read is the claim's slice of it (`m11_run_slice`).
    cosplit: Option<u16>,
}

impl SlotRun for Reader {
    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, x: &Exits) -> Value {
        if self.put || self.cosplit.is_some() {
            let full = self.sh.table(b);
            let view = if self.lean { view_table(b, &self.sh, true) } else { full };
            let (o, rows) = match self.cosplit {
                Some(w) => {
                    let r = crate::face::cosplit::range(b, env, w, crate::face::cosplit::Side::Inner);
                    crate::face::slot_window::open_slice(b, env, x, self.i, (view, full), !self.lean, r, self.sh.row_bytes)
                }
                None => {
                    let o = crate::face::slot_window::open(b, env, x, self.i, (view, full), !self.lean);
                    (o, b.load_zext(Type::I32, o.run, 4)) // SortRun::len, the window's rows
                }
            };
            self.opened.set(Some(o));
            return rows;
        }
        let tbl = view_table(b, &self.sh, self.lean);
        let h = res::host(b, "m11_run_view", &[Type::Ptr, Type::I64, Type::Ptr, Type::I64], Type::Ptr, gravity_abi::host_words!(h11::m11_run_view(ctx, k, tbl, slot)));
        let (i, slot) = (b.iconst(Type::I64, self.i as i64), b.iconst(Type::I64, self.slot as i64));
        let run = b.call_host(h, &[env.ctx, i, tbl, slot], Some(Type::Ptr)).expect("m11_run_view returns the run");
        let ri = b.conv(ConvOp::PtrToInt, Type::I64, run);
        let (bad, l_ok) = (b.icmp(IntCond::Slt, ri, env.zero), b.create_block(&[]));
        b.cond_br(bad, (x.l_exit, &[ri]), (l_ok, &[]));
        b.switch_to(l_ok);
        self.base.set(Some(run));
        b.load_zext(Type::I32, run, 4) // SortRun::len
    }

    fn fetch(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, j: Value) -> Value {
        if self.put || self.cosplit.is_some() {
            let Some(o) = self.opened.get() else { crate::refuse!(env.node, Reason::Bug { payload: "a slot run row before its open" }) };
            return crate::face::slot_window::fetch(b, env, o, self.sh.row_bytes, j);
        }
        let Some(base) = self.base.get() else { crate::refuse!(env.node, Reason::Bug { payload: "a slot run row before its open" }) };
        let rb = b.iconst(Type::I64, self.sh.row_bytes as i64);
        let off = b.int_bin(IntBinOp::Mul, j, rb);
        b.ptr_add(base, off, 1, SORTRUN_HDR as i32)
    }

    fn vals<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, p: Value, carry: Carry<'e>, next: Value, at: Value) -> RowVals<'e> {
        RunReader::vals(&self.emit, b, env, p, carry, next, at)
    }

    /// D3105 (b): the merge's mark told the window (its forward fills keep the mark's rows).
    fn mark(&self, b: &mut FunctionBuilder<'_>, _env: &mut Env, j: Value) {
        if let Some(o) = self.opened.get() {
            crate::face::slot_window::mark(b, o, j);
        }
    }
}

/// The table a reader's view takes: the run's own, or (`lean`: the merge reads no by-reference cell of the run,
/// exact_inner.rs `lean_view`) one naming no image entry and no var section, so the view points nothing and answers at
/// once where the run's own table would walk every row of a run whose by-reference cells are all NULL (D1521's absent
/// columns), at each window.
fn view_table(b: &mut FunctionBuilder<'_>, sh: &Shape, lean: bool) -> Value {
    if !lean {
        return sh.table(b);
    }
    crate::face::runs::stack_words(b, &[sh.row_bytes, 2 * sh.keys.len() as u32, sh.cols.len() as u32, 0, sh.spec as u32, (sh.spec >> 32) as u32])
}

/// `SpoolPoint(ctx, run, tbl, from)`: rows `[from, len)` of a pulled run pointed at their images.
fn point(b: &mut FunctionBuilder<'_>, env: &Env, run: Value, tbl: Value, from: Value) {
    let h = res::host(b, "spool_point", &[Type::Ptr, Type::Ptr, Type::Ptr, Type::I64], Type::I64, gravity_kernels::contract::entry3::<h3::SpoolPoint> as *const () as usize);
    let _ = b.call_host(h, &[env.ctx, run, tbl, from], Some(Type::I64));
}

/// The pulled reader (jmerge-0012): the run as stored so far at each open, its base in frame word `w_base`.
struct Pulled {
    sh: Shape,
    emit: RunEmit,
    i: u64,
    w_base: i32,
    lean: bool,
}

impl SlotRun for Pulled {
    /// `SpoolLive(ctx, i, tbl)`: the run as stored now (an error leaves by `x.l_exit`), its base kept, its row count.
    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, x: &Exits) -> Value {
        let tbl = view_table(b, &self.sh, self.lean);
        let h = res::host(b, "spool_live", &[Type::Ptr, Type::I64, Type::Ptr], Type::Ptr, gravity_kernels::contract::entry2::<h3::SpoolLive> as *const () as usize);
        let i = b.iconst(Type::I64, self.i as i64);
        let run = b.call_host(h, &[env.ctx, i, tbl], Some(Type::Ptr)).expect("spool_live returns the run");
        let ri = b.conv(ConvOp::PtrToInt, Type::I64, run);
        let (bad, l_ok) = (b.icmp(IntCond::Slt, ri, env.zero), b.create_block(&[]));
        b.cond_br(bad, (x.l_exit, &[ri]), (l_ok, &[]));
        b.switch_to(l_ok);
        env.store_word(b, self.w_base, ri);
        b.load_zext(Type::I32, run, 4) // SortRun::len
    }

    /// Row `j` at the base the last open kept (a pull since moves it only through that open).
    fn fetch(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, j: Value) -> Value {
        let w = env.load_word(b, self.w_base);
        let base = b.conv(ConvOp::IntToPtr, Type::Ptr, w);
        let rb = b.iconst(Type::I64, self.sh.row_bytes as i64);
        let off = b.int_bin(IntBinOp::Mul, j, rb);
        b.ptr_add(base, off, 1, SORTRUN_HDR as i32)
    }

    fn vals<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, p: Value, carry: Carry<'e>, next: Value, at: Value) -> RowVals<'e> {
        RunReader::vals(&self.emit, b, env, p, carry, next, at)
    }
}
