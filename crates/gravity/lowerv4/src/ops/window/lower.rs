//! Kind `window.agg`: PostgreSQL's WindowAgg over its partition buffer. decompose
//! (V4R-C) closes a Materialize under every WindowAgg and rewrites its qual, run condition and target list so each window
//! function reads column `natts + 1 + i` of the window's row. Three faces, one kind:
//! - the end (window/rows.rs): the buffer's producer stores each input row with every function's argument and FILTER
//!   cells into the window object (hosts v4/m10.rs), one cell's error kept and raised only where PG evaluates it;
//! - the source (window/rows.rs): the stored rows read back in order;
//! - the step (this file): `m10_win_row` runs PG's `ExecWindowAgg` for the row (its functions' values into the object's
//!   result cells), the run condition with its pass-through modes, then the row goes on with the results as columns.
//! Under a VOLATILE argument (D1232) every argument is the object's to evaluate at each read, and the pipeline runs on the
//! leader; an equal VOLATILE window function is its own entry (D1354). PG projects a RUN row before its run condition
//! and qual: a projection that may raise or calls a VOLATILE function is evaluated on the rows they drop too, on the
//! leader (D1234). D1415: a SubPlan in an argument the object reads only at the current row as int4 (lead / lag's offset,
//! nth_value's n, ntile's buckets) is evaluated by the step into the stored row (`step_arg`); one in any other argument or
//! FILTER that raises nothing and calls nothing VOLATILE by the fill, per stored row (`on_fill`). Refused by name: a window
//! function outside windowfuncs.c, any other argument with a SubPlan, a CTE's executor Param under a VOLATILE argument (an
//! extern `$n` is served, D1355; an InitPlan's runs at its first read, W3), a run condition or qual that may raise over
//! such a projection. Owner M10-WINDOW.
use gravity_abi::status::raise;
use gravity_decompose::expr::has_subplan;
use gravity_decompose::ops::shared::subplan::{SubLink, SubSite};
use gravity_decompose::ops::window::{arg_cells, WinFn, WinFnKind, WindowDesc};
use gravity_decompose::{Op, Source};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{Block, ConvOp, HostFlags, HostFn, IntBinOp, IntCond, Type, Value};
use gravity_kernels::ops::window::entries as abi;
use gravity_kernels::ops::window::raw;
use gravity_program::protos::{Proto, ProtoKind};
use gravity_program::{NumTypmod, Rep, Until};
use mcx::{CollectIn, GrowIn, PgBox, PgVec};
use types_nodes::primnodes::WindowFunc;
use types_nodes::Node;

use crate::cell::{At, Cell, CellHome, CellLife, Nullable, Row};
use crate::face::kinds::{Face, KindId};
use crate::face::step::{ExprMech, StepMech};
use crate::frame::env::Env;
use crate::frame::frame::{Next, StepMaker};
use crate::frame::px::{ExId, Px};
use crate::frame::row::{Carry, RowVals};
use crate::graph::Gx;
use crate::admit::refusal::{Reason, ShapeWhy};
use crate::ops::Table;

use crate::ops::window::rows::{datum_of, ev, Ev};

pub(crate) const ROWS: Table = Table { kinds: &[(KindId::WindowAgg, &StepMaker(make))], ..crate::ops::NONE };

pub(crate) fn shape(node: i32, why: &'static str) -> ! {
    crate::refuse!(node, Reason::Shape { why: ShapeWhy(why) })
}

/// The window objects' handle words by WindowAgg node, made by the first of its end, source and step to lower, each with
/// the window's form when it is read by claims (face/partruns.rs: the handle word is then its objects' table).
struct WinExt<'g> {
    handles: PgVec<'g, (i32, u16, Option<crate::face::partruns::Parted>)>,
    /// D3336 (A1a, face/partfill.rs): the fills whose source may run as claims over K-cut parts (topn/lower.rs
    /// `make_drain` offers them before it asks the window's handle), each with its sorted wide emit's run table word.
    cut_fills: PgVec<'g, (usize, u16)>,
}
crate::frame::px::ext_g!(WinExt<'g>, |m| WinExt { handles: PgVec::new_in(m), cut_fills: PgVec::new_in(m) });

/// The proto argument's bit (above the node id) of a window whose object evaluates every argument cell (D1232).
pub(crate) const ARG_LAZY: u64 = 1 << 32;
/// D3584: the proto argument's bit of a window whose object may evaluate its `deferred_arg` cells at a read (`lazy_cells`:
/// the store keeps a wide value of one as `CELL_LAZY`); exec's prepare reads the same rule (bits 33 / 34 are protos'
/// WINDOW_PULLED / WINDOW_AHEAD).
pub(crate) const ARG_DEFER: u64 = 1 << 35;
/// D1354: the proto argument's function count (bits 40..56): exec's prepare builds the same list (its VOLATILE reading
/// PG's contain_volatile_functions) and refuses one of another length.
const ARG_COUNT_SHIFT: u32 = 40;

/// WindowAgg `node`'s handle word (its protocol declared at its first use, by pipeline `k`); `d.volatile_args`: a VOLATILE argument (D1232), its function count in bits 40..56 (D1354).
pub(crate) fn handle<'p>(px: &mut Px<'_, 'p>, node: i32, k: usize, d: &WindowDesc<'p>) -> u16 {
    if let Some(&(_, h, _)) = px.ext_mut::<WinExt>(Face::Window).handles.iter().find(|x| x.0 == node) {
        return h;
    }
    let (n, lazy) = (d.functions.len(), d.volatile_args);
    let defer = !lazy && lazy_cells(px.gx, d).contains(&true);
    if n > u16::MAX as usize {
        shape(node, "a WindowAgg with more than 65,535 window functions");
    }
    // the exact request (W4): a pulled fill's proto names the fill (exec's pulled set), whichever face declares it
    let (pulled, k) = match pulled(px, node) {
        true if window_after_args(&px.gx.pipelines()[k].ops).is_some_and(|(_, w)| w.plan.plan.plan_node_id == node) => (true, fill_of(px, k)),
        p => (p, k),
    };
    // W10: a streaming fill's pulls read ahead with a budget that grows (`grows`); an exact one's read one row each
    let grow = if pulled && grows(px, node) { gravity_program::protos::WINDOW_AHEAD } else { 0 };
    let arg = node as u32 as u64 | if lazy { ARG_LAZY } else { 0 } | if defer { ARG_DEFER } else { 0 } | (n as u64) << ARG_COUNT_SHIFT | if pulled { gravity_program::protos::WINDOW_PULLED } else { 0 } | grow;
    // D3336 (win.partruns): a window read by claims declares its objects' table instead (the road off: never asked, the
    // lowering unchanged)
    let mut part = None;
    if !pulled && crate::face::partruns::road_on(px) {
        let (fill, reader) = match window_after_args(&px.gx.pipelines()[k].ops).is_some_and(|(_, w)| w.plan.plan.plan_node_id == node) {
            true => (fill_of(px, k), k),
            false => (k, crate::ops::window::rows::reader_opt(px, k).unwrap_or(usize::MAX)),
        };
        if let Some(site) = crate::face::partruns::decide(px, d, node, fill, reader) {
            // D3336 (A1a): a fill its source offered for claims (`offer_cut`) runs as claims too (`WINDOW_CUT`)
            let cut = px.ext::<WinExt>(Face::Window).and_then(|e| e.cut_fills.iter().find(|c| c.0 == fill)).map(|c| c.1).filter(|_| crate::face::partfill::road_on(px));
            part = Some(crate::face::partruns::declare(px, arg, fill, reader, site, cut));
        }
    }
    let h = match part {
        Some(p) => p.table,
        None => {
            let h = px.proto_words(1);
            px.declare_proto(Proto::new(ProtoKind::Window, k as u16).words(h, 1).arg(arg));
            h
        }
    };
    px.ext_mut::<WinExt>(Face::Window).handles.push_in((node, h, part));
    h
}

/// D3336 (A1a, face/partfill.rs): fill pipeline `k`'s source may read its rows as claims over the K-cut parts of the
/// sorted wide emit whose run table is word `runs`; offered before the window's `handle` is first asked, which then
/// declares the window's fill as claims when the window is parted.
pub(crate) fn offer_cut(px: &mut Px<'_, '_>, k: usize, runs: u16) {
    let e = px.ext_mut::<WinExt>(Face::Window);
    if !e.cut_fills.iter().any(|c| c.0 == k) {
        e.cut_fills.push_in((k, runs));
    }
}

/// D3336: whether pipeline `k` reads a window by claims (face/partruns.rs `reads_wide`).
pub(crate) fn parted_reader(px: &Px<'_, '_>, k: usize) -> bool {
    px.ext::<WinExt>(Face::Window).is_some_and(|e| e.handles.iter().any(|x| x.2.is_some_and(|p| p.reader == k)))
}

/// D3336: WindowAgg `node`'s form when it is read by claims (`handle` declared it so).
pub(crate) fn parted(px: &mut Px<'_, '_>, node: i32) -> Option<crate::face::partruns::Parted> {
    px.ext_mut::<WinExt>(Face::Window).handles.iter().find(|x| x.0 == node).and_then(|x| x.2)
}

/// The handle word's value (the object's address, laid once per execution).
pub(crate) fn load_handle(b: &mut FunctionBuilder<'_>, env: &Env, h: u16) -> Value {
    b.load(Type::I64, env.params, h as i32 * 8)
}

/// A kernels entry of M10 (`m10_*`, host_abi/v4/m10.rs) returning its I64 status.
pub(crate) fn host(b: &mut FunctionBuilder<'_>, name: &str, params: &[Type], addr: usize) -> HostFn {
    b.declare_host_once(name, params, Some(Type::I64), addr, HostFlags { touches_memory: true, may_error: true, ..HostFlags::default() })
}

/// Leaves with the host's stashed error when `st` is below zero: through the row's raise block inside a row.
pub(crate) fn raise_below_zero(b: &mut FunctionBuilder<'_>, env: &Env, row: Option<&RowVals>, st: Value) {
    let (bad, l_bad, l_ok) = (b.icmp(IntCond::Slt, st, env.zero), b.create_block(&[]), b.create_block(&[]));
    b.mark_cold(l_bad);
    b.cond_br(bad, (l_bad, &[]), (l_ok, &[]));
    b.switch_to(l_bad);
    let e = b.iconst(Type::I64, raise::bit(raise::HOST) as i64);
    match (env.l_raise_row, row) {
        (Some(l), Some(r)) => b.br(l, &[e, r.carry.out, r.next]),
        _ => b.br(env.l_raise, &[e]),
    }
    b.switch_to(l_ok);
}

/// The WindowAgg op of pipeline `k` (the reader's first op, or the first past the SubPlan steps decompose declares for its
/// functions' arguments, `SubSite::AggArg`: D1415) and its plan node. D2252: a SubPlan of the target list outside the
/// functions is declared ahead of the window too (the window op reads its tlist); no step serves it, so it refuses by name.
pub(crate) fn desc_of<'g, 'p>(px: &Px<'g, 'p>, k: usize) -> (&'g WindowDesc<'p>, i32) {
    let p = &px.gx.pipelines()[k];
    match window_after_args(&p.ops) {
        Some((w, d)) if p.ops[..w].iter().any(|o| matches!(o, Op::SubPlan(s) if s.site != SubSite::AggArg)) => shape(d.plan.plan.plan_node_id, SUBPLAN_TLIST),
        Some((_, d)) => (d, d.plan.plan.plan_node_id),
        None => crate::refuse!(p.node_id, Reason::Bug { payload: "a window reader whose first op is not its window" }),
    }
}

/// WindowAgg `node`'s fill is a pulled set (`WINDOW_PULLED`): deferred, run by the object's spool as PG's spool_tuples
/// reads its input, never whole ahead of the reader; the fill and the reader run on the leader. Under the exact request
/// (`exact`, W4) one row per pull; a fill that streams (`grows`, W10) with a budget that grows.
pub(crate) fn pulled(px: &Px<'_, '_>, node: i32) -> bool {
    exact(px, node) || grows(px, node)
}

/// The exact request (PLAN-v5 §2.5; W4): a stopping consumer asks Exact of WindowAgg `node` (decompose `ops::limit`
/// window_keeps_stop: a LIMIT over a fill whose code acts, or may raise with no read-ahead licence). Its pulled fill runs
/// one row per pull, so it reads the rows PG's spool_tuples reads and no more. W10: any consumer may stop where the lowering
/// cannot see it (a cursor's FETCH, an EXISTS, a scalar subquery, the top WindowAgg's own run condition), so a streaming fill
/// whose code calls something VOLATILE is always exact: its calls are PG's count (a sequence's), where the whole fill made
/// one per input row. A pull costs about 0.25 us (box 17: 100,000 rows over nextval 59 ms whole, 85 ms exact).
pub(crate) fn exact(px: &Px<'_, '_>, node: i32) -> bool {
    px.request(node).delivery == crate::frame::iface::Delivery::Exact || streaming_fill(px, node) == Some(false)
}

/// W10 (PM1b, the budgeted pull): WindowAgg `node`'s fill, not asked Exact, streams and calls nothing VOLATILE (no count PG
/// keeps; `streaming_fill`). Its pulls read ahead with a budget that doubles from one row (`WINDOW_AHEAD`, the object's
/// `Rt::more`): under a consumer that stops (a LIMIT, a cursor's FETCH, a run condition, an EXISTS) the fill reads under
/// twice the rows PG's spool reads, and a whole read pays one pull per doubling. Reading ahead of PG's spool departs from
/// it only for code that may raise: a pull that raises past the row the spool wanted keeps the error for the next pull
/// (the object's `Rt::more`), so it is raised only where a row needs the row that raised, as PG's spool raises it.
pub(crate) fn grows(px: &Px<'_, '_>, node: i32) -> bool {
    px.request(node).delivery != crate::frame::iface::Delivery::Exact && streaming_fill(px, node) == Some(true)
}

/// W10: whether WindowAgg `node`'s fill calls nothing VOLATILE, when it streams: a relation scan's rows through filters and
/// projections, reading no other pipeline (no producer the pulled set would run on the leader alone: a Sort's or a hash
/// build's input stays eager, at its own width), outside every rerun set (the pulled form opens once). D3377 road (3) (b):
/// or a settled union's emit whose arms it pulls (rerun/union_arms.rs `arms_read`: its feeds, the union's producers, read
/// nothing), each arm's ops counted too. D3373 (D-a): or a recursive union's store a CteScan reads (`union_store`). `None`:
/// it does not.
fn streaming_fill(px: &Px<'_, '_>, node: i32) -> Option<bool> {
    let ps = px.gx.pipelines();
    let k = (0..ps.len()).find(|&k| window_after_args(&ps[k].ops).is_some_and(|(_, w)| w.plan.plan.plan_node_id == node))?;
    let Source::Breaker { slot } = ps[k].source else { return None };
    let f = px.gx.life_of(k, slot)?.0 as usize;
    let fp = &ps[f];
    let arms = crate::rerun::union_arms::arms_read(px.gx, f);
    let union = |o: &Op<'_>| matches!(o, Op::Append(gravity_decompose::ops::shared::append::AppendDesc::Union { .. }));
    let plain = fp.ops.iter().all(|o| matches!(o, Op::Filter(_) | Op::Gate(_) | Op::Project(_)) || (arms && union(o)));
    let rerun = px.gx.graph.reruns.iter().any(|r| r.pipelines.contains(&(k as u16)) || r.pipelines.contains(&(f as u16)));
    // D3373 (D-a): or the store of a recursive union a CteScan reads (filters and projections over it), which the union's
    // driver fills only as its readers pull it (`union_store`): the window's pulls nest the store's
    let store = matches!(fp.source, Source::Breaker { slot } if fp.desc.nconsumed() == 1 && px.gx.life_of(f, slot).is_some_and(|l| union_store(px.gx, l.0 as usize)));
    if ((fp.source.relation().is_none() || fp.desc.nconsumed() != 0) && !arms && !store) || fp.union.is_some() || !plain || rerun {
        return None;
    }
    let quiet = |n: Node<'_>| !crate::effects::volatile(px.gx, n);
    let calm = |ops: &[Op<'_>]| ops.iter().all(|o| match o {
        Op::Filter(n) | Op::Gate(n) => quiet(*n),
        Op::Project(l) => l.iter().all(quiet),
        o => arms && (union(o) || crate::rerun::union_arms::feed_op(o) || crate::frame::iface::limit_bounds(o).is_some()),
    });
    let feeds = if arms { px.gx.union_feeds(f) } else { Vec::new() };
    Some(calm(&fp.ops) && feeds.iter().all(|&a| calm(&ps[a].ops)))
}

/// D3373 (D-a): pipeline `q` is a recursive union's driver whose rows are a CTE's store (ops/setop/recursive.rs: the store
/// its readers' pulls fill, `KindId::NestCte`), in no rerun set: the union runs only as a reader pulls it.
fn union_store(gx: &Gx<'_, '_>, q: usize) -> bool {
    let ps = gx.pipelines();
    q < ps.len() && ps[q].ops.iter().any(|o| matches!(o, Op::SetOp(gravity_decompose::ops::shared::setop::SetDesc::Iter(_)))) && gx.spool_kind(q) == KindId::NestCte && !gx.graph.reruns.iter().any(|r| r.pipelines.contains(&(q as u16)))
}

/// Reader `k`'s fill: the pipeline whose rows its source reads.
pub(crate) fn fill_of(px: &Px<'_, '_>, k: usize) -> usize {
    let p = &px.gx.pipelines()[k];
    let fill = match p.source {
        Source::Breaker { slot } => px.gx.life_of(k, slot).map(|l| l.0 as usize),
        _ => None,
    };
    fill.unwrap_or_else(|| crate::refuse!(p.node_id, Reason::Bug { payload: "a window reader with no fill" }))
}

/// admit's name for a pulled fill in a rerun set the set does not re-arm (rerun/sets.rs `rearms_window_fill`, D4237).
const PULLED_RERUN: &str = "a WindowAgg asked for the exact request inside a rerun set (its fill pulled to PostgreSQL's spool: W4's first form opens it once)";
/// D3373 (D-a): admit's name for a window over a recursive union (`IterDesc::window`) that does not take the pulled fill.
const WINDOW_WHOLE: &str = "a recursive union read by a window that does not stream it (the W10 pulled fill: a window reading the union whole before its first row never returns over one that never ends: D2223)";

/// program.rs's name for a pulled fill whose once-run producer a pipeline outside the pulled set also reads (as nest.cte).
pub(crate) const PULLED_SHARED: &str = "a WindowAgg asked for the exact request whose fill's producer a pipeline outside it also reads";

/// desc_of's name for a SubPlan in the WindowAgg's target list outside its window functions (D2252).
const SUBPLAN_TLIST: &str = "a SubPlan in a WindowAgg's target list outside its window functions (declared ahead of the window: D2252)";

/// The window op's index in reader `k`'s ops.
fn at_of(px: &Px<'_, '_>, k: usize) -> usize {
    window_after_args(&px.gx.pipelines()[k].ops).map_or(0, |x| x.0)
}

/// D1415: argument `a` of `f` holds a SubPlan and the object reads it only at the current row, as int4 (PG
/// `WinGetFuncArgCurrent`: lead / lag's offset, nth_value's n, ntile's buckets): the step evaluates it over the stored row
/// and writes it into that row's cell before the object runs; the end stores it NULL.
pub(crate) fn step_arg(f: &WinFn<'_>, a: usize) -> bool {
    gravity_decompose::ops::window::step_arg(f.func, a)
}

/// admit's name for a window function argument with a SubPlan neither the step evaluates (D1415 `step_arg`) nor the fill
/// (`on_fill`).
const SUBPLAN_ARG: &str = "a window function argument with a SubPlan";

/// Whether every SubPlan of argument or FILTER `n` has its op on the fill `fill` ahead of its end (decompose subplan.rs
/// `attach_window_args`: one that raises nothing and calls nothing VOLATILE), which evaluates it per stored row, where
/// the end's argument cell reads it; never under a VOLATILE argument (each cell lazy, the object evaluates it, D1232).
fn on_fill(px: &Px<'_, '_>, d: &WindowDesc<'_>, fill: usize, n: Node<'_>) -> bool {
    let ops = &px.gx.pipelines()[fill].ops;
    let mut all = !d.volatile_args;
    gravity_decompose::expr::walk(n, &mut |x| {
        if let Some(s) = x.as_sub_plan() {
            all &= ops.iter().any(|o| matches!(o, Op::SubPlan(p) if p.plan_id == s.plan_id));
        }
        all && x.as_alternative_sub_plan().is_none()
    });
    all
}

/// The window op of a reader's ops and its index: the first op, or the first after the SubPlans decompose declared ahead of
/// it (its functions' arguments, `SubSite::AggArg`, D1415; any other one desc_of refuses by name, D2252).
pub(crate) fn window_after_args<'g, 'p>(ops: &'g [Op<'p>]) -> Option<(usize, &'g WindowDesc<'p>)> {
    let i = ops.iter().position(|o| !matches!(o, Op::SubPlan(_)))?;
    match &ops[i] {
        Op::Window(d) => Some((i, d)),
        _ => None,
    }
}

/// The admission shared by the three faces over reader pipeline `k` (each checks it: the first to lower refuses): what
/// PG evaluates in an order or a number of times the stored row cannot keep. The whole-read licence of an acting
/// argument SubPlan's fill comes back (D3374; `None`: no such fill, or one whose acts are not call-local).
pub(crate) fn admit(px: &mut Px<'_, '_>, k: usize) -> Option<gravity_decompose::effects::Licence> {
    let (d, node) = desc_of(px, k);
    if pulled(px, node) {
        let fill = fill_of(px, k);
        if px.gx.graph.reruns.iter().any(|r| r.pipelines.contains(&(k as u16)) || r.pipelines.contains(&(fill as u16))) && !crate::rerun::sets::rearms_window_fill(px.gx, k, fill) {
            shape(node, PULLED_RERUN);
        }
    }
    // D3373 (D-a): a window decompose found straight over a CteScan of a recursive union (the union's `IterDesc::window`)
    // reads it as PG's spool_tuples does only through the W10 pulled fill: any other reads the union whole first
    let over_union = |o: &Op<'_>| matches!(o, Op::SetOp(gravity_decompose::ops::shared::setop::SetDesc::Iter(i)) if i.window == Some(node));
    if !pulled(px, node) && px.gx.pipelines().iter().any(|p| p.ops.iter().any(over_union)) {
        shape(node, WINDOW_WHOLE);
    }
    for f in d.functions.iter().filter(|f| f.kind == WinFnKind::Other) {
        if let Err(why) = user_window(px, f.func) {
            shape(node, why);
        }
    }
    if d.volatile_args {
        if let Err(why) = lazy_params(px.gx, d, &lazy_cells(px.gx, d)) {
            shape(node, why);
        }
    }
    let fill = fill_of(px, k);
    for f in d.functions.iter() {
        let other = f.func.args.iter().enumerate().any(|(a, n)| has_subplan(n) && (d.volatile_args || !step_arg(f, a) && !on_fill(px, d, fill, n)));
        if other || f.func.aggfilter.is_some_and(|q| has_subplan(q) && !on_fill(px, d, fill, q)) {
            shape(node, SUBPLAN_ARG);
        }
    }
    if !d.top && !d.plan.plan.qual.is_nil() {
        shape(node, "a qual on a WindowAgg below the top one");
    }
    // D3374: an acting argument SubPlan on the fill (decompose `attach_window_args`) keeps PostgreSQL's calls beside a
    // statement that runs nothing else with effects among them and reads every row (effects `window_fill_exact`); its
    // fill runs on the leader in the buffer's order, and a reader that stops early is delegated (`whole_read`, under
    // its licence: `shared::whole::read`)
    let mut ahead = None;
    if px.gx.pipelines()[fill].ops.iter().any(|o| matches!(o, Op::SubPlan(s) if s.effects().acts)) {
        if let Err(why) = crate::effects::window_fill_exact(px.gx, px.arena, k, fill) {
            shape(node, why);
        }
        let own = px.gx.pipelines()[fill].ops.iter().filter_map(|o| if let Op::SubPlan(s) = o { Some(s.effects()) } else { None }).fold(gravity_decompose::effects::Effects::NONE, |e, s| e.and(s));
        ahead = crate::ops::shared::whole::read(px, k, own);
        if !px.serial.contains(&fill) {
            px.serial.push(fill); // alloc: Program
        }
    }
    proj_first(px, k);
    ahead
}

/// D1234: PG projects each RUN-mode row before its run condition and qual (`ExecWindowAgg`), so a row either drops was
/// projected too: when that projection may raise or calls a VOLATILE function, the step evaluates it on those drop edges
/// (the qual checked in the step), the pipeline on the leader (the first raising row is PG's). The Project op's index,
/// else 0. Refused: a run condition or qual that may raise or is VOLATILE (PG raises the projection's error first; the
/// step and the Filter op both check the qual), a SubPlan in the projection, ops between the window and its projection.
pub(crate) fn proj_first(px: &Px<'_, '_>, k: usize) -> usize {
    let (d, node) = desc_of(px, k);
    let (ops, w) = (&px.gx.pipelines()[k].ops, at_of(px, k));
    let raises = ops.iter().find(|op| matches!(op, Op::Project(_))).is_some_and(|op| crate::props::ops_can_raise(px.gx, core::slice::from_ref(op)));
    if (d.run_condition.is_nil() && d.plan.plan.qual.is_nil()) || !(raises || d.volatile_proj) {
        return 0;
    }
    let f = w + 1 + ops[w + 1..].iter().take_while(|op| matches!(op, Op::Filter(_))).count();
    // ops_can_raise: a qual of anything but leakproof builtins (a VOLATILE call is never one) may raise
    let conds: PgVec<'_, Op<'_>> = d.run_condition.iter().map(Op::Filter).collect_in(px.mcx());
    let quals = crate::props::ops_can_raise(px.gx, &ops[w + 1..f]) || crate::props::ops_can_raise(px.gx, &conds[..]);
    let Some(Op::Project(l)) = ops.get(f) else { shape(node, "a WindowAgg run condition or qual over a projection that may raise or calls a VOLATILE function, ops between (PG projects first)") };
    if quals || l.iter().any(gravity_decompose::expr::has_subplan) {
        shape(node, "a WindowAgg run condition or qual that may raise or is VOLATILE, or a SubPlan, over a projection that may raise or calls a VOLATILE function (PG projects first)");
    }
    f
}

/// D1233: the windowfuncs.c signatures by builtin OID: argument count, the argument read as an int4 (ntile's buckets,
/// lag / lead's offset, nth_value's n; none = u8::MAX) and the result (0 = the first argument's type).
const WINDOW_BUILTINS: [(u8, u8, u32); 15] = [(0, u8::MAX, 20), (0, u8::MAX, 20), (0, u8::MAX, 20), (0, u8::MAX, 701), (0, u8::MAX, 701),
    (1, 0, 23), (1, u8::MAX, 0), (2, 1, 0), (3, 1, 0), (1, u8::MAX, 0), (2, 1, 0), (3, 1, 0), (1, u8::MAX, 0), (1, u8::MAX, 0), (2, 1, 0)];

/// D1233: a user `LANGUAGE internal WINDOW` function the object runs as the windowfuncs.c builtin fmgr resolves it to
/// (FuncFacts.body; exec v4/m10.rs reads it again at prepare), when its arguments and result are that function's. W7
/// (wufn-0006): windowfuncs.c reads the int argument as `DatumGetInt32`, the word's low 32 bits, as the object does
/// (`as_i32`): a by-value argument of 4 or 8 bytes reads the same in both; a 1- or 2-byte one's bits 16..31 are its
/// GetDatum's extension, refused.
fn user_window(px: &Px<'_, '_>, f: &WindowFunc<'_>) -> Result<(), &'static str> {
    let facts = px.gx.catalog.func(f.winfnoid).ok();
    if f.winagg || facts.and_then(|x| x.prolang) != Some(12) {
        return Err("a window function outside windowfuncs.c (a C or PL window function's WindowObject is not built)");
    }
    let body = facts.map_or(0, |x| x.body);
    let Some(&(nargs, int_at, result)) = body.checked_sub(3100).and_then(|i| WINDOW_BUILTINS.get(i as usize)) else {
        return Err("an internal WINDOW function over a function outside windowfuncs.c");
    };
    let types: PgVec<'_, Option<u32>> = f.args.iter().map(gravity_decompose::catalog::expr_type::type_of).collect_in(px.mcx());
    let fits = types.len() == nargs as usize
        && types.iter().all(Option::is_some)
        && (int_at == u8::MAX || types[int_at as usize].is_some_and(|t| int_word(px, t)))
        && (nargs != 3 || types[2] == types[0])
        && f.wintype == if result == 0 { types.first().copied().flatten().unwrap_or(0) } else { result };
    if !fits {
        return Err("an internal WINDOW function whose arguments or result are not its windowfuncs.c function's");
    }
    Ok(())
}

/// A type windowfuncs.c's `DatumGetInt32` reads as the object reads its stored word: by value, 4 or 8 bytes.
fn int_word(px: &Px<'_, '_>, t: u32) -> bool {
    gravity_decompose::catalog::typefacts::facts_of(px.gx.facts, t).is_some_and(|tf| tf.byval && matches!(tf.typlen, 4 | 8))
}

/// W3 (window_core-0190 / -0191): the executor parameters (PARAM_EXEC) the object's lazy cells read (D1232: every
/// argument and FILTER under a VOLATILE one), each once, in first-read order, with whether an InitPlan sets it. The
/// reader's open evaluates an owner's parameter (a nest's or a SubPlan's, set before the inner opens) and hands its value
/// to the object (window/rows.rs, `m10_win_params`), which binds it beside the extern list (D1355). An InitPlan's is
/// handed over unevaluated, as its Init cell: the object leaves it pending (PG's `execPlan`) and runs the Init at the
/// cell's first read (`m8_init_run`), as PG's ExecEvalParamExec runs ExecSetParamPlan there, so an InitPlan no read
/// reaches (no row, an untaken CASE arm, a false FILTER) never runs (no departure: no licence asked). A CTE's parameter
/// is refused by name. Else the name of what the object cannot bind.
pub(crate) fn lazy_params<'g, 'p>(gx: &Gx<'g, 'p>, d: &WindowDesc<'p>, cells: &[bool]) -> Result<PgVec<'g, (i32, Node<'p>, bool)>, &'static str> {
    let mut out: PgVec<'_, (i32, Node<'p>)> = PgVec::new_in(gx.mcx);
    let (mut known, mut other) = (true, false);
    // D3584: the lazy cells only (`lazy_cells`: every one under a VOLATILE argument)
    let all = d.functions.iter().flat_map(|f| f.func.args.iter().chain(f.func.aggfilter));
    for e in all.zip(cells.iter().copied().chain(core::iter::repeat(false))).filter_map(|(e, lazy)| lazy.then_some(e)) {
        known &= gravity_decompose::expr::walk(e, &mut |x| {
            match x.as_param().map(|p| (p.paramkind, p.paramid)) {
                Some((types_nodes::primnodes::ParamKind::PARAM_EXEC, id)) if !out.iter().any(|o| o.0 == id) => out.push_in((id, x)),
                Some((types_nodes::primnodes::ParamKind::PARAM_EXEC | types_nodes::primnodes::ParamKind::PARAM_EXTERN, _)) | None => {}
                Some(_) => other = true,
            }
            true
        });
    }
    if !known || other {
        return Err("a window function argument of a node the walk does not know under a VOLATILE argument (D1355)");
    }
    // An InitPlan's parameter is the object's to run at the cell's first read (its Init cell, `m8_init_run`); a CTE's
    // InitPlan has no Init cell
    let init = |id: i32, cte: bool| gx.graph.init_plans.iter().any(|i| matches!(i.link, SubLink::Cte) == cte && i.params.iter().any(|x| x == id));
    if out.iter().any(|o| init(o.0, true)) {
        return Err("a window function argument reading a CTE's parameter under a VOLATILE argument");
    }
    Ok(out.into_iter().map(|(id, e)| (id, e, init(id, false))).collect_in(gx.mcx))
}

/// D3584: per stored argument / FILTER cell of `d` (decompose `arg_cells`' order, past the child's columns), whether the
/// object may evaluate it at a read, as PG evaluates a window function's argument where it reads it: every cell under a
/// VOLATILE argument (D1232, stored `CELL_LAZY`); else each decompose `deferred_arg` (a computed by-reference argument),
/// which the IR evaluates and stores, and the object's store keeps as `CELL_LAZY` instead of its copy when the value is
/// wide (hosts window store.rs `put`), when the object can bind what they read (`lazy_params` over them; else none). No
/// such cell: all false.
pub(crate) fn lazy_cells<'g, 'p>(gx: &Gx<'g, 'p>, d: &WindowDesc<'p>) -> PgVec<'g, bool> {
    let n: usize = d.functions.iter().map(|f| f.func.args.len() + f.func.aggfilter.is_some() as usize).sum();
    if d.volatile_args {
        return mcx::pg_vec![in gx.mcx; true; n];
    }
    // the argument's base type and whether it is by value (a type the facts do not know: by value, never deferred)
    let ty = |e: Node<'_>| {
        let t = gravity_decompose::catalog::expr_type::type_of(e).unwrap_or_else(|| gravity_decompose::catalog::expr_type::result_type(e));
        gravity_decompose::catalog::typefacts::facts_of(gx.facts, t).map_or((t, true), |f| (if f.base == 0 { t } else { f.base }, f.byval))
    };
    let mut cells = mcx::reserved_in(n, gx.mcx);
    for f in d.functions.iter() {
        cells.extend_in(f.func.args.iter().enumerate().map(|(a, e)| {
            let (base, byval) = ty(e);
            gravity_decompose::ops::window::deferred_arg(f.func, a, base, byval)
        }));
        if f.func.aggfilter.is_some() {
            cells.push_in(false);
        }
    }
    if cells.contains(&true) && lazy_params(gx, d, &cells[..]).is_err() {
        cells.iter_mut().for_each(|c| *c = false);
    }
    cells
}

struct WinStep<'g> {
    node: i32,
    /// D1232: the object evaluates the arguments at each read; D1234: the projection evaluated first; W4: the fill pulled
    /// (the exact request): the pipeline runs on the leader.
    lazy: bool,
    h: u16,
    ex: &'static dyn ExprMech,
    natts: usize,
    /// Each function's result cell facts.
    res: PgVec<'g, (Rep, u32)>,
    /// The run condition's clauses over the window's row.
    runcond: PgVec<'g, ExId>,
    /// D1234: the projection's computed entries, evaluated on a RUN row the run condition or qual drops (PG projects
    /// first), and the qual's clauses the step checks; both empty unless `proj_first` admits the window.
    proj: PgVec<'g, ExId>,
    qual: PgVec<'g, ExId>,
    /// D1415: the stored row's cells (`ncols`) and each `step_arg` cell with its argument over the reader's row, and, for
    /// ntile's buckets, its function's index (read at a partition's first row, again while NULL; D2246).
    ncols: usize,
    fill: PgVec<'g, (u16, Option<u16>, Ev)>,
    /// D3336: the window is read by claims: the frame word holding this claim's object (the reader's, rows.rs
    /// `object_word`), which the step reads in place of the handle word.
    part: Option<i32>,
    /// Construct C2: the whole-read licence of an acting argument SubPlan's fill (`admit`, D3374); `None`: no such
    /// fill, or its acts are not call-local. Held, not read.
    #[allow(dead_code)]
    ahead: Option<gravity_decompose::effects::Licence>,
}

fn make<'g>(px: &mut Px<'g, '_>, k: usize, j: usize) -> PgBox<'g, dyn StepMech + 'g> {
    let (d, node) = desc_of(px, k);
    let w = at_of(px, k);
    if j != w {
        crate::refuse!(node, Reason::Bug { payload: "a window step off its reader's window op" });
    }
    let ahead = admit(px, k);
    let ex = crate::frame::iface::expr_face(&node).0;
    let mcx = px.mcx();
    let mut res = mcx::reserved_in(d.functions.len(), mcx);
    for f in d.functions.iter() {
        let tf = ex.tf_of(px.gx, f.func.wintype);
        res.push_in((Rep::of_storage(tf.typlen, tf.byval), f.func.wintype));
    }
    let h = handle(px, node, k, d);
    let runcond = d.run_condition.iter().map(|c| ex.compile_at(px, (k, j + 1), c, None, node)).collect_in(mcx);
    let (mut proj, mut qual) = (PgVec::new_in(mcx), PgVec::new_in(mcx));
    let f = proj_first(px, k);
    let ops = &px.gx.pipelines()[k].ops;
    if let Some(Op::Project(l)) = ops.get(f).filter(|_| f > 0) {
        for te in l.iter().filter_map(|n| n.as_target_entry()).filter(|te| te.expr.as_var().is_none() && te.expr.as_const().is_none()) {
            proj.push_in(ex.compile_at(px, (k, f), te.expr, None, node));
        }
        for (i, op) in ops[w + 1..f].iter().enumerate() {
            let Op::Filter(q) = op else { continue };
            qual.push_in(ex.compile_at(px, (k, w + 1 + i), *q, None, node));
        }
    }
    let funcs: PgVec<'_, Node<'_>> = d.functions.iter().map(|f| f.node).collect_in(mcx);
    let cells = arg_cells(&funcs[..], d.natts as usize);
    let ncols = cells.last().map_or(d.natts as usize, |&(base, n, f)| base + n + f.is_some() as usize);
    let mut fill = PgVec::new_in(mcx);
    for (i, (wf, &(base, _, _))) in d.functions.iter().zip(cells.iter()).enumerate() {
        for (a, e) in wf.func.args.iter().enumerate().filter(|&(a, _)| step_arg(wf, a)) {
            let ntile = (wf.kind == WinFnKind::Ntile).then_some(i as u16);
            fill.push_in(((base + a) as u16, ntile, ev(px, ex, Some((k, j)), e, node)));
        }
    }
    // D3584: a deferred cell may be evaluated by the object, on the leader, as D1232's
    let lazy = d.volatile_args || f > 0 || pulled(px, node) || lazy_cells(px.gx, d).contains(&true);
    let part = parted(px, node).filter(|p| p.reader == k).map(|_| crate::ops::window::rows::object_word(px, k));
    mcx::pg_box_dyn!(mcx, WinStep { node, lazy, h, ex, natts: d.natts as usize, res, runcond, proj, qual, ncols, fill, part, ahead } => dyn StepMech + 'g)
}

impl WinStep<'_> {
    /// The window's row: the input row's columns, then each function's value from the object's result cells.
    fn row<'r>(&self, b: &mut FunctionBuilder<'_>, env: &Env, row: &RowVals<'r>, h: Value) -> RowVals<'r> {
        let mut out = RowVals { row: Row { cells: row.row.cells.clone() }, vals: row.vals.clone(), cols: row.cols.clone(), carry: row.carry.copy_in(*row.vals.allocator()), next: row.next, at: row.at };
        out.cols.resize_in(self.natts, None);
        let hp = b.conv(gravity_ir::ConvOp::IntToPtr, Type::Ptr, h);
        let res = b.load(Type::Ptr, hp, abi::WIN_RES as i32);
        let nf = self.res.len();
        for (i, &(rep, typid)) in self.res.iter().enumerate() {
            let v = b.load(Type::I64, res, 8 * i as i32);
            let valid = b.load_zext(Type::I8, res, abi::res_valid(nf, i) as i32);
            let null = b.icmp(IntCond::Eq, valid, env.zero);
            let life = CellLife { until: Until::Row, home: CellHome::FrameSlot };
            out.cols.push_in(Some(out.row.cells.len() as u16));
            out.row.cells.push_in(Cell { rep, typid, num: NumTypmod::NONE, life, at: At::Step { k: 0, col: i as u16 }, null: Nullable::Maybe });
            out.vals.push_in((v, Some(null)));
        }
        out
    }
}

impl StepMech for WinStep<'_> {
    /// PG `ExecWindowAgg`'s row: the object's answer, then (run mode) the run condition over the window's row.
    fn lower<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, row: &RowVals, next: Next<'_, 'e>) -> Carry<'e> {
        let types: mcx::PgVec<'_, Type> = row.carry.params().iter().map(|&v| b.value_type(v)).collect_in(env.mcx);
        let l_join = b.create_block(&types);
        let [l_done, l_have, l_next] = [(); 3].map(|_| b.create_block(&[]));
        let h = match self.part {
            Some(at) => env.load_word(b, at),
            None => load_handle(b, env, self.h),
        };
        self.fill_cells(b, env, row, h);
        let f = host(b, "m10_win_row", &[Type::Ptr, Type::I64, Type::I64], gravity_abi::host_words!(abi::m10_win_row(ctx, h, j)));
        let st = b.call_host(f, &[env.ctx, h, row.at], Some(Type::I64)).expect("m10_win_row returns its status");
        raise_below_zero(b, env, Some(row), st);
        self.branch(b, st, abi::ROW_DONE, l_done);
        let skip = b.iconst(Type::I64, abi::ROW_SKIP);
        let is_skip = b.icmp(IntCond::Eq, st, skip);
        b.cond_br(is_skip, (l_join, &row.carry.params()), (l_have, &[]));
        b.switch_to(l_have);
        let wrow = self.row(b, env, row, h);
        let l_kept = if self.qual.is_empty() { l_next } else { b.create_block(&[]) };
        if self.runcond.is_empty() {
            b.br(l_kept, &[]);
        } else {
            let l_check = b.create_block(&[]);
            let pass = b.iconst(Type::I64, abi::ROW_PASS);
            let is_pass = b.icmp(IntCond::Eq, st, pass);
            b.cond_br(is_pass, (l_next, &[]), (l_check, &[]));
            b.switch_to(l_check);
            let l_fail = b.create_block(&[]);
            for &q in self.runcond.iter() {
                let ok = self.ex.pred(b, env, self.node, &wrow, q);
                let l_yes = b.create_block(&[]);
                b.cond_br(ok, (l_yes, &[]), (l_fail, &[]));
                b.switch_to(l_yes);
            }
            b.br(l_kept, &[]);
            b.switch_to(l_fail);
            let f = host(b, "m10_win_runfail", &[Type::Ptr, Type::I64], gravity_abi::host_words!(abi::m10_win_runfail(ctx, h)));
            let r = b.call_host(f, &[env.ctx, h], Some(Type::I64)).expect("m10_win_runfail returns its status");
            raise_below_zero(b, env, Some(row), r);
            let emit = b.iconst(Type::I64, abi::ROW_EMIT);
            let goes = b.icmp(IntCond::Eq, r, emit);
            let l_drop = b.create_block(&[]);
            b.cond_br(goes, (l_next, &[]), (l_drop, &[]));
            // D1234: the row PG projected before its run condition failed (Done or PassThroughStrict: it goes no further)
            b.switch_to(l_drop);
            self.project(b, env, &wrow);
            self.branch(b, r, abi::ROW_DONE, l_done);
            b.br(l_join, &row.carry.params());
        }
        if !self.qual.is_empty() {
            // D1234: PG's top qual over the projected row: a row it filters was projected
            b.switch_to(l_kept);
            let l_out = b.create_block(&[]);
            for &q in self.qual.iter() {
                let ok = self.ex.pred(b, env, self.node, &wrow, q);
                let l_yes = b.create_block(&[]);
                b.cond_br(ok, (l_yes, &[]), (l_out, &[]));
                b.switch_to(l_yes);
            }
            b.br(l_next, &[]);
            b.switch_to(l_out);
            self.project(b, env, &wrow);
            b.br(l_join, &row.carry.params());
        }
        b.switch_to(l_next);
        let c = next(b, env, &wrow);
        b.br(l_join, &c.params());
        // PG's WINDOWAGG_DONE: no further row of the WindowAgg; the reader ends as a LIMIT met
        b.switch_to(l_done);
        env.full(b, &row.carry, row.next);
        b.switch_to(l_join);
        Carry::from_params(b.block_params(l_join), env.mcx)
    }

    /// D1232: a lazy argument may call a parallel-unsafe or SQL function, which only the leader runs. D1234: a projection
    /// that may raise or calls a VOLATILE function runs on the leader too (the first raising row, the calls: PG's order).
    /// W4: a pulled fill's reader too (the pull runs on the leader).
    fn serial(&self) -> bool {
        self.lazy
    }
}

impl WinStep<'_> {
    /// D1415: stored row `row.at`'s `step_arg` cells, each its argument's datum word and NULL byte over the row (the end's
    /// conversion, window/rows.rs `datum_of`), written before `m10_win_row` reads them. D2246: each only where PG reads it
    /// (`m10_win_reads`): a row whose functions run (not a run condition's pass-through row), and for ntile's buckets a
    /// partition's first row and each next row while they were NULL (asked per ntile); a resumed row writes none again.
    fn fill_cells(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, row: &RowVals, h: Value) {
        if self.fill.is_empty() {
            return;
        }
        let f = host(b, "m10_win_reads", &[Type::Ptr, Type::I64, Type::I64, Type::I64], gravity_kernels::contract::entry3::<raw::WinReads> as *const () as usize);
        let ask = |b: &mut FunctionBuilder<'_>, env: &mut Env, func: i64| {
            let func = b.iconst(Type::I64, func);
            let reads = b.call_host(f, &[env.ctx, h, row.at, func], Some(Type::I64)).expect("m10_win_reads returns its answer");
            raise_below_zero(b, env, Some(row), reads);
            reads
        };
        let mut run = None;
        let f = host(b, "m10_win_rec", &[Type::Ptr, Type::I64, Type::I64], gravity_abi::host_words!(abi::m10_win_rec(ctx, h, j)));
        let r = b.call_host(f, &[env.ctx, h, row.at], Some(Type::I64)).expect("m10_win_rec returns the row");
        raise_below_zero(b, env, Some(row), r);
        let rec = b.conv(ConvOp::IntToPtr, Type::Ptr, r);
        for (c, ntile, e) in self.fill.iter() {
            // asked in the block ahead of the cell's branch, which dominates every later cell's
            let (reads, bit) = match ntile {
                Some(i) => (ask(b, env, *i as i64 + 1), abi::READS_FIRST),
                None => (*run.get_or_insert_with(|| ask(b, env, 0)), abi::READS_RUN),
            };
            let bit = b.iconst(Type::I64, bit);
            let m = b.int_bin(IntBinOp::And, reads, bit);
            let read = b.icmp(IntCond::Ne, m, env.zero);
            let (l_fill, l_next) = (b.create_block(&[]), b.create_block(&[]));
            b.cond_br(read, (l_fill, &[]), (l_next, &[]));
            b.switch_to(l_fill);
            // the branch's values do not dominate the rows after it
            let memo = env.memo.set_aside();
            let (w, null) = datum_of(b, env, self.ex, self.node, row, e, 0x6000 + c);
            b.store(rec, 8 * *c as i32, w);
            let nb = b.conv(ConvOp::Zext, Type::I8, null);
            b.store(rec, (8 * self.ncols + *c as usize) as i32, nb);
            env.memo.restore(memo);
            b.br(l_next, &[]);
            b.switch_to(l_next);
        }
    }

    /// D1234: the projection's computed entries over `wrow`, values dropped (a raise leaves through the row's raise block).
    fn project(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, wrow: &RowVals) {
        for &e in self.proj.iter() {
            let _ = self.ex.emit(b, env, self.node, wrow, e);
        }
    }

    /// To `to` when `st` is `code`; the builder left on the other edge.
    fn branch(&self, b: &mut FunctionBuilder<'_>, st: Value, code: i64, to: Block) {
        let (c, l_other) = (b.iconst(Type::I64, code), b.create_block(&[]));
        let is = b.icmp(IntCond::Eq, st, c);
        b.cond_br(is, (to, &[]), (l_other, &[]));
        b.switch_to(l_other);
    }
}
