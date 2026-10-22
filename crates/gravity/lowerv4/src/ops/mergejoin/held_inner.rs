//! merge.join's held inner (D1236): op 14 for runs.sort. PostgreSQL's ExecMergeJoin first calls its inner
//! at INITIALIZE_INNER (the first matchable outer tuple) or ENDOUTER (a fill-inner join over an ended outer), and its Sort
//! (ExecSort) reads the whole subtree there: every error a Sort-ended inner raises surfaces at that first call, and none
//! when the merge makes no call. Where D824's lazy group cannot wait for that call (lower.rs: its group or the shape
//! conditions fail) and no recheck licence runs the inner ahead, the inner's Sort runs in program order as any Sort's,
//! and an error its fill raises is held: the program marks the pipeline `held_sort`, exec's merge for it stamps the run
//! `RUN_HELD` with no rows and the driver holds the error (op 14, `Entry::defer_failed`), and the merge's run reader
//! raises it at its first inner call (`first_fetch`, kernels `m11_run_held`), where the lazy group would have run the
//! inner. A held merge never fetching its inner drops the error with the statement, as PostgreSQL never raises it.
//! The machine is nodeMergejoin's own (machine.rs INITIALIZE_INNER, lower.rs ENDOUTER's `flush`), so the raise falls at
//! PostgreSQL's first inner call for every join type (a RIGHT / FULL join's ENDOUTER over an empty or unmatchable outer
//! too) and whatever runs before the merge in the outer (an outer that acts: its rows and effects up to that call are
//! PG's when the outer reads as PG's does, `outer_exact`; a merge a LIMIT stops early: no call, no raise). Taken for one
//! full Sort's fill (no UNION ALL arms) of builtin code
//! only (effects/user_fn.rs `held_node`, as op 14's nest builds: a user function's error is not held, D2573), its feeders quiet
//! (their errors are never held), its run read by this merge alone, outside every rerun set. The fill holds the held
//! error's licence (`effects::held_error`): a fill that acts acts only through call-local builtins (nextval, random ..:
//! `effects::call_local`, which also keeps no currval / lastval / setval / setseed beside it), whose call run where
//! PostgreSQL may make none is an extra call, free; the fill runs whole, so its calls keep their own order, and their
//! place against the statement's other calls (another nextval of the same sequence too) is one a commuted merge or a
//! hash join gives; any other VOLATILE builtin (setval, an advisory lock, set_config ..) is no held form's.
use gravity_abi::status::raise;
use gravity_decompose::graph::Bound;
use gravity_decompose::{Op, Source};
use gravity_ir::builder::FunctionBuilder;
use gravity_ir::{HostFlags, IntCond, Type, Value};
use gravity_kernels::host_abi::m11_run_held;
use gravity_program::LifeId;
use mcx::{GrowIn, PgBox, PgVec};

use crate::face::runs::SlotRun;
use crate::frame::env::Env;
use crate::frame::px::Px;
use crate::frame::row::{Carry, RowVals};
use crate::frame::stop::Exits;

/// Merge `(k, j)`'s inner `inner` (its one filler, `fillers`) takes the held form (`fill_holds`): its licence. The caller
/// has no recheck licence for the inner (`Px::run_ahead`) and no lazy group.
pub(crate) fn holds(px: &Px<'_, '_>, (k, _j): (usize, usize), inner: usize, fillers: &[usize]) -> Option<gravity_decompose::effects::Licence> {
    let held = fill_holds(px, k, inner, fillers);
    if held.is_some() {
        fires::fire("merge.inner.held"); // shapecheck fire signal (D1236's held inner, op 14 for runs.sort)
    }
    held
}

/// Full Sort `inner` (its one filler, `fillers`) holds its fill's error for its one reader `k` (`fill_shape`), under the
/// held error's licence: its fill's code acts nowhere, or only through call-local builtins in a statement that lets them
/// run ahead (`effects::call_local`). None otherwise.
pub(crate) fn fill_holds(px: &Px<'_, '_>, k: usize, inner: usize, fillers: &[usize]) -> Option<gravity_decompose::effects::Licence> {
    use gravity_decompose::effects::{call_local, expr_acts, held_error, Cx, Effects};
    let units = fill_shape(px.gx, k, inner, fillers)?;
    let gx = px.gx;
    let cx = gx.graph.stmt.map_or_else(|| Cx::without_stmt(gx.facts, px.arena), |s| Cx::new(s, gx.facts, px.arena));
    let acts = units.iter().any(|&n| expr_acts(&cx, n));
    held_error(Effects { acts, may_raise: true, same_rows: !acts }, false, call_local(&cx, units.iter().copied())).ok()
}

/// Whether full Sort `inner` (its one filler, `fillers`) may hold its fill's error for its one reader `k` (op 14 for
/// runs.sort): its fill builtin code over a scan, a quiet breaker or a constant, its feeders quiet, the run read by `k`
/// alone, neither in a rerun set; the fill's expressions (source keys and recheck, filters, projections, bounds) when it
/// may. Also a pull-form MergeAppend's Sort arm (frame/iface/sort.rs `held_fill`, D449).
pub(crate) fn fill_shape<'g, 'p>(gx: &crate::graph::Gx<'g, 'p>, k: usize, inner: usize, fillers: &[usize]) -> Option<PgVec<'g, types_nodes::Node<'p>>> {
    let ps = gx.pipelines();
    if k >= ps.len() || inner >= ps.len() || fillers != [inner] {
        return None;
    }
    let rerun = |x: usize| gx.graph.reruns.iter().any(|r| r.pipelines.contains(&(x as u16)));
    let p = &ps[inner];
    if rerun(k) || rerun(inner) || !crate::ops::mergejoin::lazy_outer::full(p) || crate::effects::visible::merge::user_sort(gx, p) {
        return None;
    }
    // the run's one reader: the merge's pipeline (no other graph pipeline, no embedded inner reads it)
    let slot = p.desc.produces()?;
    let life = LifeId(inner as u16);
    let reads = |c: usize| ps[c].desc.consumes.contains(&slot) && gx.life_of(c, slot) == Some(life);
    if (0..ps.len()).any(|c| c != k && c != inner && reads(c)) || !reads(k) || ps.inners().iter().any(|(_, q)| q.desc.consumes_from.contains(&life)) {
        return None;
    }
    let node = |n: types_nodes::Node<'_>| crate::effects::user_fn::held_node(gx, n);
    let source = match &p.source {
        Source::Range { .. } | Source::Breaker { .. } | Source::Const => true,
        Source::Index { keys, recheck, .. } => keys.iter().all(|x| crate::effects::expr_quiet(gx, x.value)) && recheck.iter().all(node),
        _ => false,
    };
    let bound = |b: &Bound<'_>| match b {
        Bound::Expr(n) => node(*n),
        _ => true,
    };
    let (last, body) = p.ops.split_last()?;
    let ops = matches!(last, Op::Sort(_))
        && body.iter().all(|op| match op {
            Op::Filter(n) | Op::Gate(n) => node(*n),
            Op::Project(l) => l.iter().all(node),
            op => crate::frame::iface::limit_bounds(op).is_some_and(|(count, offset)| bound(count) && bound(offset)),
        });
    // the fill's feeders (what its source and consumes read) run before it and are not held: quiet only
    let src = if let Source::Breaker { slot } = p.source { Some(slot) } else { None };
    let feeders = crate::ops::mergejoin::lazy_outer::lives(gx, inner, p.desc.consumes.iter().copied().chain(src));
    let quiet = feeders.iter().all(|&x| crate::effects::visible::merge::quiet(gx, x, usize::MAX));
    if !(source && ops && quiet) {
        return None;
    }
    let mut units: PgVec<'g, types_nodes::Node<'p>> = PgVec::new_in(gx.mcx);
    if let Source::Index { keys, recheck, .. } = &p.source {
        units.extend_in(keys.iter().map(|x| x.value).chain(recheck.iter()));
    }
    for op in body {
        match op {
            Op::Filter(n) | Op::Gate(n) => units.push_in(*n),
            Op::Project(l) => units.extend_in(l.iter()),
            op => {
                let expr = |b: &Bound<'p>| if let Bound::Expr(n) = b { Some(*n) } else { None };
                let bounds = crate::frame::iface::limit_bounds(op).map_or([None, None], |(count, offset)| [expr(count), expr(offset)]);
                units.extend_in(bounds.into_iter().flatten());
            }
        }
    }
    Some(units)
}

/// Merge `(k, j)`'s outer, one that may act or raise (lower.rs: D824's lazy group's `acting_outer` failed), reads as PG
/// reads it: a full Sort's run (ExecSort reads it whole at INITIALIZE_OUTER), or a scan in `k` itself, which the machine
/// ends where nodeMergejoin fetches no further outer tuple (lower.rs: the inner exhausted under INNER / SEMI); PG surely
/// runs the merge (lazy_outer.rs `reached`: a LIMIT, gate or SubPlan above may leave it unrun, where the outer's acts and
/// raises would run here alone). An Incremental Sort's run (PG reads one presorted group at a time, D2316), a Materialize
/// or CTE store and any other breaker are not this form.
pub(crate) fn outer_exact(gx: &crate::graph::Gx<'_, '_>, k: usize, j: usize) -> bool {
    let ps = gx.pipelines();
    let read = match ps[k].source {
        Source::Range { .. } | Source::Index { .. } | Source::Const => true,
        Source::Breaker { slot } => gx.life_of(k, slot).is_some_and(|o| crate::ops::mergejoin::lazy_outer::full(&ps[o.0 as usize])),
        _ => false,
    };
    read && crate::ops::mergejoin::lazy_outer::reached(gx, k, j + 1)
}

/// The merge's run reader over a held inner: `first_fetch` raises the held error of a fill that raised (`m11_run_held`,
/// consumed region `i`), after the reader's own first call (sort_noproc.rs's: PG's tuplesort_begin raises before the
/// Sort reads its input).
struct Held<'g> {
    run: PgBox<'g, dyn SlotRun + 'g>,
    i: u64,
    /// The held error's licence the fill runs ahead of the merge's first inner call under (`fill_holds`). Held, not read.
    #[allow(dead_code)]
    held: gravity_decompose::effects::Licence,
}

/// `run`, merge `k`'s reader of slot `slot`, as the held inner's, under `held` (`holds`).
pub(crate) fn wrap<'g>(px: &Px<'g, '_>, k: usize, slot: u8, run: PgBox<'g, dyn SlotRun + 'g>, held: gravity_decompose::effects::Licence) -> PgBox<'g, dyn SlotRun + 'g> {
    let Some(i) = px.consumed(k, slot) else { crate::refuse!(px.gx.pipelines()[k].node_id, crate::admit::refusal::Reason::Bug { payload: "a held merge inner its pipeline does not consume" }) };
    mcx::pg_box_dyn!(px.mcx(), Held { run, i: i as u64, held } => dyn SlotRun + 'g)
}

impl SlotRun for Held<'_> {
    fn open(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, x: &Exits) -> Value {
        self.run.open(b, env, x)
    }
    fn fetch(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, j: Value) -> Value {
        self.run.fetch(b, env, j)
    }
    fn vals<'e>(&self, b: &mut FunctionBuilder<'_>, env: &mut Env<'e, '_>, p: Value, carry: Carry<'e>, next: Value, at: Value) -> RowVals<'e> {
        self.run.vals(b, env, p, carry, next, at)
    }
    fn mark(&self, b: &mut FunctionBuilder<'_>, env: &mut Env, j: Value) {
        self.run.mark(b, env, j)
    }
    /// Op 14's raise at PG's first inner call: 1 from `m11_run_held` (the held error stashed again for this thread) leaves
    /// by the raise exit, as the hash join's probe raises a held build's (probe_wait.rs `raise_failed`).
    fn first_fetch(&self, b: &mut FunctionBuilder<'_>, env: &mut Env) {
        self.run.first_fetch(b, env);
        let flags = HostFlags { may_error: true, ..HostFlags::default() };
        let h = b.declare_host_once("m11_run_held", &[Type::Ptr, Type::I64], Some(Type::I64), gravity_abi::host_words!(m11_run_held(ctx, k)), flags);
        let i = b.iconst(Type::I64, self.i as i64);
        let st = b.call_host(h, &[env.ctx, i], Some(Type::I64)).expect("m11_run_held returns its answer");
        let (held, l_raise, l_on) = (b.icmp(IntCond::Ne, st, env.zero), b.create_block(&[]), b.create_block(&[]));
        b.cond_br(held, (l_raise, &[]), (l_on, &[]));
        b.switch_to(l_raise);
        b.mark_cold(l_raise);
        let e = b.iconst(Type::I64, raise::bit(raise::HOST) as i64);
        b.br(env.l_raise, &[e]);
        b.switch_to(l_on);
    }
}
